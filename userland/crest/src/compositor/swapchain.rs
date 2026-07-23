use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

/// An Event Horizon represents a framebuffer in the Kerr black hole metaphor.
pub struct EventHorizon {
    pub data: Vec<u32>,
    pub width: usize,
    pub height: usize,
}

impl EventHorizon {
    pub fn new(width: usize, height: usize) -> Self {
        let size = width * height;
        let mut data = Vec::with_capacity(size);
        data.resize(size, 0);
        Self {
            data,
            width,
            height,
        }
    }
}

/// A double-buffered atomic presentation swapchain.
pub struct Swapchain {
    /// The currently presented Event Horizon (front buffer).
    presented: AtomicPtr<EventHorizon>,
    /// The backbuffer Event Horizon.
    backbuffer: AtomicPtr<EventHorizon>,
}

// Ensure Swapchain is Send and Sync since we use AtomicPtr.
unsafe impl Send for Swapchain {}
unsafe impl Sync for Swapchain {}

impl Swapchain {
    pub fn new(width: usize, height: usize) -> Self {
        let front = Box::new(EventHorizon::new(width, height));
        let back = Box::new(EventHorizon::new(width, height));
        Self {
            presented: AtomicPtr::new(Box::into_raw(front)),
            backbuffer: AtomicPtr::new(Box::into_raw(back)),
        }
    }

    /// Quantum Teleportation: swaps the front and back buffers atomically.
    /// The old front buffer is then "spaghettified" (zeroed out asynchronously).
    pub async fn flip_and_spaghettify(&self) {
        // Quantum teleportation: atomically swap pointers
        let back_ptr = self.backbuffer.load(Ordering::Acquire);
        let old_front = self.presented.swap(back_ptr, Ordering::SeqCst);
        self.backbuffer.store(old_front, Ordering::Release);

        // Spaghettification: zero out the old buffer asynchronously
        spaghettify(old_front).await;
    }

    /// Get a reference to the backbuffer for drawing.
    /// Safety: Caller must ensure this isn't called concurrently with `flip_and_spaghettify`.
    pub unsafe fn backbuffer(&self) -> &mut EventHorizon {
        unsafe { &mut *self.backbuffer.load(Ordering::Acquire) }
    }

    /// Get a reference to the currently presented buffer.
    pub fn presented(&self) -> &EventHorizon {
        unsafe { &*self.presented.load(Ordering::Acquire) }
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        let front = self.presented.swap(ptr::null_mut(), Ordering::SeqCst);
        let back = self.backbuffer.swap(ptr::null_mut(), Ordering::SeqCst);
        if !front.is_null() {
            unsafe {
                drop(Box::from_raw(front));
            }
        }
        if !back.is_null() {
            unsafe {
                drop(Box::from_raw(back));
            }
        }
    }
}

/// Spaghettify (zero out) the event horizon.
async fn spaghettify(eh_ptr: *mut EventHorizon) {
    if eh_ptr.is_null() {
        return;
    }
    // Safety: we have exclusive access to this backbuffer after swap
    let eh = unsafe { &mut *eh_ptr };
    for pixel in eh.data.iter_mut() {
        *pixel = 0; // spaghettify
    }
}
