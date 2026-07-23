// libraries/driver-abi/src/lazarus.rs
// #![no_std] inherited
//
// LAZARUS — Transactional Self-Healing ABI Membrane
//
// Every KernelApi call through Lazarus is wrapped in a transaction:
//  1. Pre-call: snapshot current driver state (handles, mappings, IRQs)
//     into a shadow journal entry
//  2. Execute: call the actual KernelApi function
//  3. Post-call:
//     a. Success: commit journal entry (advance watermark)
//     b. Failure (STATUS_*_ERROR): rollback from journal, re-arm Lazarus
//     c. Timeout: kill driver context, rollback, re-probe (resurrection)
//
// Shadow journal: append-only log of (call_type, handle, state_before)
//   On rollback: replay log in reverse, calling the inverse of each op:
//     mmio_map was called? → call mmio_unmap with saved handle
//     dma_alloc was called? → call dma_free with saved handle
//     irq_register was called? → call irq_unregister
//
// Resurrection protocol:
//  1. Detect driver death (fault vector, watchdog timeout, or STATUS_IO_ERROR)
//  2. Rollback all journal entries to last checkpoint
//  3. Re-call driver's probe() with same DeviceInfo
//  4. Increment resurrection count
//  5. If resurrection_count > MAX_RESURRECTIONS: quarantine driver (no more tries)
//
// Checkpoint: committed journal state. Driver can advance its own checkpoint
//   by calling lazarus_checkpoint(handle) after a successful init phase.
//
// Memory: journal stored in a fixed-size arena (no heap needed for rollback path)

#![allow(dead_code)]
extern crate alloc;
use super::{DeviceInfo, Handle, INVALID_HANDLE, KernelApi, STATUS_IO_ERROR, STATUS_OK, Status};
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_JOURNAL_ENTRIES: usize = 512;
pub const MAX_RESURRECTIONS: u32 = 3;
pub const WATCHDOG_TICK_LIMIT: u64 = 10_000_000; // 10M ticks ≈ ~3ms @ 3GHz
pub const MAX_LAZARUS_INSTANCES: usize = 64;

// ─────────────────────────────────────────────
// JOURNAL ENTRY — records one KernelApi call for rollback
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum JournalOp {
    MmioMap = 1,
    DmaAlloc = 2,
    IrqRegister = 3,
    MemAlloc = 4,
    Checkpoint = 0xFF,
}

#[derive(Clone, Copy)]
pub struct JournalEntry {
    pub op: JournalOp,
    pub handle: Handle,   // handle returned by the kernel call
    pub aux_addr: u64,    // for alloc: virtual address
    pub aux_size: usize,  // for alloc: size
    pub aux_align: usize, // for alloc: alignment
    pub committed: bool,  // past a checkpoint
    pub rolled_back: bool,
}

impl JournalEntry {
    pub const fn empty() -> Self {
        Self {
            op: JournalOp::Checkpoint,
            handle: INVALID_HANDLE,
            aux_addr: 0,
            aux_size: 0,
            aux_align: 0,
            committed: false,
            rolled_back: false,
        }
    }
}

// ─────────────────────────────────────────────
// SHADOW JOURNAL
// ─────────────────────────────────────────────

pub struct ShadowJournal {
    pub entries: [JournalEntry; MAX_JOURNAL_ENTRIES],
    pub head: usize,       // next write position
    pub checkpoint: usize, // last committed checkpoint
    pub total_entries: AtomicU64,
    pub total_rollbacks: AtomicU32,
}

impl ShadowJournal {
    pub const fn new() -> Self {
        Self {
            entries: [JournalEntry::empty(); MAX_JOURNAL_ENTRIES],
            head: 0,
            checkpoint: 0,
            total_entries: AtomicU64::new(0),
            total_rollbacks: AtomicU32::new(0),
        }
    }

