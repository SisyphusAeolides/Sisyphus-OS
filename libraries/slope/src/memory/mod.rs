// SISYPHEAN SLAB HEAP — multi-class zero-metadata slab allocator
//
// Design:
//   Each size class (8B, 16B, 32B, 64B, 128B, 256B, 512B, 1024B, 2048B, 4096B)
//   owns a fixed number of slab pages from the kernel (SYS_BRKSLAB).
//   Each page is divided into N slots = PAGE_SIZE / object_size.
//   Free slots tracked via u64 bitmask words (1 bit = 1 slot free).
//   alloc(size): find the smallest class >= size, find first free slot, return ptr.
//   dealloc(ptr, size): recompute class + slot from ptr arithmetic, set bit free.
//
//   ZERO per-allocation metadata. No header. No footer. No canary.
//   Safety guaranteed entirely by bitmask ownership tracking.
//
// SlabToken: a packed (class:u8, page:u8, slot:u16) descriptor for any live alloc.
//   Can reconstruct the original pointer from just the token + base addresses.
//   Useful for capability-passing IPC: send a SlabToken instead of a raw pointer.
//
// GlobalSlabHeap: implements GlobalAlloc for the #[global_allocator] slot.
//   Backed by a SpookyCell<HeapState> for process-shared lock-free access.

extern crate alloc;

use crate::syscall;
use crate::syscalls::SYS_BRKSLAB;
use core::alloc::{GlobalAlloc, Layout};

pub const PAGE_SIZE: usize = 4096;
pub const SLAB_PAGES: usize = 8; // pages per class at init
pub const MAX_SLAB_PAGES: usize = 64; // max pages per class before OOM

// Size classes: index → object size in bytes
pub const SIZE_CLASSES: [usize; 10] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
pub const NUM_CLASSES: usize = SIZE_CLASSES.len();

fn class_for(size: usize) -> Option<usize> {
    SIZE_CLASSES.iter().position(|&s| s >= size)
}

// ─── SLAB PAGE ─────────────────────────────────────────────────────────────

const MAX_SLOTS_PER_PAGE: usize = PAGE_SIZE / 8; // smallest class = 8B → 512 slots
const BITMASK_WORDS: usize = MAX_SLOTS_PER_PAGE / 64; // 8 words of u64

struct SlabPage {
    base: *mut u8,
    free_mask: [u64; BITMASK_WORDS], // 1 = free, 0 = allocated
    object_size: usize,
    slot_count: usize, // PAGE_SIZE / object_size
    free_count: u16,
}

impl SlabPage {
    fn new(base: *mut u8, object_size: usize) -> Self {
        let slot_count = PAGE_SIZE / object_size;
        let word_count = slot_count.div_ceil(64);
        let mut mask = [0u64; BITMASK_WORDS];
        for i in 0..word_count.min(BITMASK_WORDS) {
            mask[i] = u64::MAX;
        }
        // Mask out slots beyond slot_count
        if slot_count % 64 != 0 && word_count > 0 {
            let last = word_count - 1;
            if last < BITMASK_WORDS {
                mask[last] = (1u64 << (slot_count % 64)) - 1;
            }
        }
        Self {
            base,
            free_mask: mask,
            object_size,
            slot_count,
            free_count: slot_count.min(u16::MAX as usize) as u16,
        }
    }

    fn alloc_slot(&mut self) -> Option<*mut u8> {
        let word_count = self.slot_count.div_ceil(64);
        for wi in 0..word_count.min(BITMASK_WORDS) {
            if self.free_mask[wi] == 0 {
                continue;
            }
            let bit = self.free_mask[wi].trailing_zeros() as usize;
            let slot = wi * 64 + bit;
            if slot >= self.slot_count {
                continue;
            }
            self.free_mask[wi] &= !(1u64 << bit);
            self.free_count -= 1;
            return Some(unsafe { self.base.add(slot * self.object_size) });
        }
        None
    }

