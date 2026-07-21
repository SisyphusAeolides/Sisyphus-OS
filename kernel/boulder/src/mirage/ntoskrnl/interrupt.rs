use core::ffi::c_void;

use sisyphus_driver_abi::Handle;

use crate::sync::SpinLock;

use super::abi::NtStatus;

const MAXIMUM_WINDOWS_INTERRUPTS: usize = 64;

pub type WindowsIsr =
    unsafe extern "win64" fn(interrupt_object: *mut c_void, service_context: *mut c_void) -> u8;

#[derive(Clone, Copy)]
struct InterruptSlot {
    generation: u32,
    interrupt_object: usize,
    service_context: usize,
    service_routine: Option<WindowsIsr>,
    vector: u32,
}

impl InterruptSlot {
    const EMPTY: Self = Self {
        generation: 0,
        interrupt_object: 0,
        service_context: 0,
        service_routine: None,
        vector: 0,
    };
}

struct RegistryState {
    slots: [InterruptSlot; MAXIMUM_WINDOWS_INTERRUPTS],
    next_generation: u32,
}

pub struct WindowsInterruptRegistry {
    state: SpinLock<RegistryState>,
}

impl WindowsInterruptRegistry {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(RegistryState {
                slots: [InterruptSlot::EMPTY; MAXIMUM_WINDOWS_INTERRUPTS],
                next_generation: 1,
            }),
        }
    }

    /// Registers opaque, version-owned interrupt state for deferred execution.
    ///
    /// # Safety
    ///
    /// Both pointers and the Win64 callback must remain valid until disconnect.
    pub unsafe fn connect(
        &self,
        vector: u32,
        interrupt_object: *mut c_void,
        service_routine: WindowsIsr,
        service_context: *mut c_void,
    ) -> Result<Handle, NtStatus> {
        if interrupt_object.is_null() || vector < 32 {
            return Err(-1);
        }
        let mut state = self.state.lock();
        let index = state
            .slots
            .iter()
            .position(|slot| slot.service_routine.is_none())
            .ok_or(-1)?;
        let generation = state.next_generation.max(1);
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        state.slots[index] = InterruptSlot {
            generation,
            interrupt_object: interrupt_object as usize,
            service_context: service_context as usize,
            service_routine: Some(service_routine),
            vector,
        };
        Ok((u64::from(generation) << 32) | (index as u64 + 1))
    }

    /// Executes a previously reflected ISR on its enclave CPU.
    ///
    /// # Safety
    ///
    /// The caller must enforce the selected personality's IRQL, affinity, and
    /// serialization rules around this callback.
    pub unsafe fn execute(&self, registration: Handle) -> Result<bool, NtStatus> {
        let slot = self.lookup(registration).ok_or(-1)?;
        let routine = slot.service_routine.ok_or(-1)?;
        Ok(unsafe {
            routine(
                slot.interrupt_object as *mut c_void,
                slot.service_context as *mut c_void,
            ) != 0
        })
    }

    pub fn disconnect(&self, registration: Handle) -> Result<(), NtStatus> {
        let (index, generation) = decode_handle(registration).ok_or(-1)?;
        let mut state = self.state.lock();
        let slot = state.slots.get_mut(index).ok_or(-1)?;
        if slot.generation != generation || slot.service_routine.is_none() {
            return Err(-1);
        }
        *slot = InterruptSlot::EMPTY;
        Ok(())
    }

    pub fn vector(&self, registration: Handle) -> Option<u32> {
        self.lookup(registration).map(|slot| slot.vector)
    }

    fn lookup(&self, registration: Handle) -> Option<InterruptSlot> {
        let (index, generation) = decode_handle(registration)?;
        self.state
            .lock()
            .slots
            .get(index)
            .copied()
            .filter(|slot| slot.generation == generation && slot.service_routine.is_some())
    }
}

impl Default for WindowsInterruptRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn decode_handle(handle: Handle) -> Option<(usize, u32)> {
    let index = (handle as u32).checked_sub(1)? as usize;
    let generation = (handle >> 32) as u32;
    (generation != 0 && index < MAXIMUM_WINDOWS_INTERRUPTS).then_some((index, generation))
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    unsafe extern "win64" fn isr(_interrupt_object: *mut c_void, context: *mut c_void) -> u8 {
        unsafe { &*context.cast::<AtomicUsize>() }.fetch_add(1, Ordering::Relaxed);
        1
    }

    #[test]
    fn executes_only_live_generation_checked_isr_handles() {
        let registry = WindowsInterruptRegistry::new();
        let counter = AtomicUsize::new(0);
        let mut object = 0_u8;
        let handle = unsafe {
            registry.connect(
                48,
                core::ptr::addr_of_mut!(object).cast(),
                isr,
                core::ptr::addr_of!(counter) as *mut c_void,
            )
        }
        .unwrap();
        assert_eq!(registry.vector(handle), Some(48));
        assert_eq!(unsafe { registry.execute(handle) }, Ok(true));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        registry.disconnect(handle).unwrap();
        assert_eq!(unsafe { registry.execute(handle) }, Err(-1));
    }
}
