use core::ffi::c_void;

use sisyphus_driver_abi::{
    Handle, IrqHandler, STATUS_BUSY, STATUS_INVALID_ARGUMENT, STATUS_NOT_FOUND, STATUS_OK,
    STATUS_UNSUPPORTED, Status,
};

use crate::shim::IrqService;
use crate::sync::SpinLock;

use super::set_irq_masked;

const IRQ_LINES: usize = 16;

#[derive(Clone, Copy)]
struct IrqSlot {
    generation: u32,
    handler: Option<IrqHandler>,
    context: usize,
    enabled: bool,
}

impl IrqSlot {
    const EMPTY: Self = Self {
        generation: 0,
        handler: None,
        context: 0,
        enabled: false,
    };
}

struct Registry {
    slots: [IrqSlot; IRQ_LINES],
    next_generation: u32,
}

impl Registry {
    const fn new() -> Self {
        Self {
            slots: [IrqSlot::EMPTY; IRQ_LINES],
            next_generation: 1,
        }
    }
}

pub struct KernelIrq {
    registry: SpinLock<Registry>,
}

impl KernelIrq {
    const fn new() -> Self {
        Self {
            registry: SpinLock::new(Registry::new()),
        }
    }

    pub fn dispatch(&self, irq: u8) {
        let slot = {
            let registry = self.registry.lock();
            registry.slots.get(irq as usize).copied()
        };
        let Some(slot) = slot else {
            return;
        };
        if !slot.enabled {
            return;
        }
        if let Some(handler) = slot.handler {
            unsafe { handler(slot.context as *mut c_void) };
        }
    }

    fn decode_handle(handle: Handle) -> Option<(usize, u32)> {
        let line_number = (handle & 0xffff_ffff) as usize;
        let generation = (handle >> 32) as u32;
        if line_number == 0 || line_number > IRQ_LINES || generation == 0 {
            None
        } else {
            Some((line_number - 1, generation))
        }
    }
}

impl IrqService for KernelIrq {
    fn register(
        &self,
        irq: u32,
        flags: u64,
        handler: IrqHandler,
        driver_context: *mut c_void,
    ) -> Result<Handle, Status> {
        if irq as usize >= IRQ_LINES {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        if irq == 2 {
            return Err(STATUS_UNSUPPORTED);
        }
        if flags != 0 {
            return Err(STATUS_UNSUPPORTED);
        }
        let mut registry = self.registry.lock();
        if registry.slots[irq as usize].handler.is_some() {
            return Err(STATUS_BUSY);
        }
        let generation = registry.next_generation.max(1);
        registry.next_generation = registry.next_generation.wrapping_add(1).max(1);
        registry.slots[irq as usize] = IrqSlot {
            generation,
            handler: Some(handler),
            context: driver_context as usize,
            enabled: false,
        };
        Ok((u64::from(generation) << 32) | (u64::from(irq) + 1))
    }

    fn set_enabled(&self, registration: Handle, enabled: bool) -> Status {
        let Some((irq, generation)) = Self::decode_handle(registration) else {
            return STATUS_NOT_FOUND;
        };
        let mut registry = self.registry.lock();
        let slot = &mut registry.slots[irq];
        if slot.handler.is_none() || slot.generation != generation {
            return STATUS_NOT_FOUND;
        }
        slot.enabled = enabled;
        set_irq_masked(irq as u8, !enabled);
        STATUS_OK
    }

    fn unregister(&self, registration: Handle) -> Status {
        let Some((irq, generation)) = Self::decode_handle(registration) else {
            return STATUS_NOT_FOUND;
        };
        let mut registry = self.registry.lock();
        let slot = &mut registry.slots[irq];
        if slot.handler.is_none() || slot.generation != generation {
            return STATUS_NOT_FOUND;
        }
        set_irq_masked(irq as u8, true);
        *slot = IrqSlot::EMPTY;
        STATUS_OK
    }
}

static KERNEL_IRQ: KernelIrq = KernelIrq::new();

pub fn kernel_irq() -> &'static KernelIrq {
    &KERNEL_IRQ
}