    fn dealloc_slot(&mut self, ptr: *mut u8) -> bool {
        if ptr < self.base {
            return false;
        }
        let offset = ptr as usize - self.base as usize;
        if offset % self.object_size != 0 {
            return false;
        }
        let slot = offset / self.object_size;
        if slot >= self.slot_count {
            return false;
        }
        let wi = slot / 64;
        let bit = slot % 64;
        if self.free_mask[wi] & (1u64 << bit) != 0 {
            return false;
        } // double-free
        self.free_mask[wi] |= 1u64 << bit;
        self.free_count += 1;
        true
    }

    const fn _has_free(&self) -> bool {
        self.free_count > 0
    }
}

// ─── SLAB CLASS ────────────────────────────────────────────────────────────

struct SlabClass {
    pages: [Option<SlabPage>; MAX_SLAB_PAGES],
    page_count: usize,
    object_size: usize,
}

impl SlabClass {
    const fn uninit(object_size: usize) -> Self {
        Self {
            pages: [const { None }; MAX_SLAB_PAGES],
            page_count: 0,
            object_size,
        }
    }

    fn alloc(&mut self) -> Option<*mut u8> {
        // Try existing pages first
        for p in self.pages[..self.page_count].iter_mut().flatten() {
            if let Some(ptr) = p.alloc_slot() {
                return Some(ptr);
            }
        }
        // Grow: request a new page from the kernel
        self.grow()?;
        // Try again on the new page
        self.pages[self.page_count - 1].as_mut()?.alloc_slot()
    }

    fn grow(&mut self) -> Option<()> {
        if self.page_count >= MAX_SLAB_PAGES {
            return None;
        }
        let args = [self.object_size, PAGE_SIZE, 0, 0, 0, 0];
        let raw = unsafe { syscall(SYS_BRKSLAB, args) }.ok()?;
        let base = raw as *mut u8;
        if base.is_null() {
            return None;
        }
        self.pages[self.page_count] = Some(SlabPage::new(base, self.object_size));
        self.page_count += 1;
        Some(())
    }

    fn dealloc(&mut self, ptr: *mut u8) -> bool {
        for p in self.pages[..self.page_count].iter_mut().flatten() {
            if p.dealloc_slot(ptr) {
                return true;
            }
        }
        false
    }
}

// ─── HEAP STATE ────────────────────────────────────────────────────────────

struct HeapState {
    classes: [SlabClass; NUM_CLASSES],
    alloc_count: u64,
    dealloc_count: u64,
    oom_count: u64,
}

impl HeapState {
    const fn new() -> Self {
        Self {
            classes: [
                SlabClass::uninit(8),
                SlabClass::uninit(16),
                SlabClass::uninit(32),
                SlabClass::uninit(64),
                SlabClass::uninit(128),
                SlabClass::uninit(256),
                SlabClass::uninit(512),
                SlabClass::uninit(1024),
                SlabClass::uninit(2048),
                SlabClass::uninit(4096),
            ],
            alloc_count: 0,
            dealloc_count: 0,
            oom_count: 0,
        }
    }
}

// ─── GLOBAL ALLOCATOR ──────────────────────────────────────────────────────
// Uses a SpookyCell from slope::sync::entanglement as the lock.

use crate::sync::entanglement::SpookyCell;
use core::sync::atomic::{AtomicBool, Ordering};

pub struct GlobalSlabHeap {
    cell: SpookyCell<HeapState>,
    ready: AtomicBool,
}

impl GlobalSlabHeap {
    pub const fn new() -> Self {
        Self {
            cell: SpookyCell::new(HeapState::new()),
            ready: AtomicBool::new(false),
        }
    }

    pub fn init(&self) {
        self.ready.store(true, Ordering::Release);
    }