    pub fn push(&mut self, entry: JournalEntry) -> bool {
        if self.head >= MAX_JOURNAL_ENTRIES {
            return false;
        }
        self.entries[self.head] = entry;
        self.head += 1;
        self.total_entries.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn set_checkpoint(&mut self) {
        // Mark all current entries as committed
        for e in &mut self.entries[..self.head] {
            e.committed = true;
        }
        self.checkpoint = self.head;
    }

    /// Rollback: undo all uncommitted entries in reverse order
    /// Calls the provided inverse-op callback for each entry
    pub fn rollback<F>(&mut self, mut inverse_fn: F)
    where
        F: FnMut(&JournalEntry),
    {
        if self.head == 0 {
            return;
        }
        // Replay in reverse from head to checkpoint
        let mut i = self.head;
        while i > self.checkpoint {
            i -= 1;
            let entry = &self.entries[i];
            if !entry.committed && !entry.rolled_back {
                inverse_fn(entry);
                self.entries[i].rolled_back = true;
            }
        }
        self.head = self.checkpoint; // trim journal to checkpoint
        self.total_rollbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reset(&mut self) {
        for e in &mut self.entries {
            *e = JournalEntry::empty();
        }
        self.head = 0;
        self.checkpoint = 0;
    }
}

// ─────────────────────────────────────────────
// DRIVER VITAL SIGNS — health monitoring
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverHealth {
    Healthy,
    Degraded,     // had errors but still running
    FlatLined,    // crashed / timed out
    Quarantined,  // exceeded MAX_RESURRECTIONS
    Resurrecting, // rollback in progress
}

// ─────────────────────────────────────────────
// LAZARUS MEMBRANE — per-driver wrapper
// ─────────────────────────────────────────────

pub struct LazarusMembrane {
    pub driver_handle: u64,
    pub journal: ShadowJournal,
    pub health: DriverHealth,
    pub resurrection_count: u32,
    pub total_calls: AtomicU64,
    pub total_errors: AtomicU32,
    pub total_resurrections: AtomicU32,
    pub watchdog_start: u64,
    pub watchdog_limit: u64,
    pub last_good_tick: u64,
    // Shadow of driver's registered resources (for resurrection replay)
    pub last_device_info: Option<DeviceInfo>,
}

impl LazarusMembrane {
    pub fn new(driver_handle: u64) -> Self {
        Self {
            driver_handle,
            journal: ShadowJournal::new(),
            health: DriverHealth::Healthy,
            resurrection_count: 0,
            total_calls: AtomicU64::new(0),
            total_errors: AtomicU32::new(0),
            total_resurrections: AtomicU32::new(0),
            watchdog_start: 0,
            watchdog_limit: WATCHDOG_TICK_LIMIT,
            last_good_tick: 0,
            last_device_info: None,
        }
    }

    // ─── TRANSACTIONAL WRAPPERS ───

    /// Wrapped mmio_map: journals the mapping handle for rollback
    pub unsafe fn mmio_map(
        &mut self,
        api: &KernelApi,
        phys_addr: u64,
        length: usize,
        flags: u64,
    ) -> (Status, Handle, *mut u8) {
        self.total_calls.fetch_add(1, Ordering::Relaxed);
        let mut out_handle: Handle = INVALID_HANDLE;
        let mut out_ptr: *mut u8 = core::ptr::null_mut();

        let status = match api.mmio_map {
            Some(f) => unsafe {
                f(
                    api.kernel_context,
                    phys_addr,
                    length,
                    flags,
                    &mut out_handle,
                    &mut out_ptr,
                )
            },
            None => STATUS_IO_ERROR,
        };

        if status == STATUS_OK && out_handle != INVALID_HANDLE {
            self.journal.push(JournalEntry {
                op: JournalOp::MmioMap,
                handle: out_handle,
                aux_addr: phys_addr,
                aux_size: length,
                aux_align: 0,
                committed: false,
                rolled_back: false,
            });
        } else if status != STATUS_OK {
            self.total_errors.fetch_add(1, Ordering::Relaxed);
            self.health = DriverHealth::Degraded;
        }
        (status, out_handle, out_ptr)
    }

    /// Wrapped dma_alloc: journals the DMA handle for rollback
    pub unsafe fn dma_alloc(
        &mut self,
        api: &KernelApi,
        size: usize,
        alignment: usize,
        flags: u64,
    ) -> (Status, Handle, *mut c_void, u64) {
        self.total_calls.fetch_add(1, Ordering::Relaxed);
        let mut out_handle: Handle = INVALID_HANDLE;
        let mut out_cpu: *mut c_void = core::ptr::null_mut();
        let mut out_dev_addr: u64 = 0;

        let status = match api.dma_alloc {
            Some(f) => unsafe {
                f(
                    api.kernel_context,
                    size,
                    alignment,
                    flags,
                    &mut out_handle,
                    &mut out_cpu,
                    &mut out_dev_addr,
                )
            },
            None => STATUS_IO_ERROR,
        };

        if status == STATUS_OK && out_handle != INVALID_HANDLE {
            self.journal.push(JournalEntry {
                op: JournalOp::DmaAlloc,
                handle: out_handle,
                aux_addr: out_dev_addr,
                aux_size: size,
                aux_align: alignment,
                committed: false,
                rolled_back: false,
            });
        } else if status != STATUS_OK {
            self.total_errors.fetch_add(1, Ordering::Relaxed);
        }
        (status, out_handle, out_cpu, out_dev_addr)
    }

    /// Wrapped irq_register
    pub unsafe fn irq_register(
        &mut self,
        api: &KernelApi,
        irq: u32,
        flags: u64,
        handler: super::IrqHandler,
        driver_ctx: *mut c_void,
    ) -> (Status, Handle) {
        self.total_calls.fetch_add(1, Ordering::Relaxed);
        let mut out_handle: Handle = INVALID_HANDLE;

        let status = match api.irq_register {
            Some(f) => unsafe {
                f(
                    api.kernel_context,
                    irq,
                    flags,
                    Some(handler),
                    driver_ctx,
                    &mut out_handle,
                )
            },
            None => STATUS_IO_ERROR,
        };

        if status == STATUS_OK {
            self.journal.push(JournalEntry {
                op: JournalOp::IrqRegister,
                handle: out_handle,
                aux_addr: irq as u64,
                aux_size: 0,
                aux_align: 0,
                committed: false,
                rolled_back: false,
            });
        }
        (status, out_handle)
    }

    /// Advance checkpoint — driver signals it has successfully initialized a phase
    pub fn checkpoint(&mut self) {
        self.journal.set_checkpoint();
        self.health = DriverHealth::Healthy;
    }

    /// ROLLBACK: undo all uncommitted state — called on driver crash
    pub unsafe fn rollback(&mut self, api: &KernelApi) {
        self.health = DriverHealth::Resurrecting;
        let kernel_ctx = api.kernel_context;

        self.journal.rollback(|entry| match entry.op {
            JournalOp::MmioMap => {
                if let Some(f) = api.mmio_unmap {
                    unsafe {
                        let _ = f(kernel_ctx, entry.handle);
                    }
                }
            }
            JournalOp::DmaAlloc => {
                if let Some(f) = api.dma_free {
                    unsafe {
                        let _ = f(kernel_ctx, entry.handle);
                    }
                }
            }
            JournalOp::IrqRegister => {
                if let Some(f) = api.irq_unregister {
                    unsafe {
                        let _ = f(kernel_ctx, entry.handle);
                    }
                }
            }
            JournalOp::MemAlloc => {
                if let Some(f) = api.dealloc {
                    unsafe {
                        let _ = f(
                            kernel_ctx,
                            entry.aux_addr as *mut c_void,
                            entry.aux_size,
                            entry.aux_align,
                        );
                    }
                }
            }
            _ => {}
        });

        self.total_resurrections.fetch_add(1, Ordering::Relaxed);
        self.resurrection_count += 1;
    }

    /// RESURRECT: rollback + re-probe
    /// Returns true if resurrection was attempted, false if quarantined
    pub unsafe fn resurrect(
        &mut self,
        api: &KernelApi,
        probe_fn: super::ProbeFn,
        driver_ctx: *mut c_void,
    ) -> ResurrectionResult {
        if self.resurrection_count >= MAX_RESURRECTIONS {
            self.health = DriverHealth::Quarantined;
            return ResurrectionResult::Quarantined {
                count: self.resurrection_count,
            };
        }

        // Step 1: rollback all uncommitted state
        unsafe {
            self.rollback(api);
        }

        // Step 2: re-call probe with last known good DeviceInfo
        let device_info = match &self.last_device_info {
            Some(d) => *d,
            None => return ResurrectionResult::NoDeviceInfo,
        };

        let mut out_instance: *mut c_void = core::ptr::null_mut();
        let status = unsafe { probe_fn(driver_ctx, api, &device_info, &mut out_instance) };

        if status == STATUS_OK {
            self.health = DriverHealth::Healthy;
            self.journal.reset();
            ResurrectionResult::Revived {
                resurrection_number: self.resurrection_count,
                new_instance: out_instance,
            }
        } else {
            self.health = DriverHealth::FlatLined;
            ResurrectionResult::Failed { status }
        }
    }

    /// Watchdog tick: call periodically with current TSC
    /// Returns true if driver has timed out
    pub fn watchdog_tick(&mut self, current_tsc: u64) -> bool {
        if self.watchdog_start > 0
            && current_tsc.saturating_sub(self.watchdog_start) > self.watchdog_limit
        {
            self.health = DriverHealth::FlatLined;
            return true;
        }
        false
    }

    pub fn arm_watchdog(&mut self, current_tsc: u64) {
        self.watchdog_start = current_tsc;
    }

    pub fn disarm_watchdog(&mut self) {
        self.watchdog_start = 0;
    }

    pub fn stats(&self) -> LazarusStats {
        LazarusStats {
            driver_handle: self.driver_handle,
            health: self.health,
            resurrections: self.resurrection_count,
            journal_entries: self.journal.head as u32,
            journal_checkpoint: self.journal.checkpoint as u32,
            total_calls: self.total_calls.load(Ordering::Relaxed),
            total_errors: self.total_errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ResurrectionResult {
    Revived {
        resurrection_number: u32,
        new_instance: *mut c_void,
    },
    Quarantined {
        count: u32,
    },
    Failed {
        status: Status,
    },
    NoDeviceInfo,
}

#[derive(Clone, Copy, Debug)]
pub struct LazarusStats {
    pub driver_handle: u64,
    pub health: DriverHealth,
    pub resurrections: u32,
    pub journal_entries: u32,
    pub journal_checkpoint: u32,
    pub total_calls: u64,
    pub total_errors: u32,
}

// ─────────────────────────────────────────────
// LAZARUS POOL — manages membranes for all loaded drivers
// ─────────────────────────────────────────────

pub struct LazarusPool {
    pub membranes: Vec<LazarusMembrane>,
    pub total_alive: AtomicU32,
    pub total_dead: AtomicU32,
    pub total_revived: AtomicU32,
}

impl LazarusPool {
    pub fn new() -> Self {
        Self {
            membranes: Vec::new(),
            total_alive: AtomicU32::new(0),
            total_dead: AtomicU32::new(0),
            total_revived: AtomicU32::new(0),
        }
    }

    pub fn register(&mut self, driver_handle: u64) {
        if self.membranes.len() < MAX_LAZARUS_INSTANCES {
            self.membranes.push(LazarusMembrane::new(driver_handle));
            self.total_alive.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn get_mut(&mut self, driver_handle: u64) -> Option<&mut LazarusMembrane> {
        self.membranes
            .iter_mut()
            .find(|m| m.driver_handle == driver_handle)
    }

    pub fn tick_watchdogs(&mut self, current_tsc: u64) -> Vec<u64> {
        // Returns handles of timed-out drivers
        let mut timeouts = Vec::new();
        for m in &mut self.membranes {
            if m.watchdog_tick(current_tsc) {
                timeouts.push(m.driver_handle);
            }
        }
        timeouts
    }
}