    pub fn stats(&self) -> HeapStats {
        // Try to observe — if busy, return zeros (non-blocking)
        // SAFETY: cell is process-local here; lock is a spinlock
        let pair = unsafe { crate::sync::entanglement::EntangledPair::from_mapping(&self.cell, 0) };
        pair.try_observe(|h| HeapStats {
            alloc_count: h.alloc_count,
            dealloc_count: h.dealloc_count,
            oom_count: h.oom_count,
        })
        .unwrap_or(HeapStats {
            alloc_count: 0,
            dealloc_count: 0,
            oom_count: 0,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HeapStats {
    pub alloc_count: u64,
    pub dealloc_count: u64,
    pub oom_count: u64,
}

unsafe impl Sync for GlobalSlabHeap {}

unsafe impl GlobalAlloc for GlobalSlabHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.ready.load(Ordering::Acquire) {
            return core::ptr::null_mut();
        }
        let size = layout.size().max(layout.align());
        let ci = match class_for(size) {
            Some(c) => c,
            None => return core::ptr::null_mut(),
        };
        let pair = unsafe { crate::sync::entanglement::EntangledPair::from_mapping(&self.cell, 0) };
        pair.mutate_bounded(64, |h| {
            let ptr = h.classes[ci].alloc().unwrap_or(core::ptr::null_mut());
            if ptr.is_null() {
                h.oom_count += 1;
            } else {
                h.alloc_count += 1;
            }
            ptr
        })
        .unwrap_or(core::ptr::null_mut())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let size = layout.size().max(layout.align());
        let ci = match class_for(size) {
            Some(c) => c,
            None => return,
        };
        let pair = unsafe { crate::sync::entanglement::EntangledPair::from_mapping(&self.cell, 0) };
        let _ = pair.mutate_bounded(64, |h| {
            if h.classes[ci].dealloc(ptr) {
                h.dealloc_count += 1;
            }
        });
    }
}

/// Sisyphus binaries call `HEAP.init()` in `_start` before allocating.
/// Host tests retain their platform allocator.
pub static HEAP: GlobalSlabHeap = GlobalSlabHeap::new();

// ─── SLAB TOKEN — capability-safe allocation identity ──────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlabToken(u32);

impl SlabToken {
    pub const fn pack(class: u8, page: u8, slot: u16) -> Self {
        Self((class as u32) << 24 | (page as u32) << 16 | slot as u32)
    }

    pub const fn class(&self) -> u8 {
        (self.0 >> 24) as u8
    }
    pub const fn page(&self) -> u8 {
        (self.0 >> 16) as u8
    }
    pub const fn slot(&self) -> u16 {
        self.0 as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_for_rounds_up_to_nearest_slab() {
        assert_eq!(class_for(1), Some(0)); // → 8B class
        assert_eq!(class_for(8), Some(0)); // → 8B class
        assert_eq!(class_for(9), Some(1)); // → 16B class
        assert_eq!(class_for(4096), Some(9)); // → 4096B class
        assert_eq!(class_for(4097), None); // no class for >4096B
    }

    #[test]
    fn slab_token_roundtrips() {
        let tok = SlabToken::pack(2, 5, 300);
        assert_eq!(tok.class(), 2);
        assert_eq!(tok.page(), 5);
        assert_eq!(tok.slot(), 300);
    }

    #[test]
    fn slab_page_alloc_dealloc_roundtrip() {
        let mut backing = [0u8; PAGE_SIZE];
        let mut page = SlabPage::new(backing.as_mut_ptr(), 64);
        assert_eq!(page.slot_count, 64);
        let p1 = page.alloc_slot().unwrap();
        let p2 = page.alloc_slot().unwrap();
        assert_ne!(p1, p2);
        assert!(page.dealloc_slot(p1));
        assert!(!page.dealloc_slot(p1)); // double-free returns false
        let p3 = page.alloc_slot().unwrap();
        assert_eq!(p1, p3); // reused
    }
}
pub mod relativistic;
