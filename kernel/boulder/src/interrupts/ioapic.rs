use core::sync::atomic::{Ordering, compiler_fence};

use sisyphus_driver_abi::{Handle, Status};

use crate::boot::acpi::{
    InterruptPolarity, InterruptSourceOverride, InterruptTriggerMode, MAXIMUM_IO_APICS, MadtInfo,
};
use crate::shim::MmioService;
use crate::sync::SpinLock;

const IO_APIC_MAPPING_LENGTH: usize = 0x20;
const REGISTER_VERSION: u32 = 1;
const REDIRECTION_TABLE_BASE: u32 = 0x10;
const REDIRECTION_MASKED: u32 = 1 << 16;
const REDIRECTION_ACTIVE_LOW: u32 = 1 << 13;
const REDIRECTION_LEVEL_TRIGGERED: u32 = 1 << 15;
const LEGACY_IRQ_COUNT: usize = 16;
const LEGACY_VECTOR_BASE: u8 = 32;

#[derive(Clone, Copy)]
struct Controller {
    base: usize,
    mapping: Handle,
    global_interrupt_base: u32,
    redirection_entries: u32,
}

impl Controller {
    const EMPTY: Self = Self {
        base: 0,
        mapping: 0,
        global_interrupt_base: 0,
        redirection_entries: 0,
    };

    fn contains(self, global_interrupt: u32) -> bool {
        global_interrupt >= self.global_interrupt_base
            && global_interrupt - self.global_interrupt_base < self.redirection_entries
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LegacyRoute {
    global_interrupt: u32,
    active_low: bool,
    level_triggered: bool,
}

impl LegacyRoute {
    const EMPTY: Self = Self {
        global_interrupt: 0,
        active_low: false,
        level_triggered: false,
    };
}

struct IoApicState {
    controllers: [Controller; MAXIMUM_IO_APICS],
    controller_count: usize,
    routes: [LegacyRoute; LEGACY_IRQ_COUNT],
    initialized: bool,
}

impl IoApicState {
    const fn new() -> Self {
        Self {
            controllers: [Controller::EMPTY; MAXIMUM_IO_APICS],
            controller_count: 0,
            routes: [LegacyRoute::EMPTY; LEGACY_IRQ_COUNT],
            initialized: false,
        }
    }

    fn controller_for(&self, global_interrupt: u32) -> Option<Controller> {
        self.controllers[..self.controller_count]
            .iter()
            .copied()
            .find(|controller| controller.contains(global_interrupt))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicInfo {
    pub controller_count: usize,
    pub redirection_entries: u32,
    pub interrupt_source_overrides: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoApicError {
    AlreadyInitialized,
    MissingController,
    InvalidControllerAddress,
    MappingFailed(Status),
    UnmappedLegacyInterrupt(u8),
}

static IO_APICS: SpinLock<IoApicState> = SpinLock::new(IoApicState::new());

/// Initializes I/O APIC redirection for the 16 ISA interrupt sources.
///
/// # Safety
///
/// The caller must provide the active platform MADT and exclusive access to
/// the listed I/O APICs while interrupts are disabled.
pub unsafe fn initialize(
    madt: &MadtInfo,
    mmio: &dyn MmioService,
    destination_apic_id: u8,
) -> Result<IoApicInfo, IoApicError> {
    let mut state = IO_APICS.lock();
    if state.initialized {
        return Err(IoApicError::AlreadyInitialized);
    }
    if madt.io_apics().is_empty() {
        return Err(IoApicError::MissingController);
    }

    let mut total_redirection_entries = 0_u32;
    for descriptor in madt.io_apics() {
        if descriptor.address == 0 {
            unmap_controllers(&mut state, mmio);
            return Err(IoApicError::InvalidControllerAddress);
        }
        let mapping = match mmio.map(u64::from(descriptor.address), IO_APIC_MAPPING_LENGTH, 0) {
            Ok(mapping) => mapping,
            Err(status) => {
                unmap_controllers(&mut state, mmio);
                return Err(IoApicError::MappingFailed(status));
            }
        };
        let base = mapping.pointer.as_ptr() as usize;
        let version = unsafe { read_register(base, REGISTER_VERSION) };
        let redirection_entries = ((version >> 16) & 0xff) + 1;
        let controller = Controller {
            base,
            mapping: mapping.handle,
            global_interrupt_base: descriptor.global_system_interrupt_base,
            redirection_entries,
        };
        let controller_index = state.controller_count;
        state.controllers[controller_index] = controller;
        state.controller_count += 1;
        total_redirection_entries = total_redirection_entries.saturating_add(redirection_entries);
    }

    for irq in 0..LEGACY_IRQ_COUNT {
        state.routes[irq] = resolve_legacy_route(irq as u8, madt.interrupt_source_overrides());
        if state
            .controller_for(state.routes[irq].global_interrupt)
            .is_none()
        {
            unmap_controllers(&mut state, mmio);
            return Err(IoApicError::UnmappedLegacyInterrupt(irq as u8));
        }
    }

    for controller in state.controllers[..state.controller_count].iter().copied() {
        for index in 0..controller.redirection_entries {
            unsafe {
                write_redirection(
                    controller,
                    index,
                    REDIRECTION_MASKED,
                    u32::from(destination_apic_id) << 24,
                );
            }
        }
    }
    for irq in 0..LEGACY_IRQ_COUNT {
        if irq == 2 {
            continue;
        }
        let route = state.routes[irq];
        let controller = state
            .controller_for(route.global_interrupt)
            .ok_or(IoApicError::UnmappedLegacyInterrupt(irq as u8))?;
        let index = route.global_interrupt - controller.global_interrupt_base;
        let mut low = u32::from(LEGACY_VECTOR_BASE + irq as u8) | REDIRECTION_MASKED;
        if route.active_low {
            low |= REDIRECTION_ACTIVE_LOW;
        }
        if route.level_triggered {
            low |= REDIRECTION_LEVEL_TRIGGERED;
        }
        unsafe {
            write_redirection(controller, index, low, u32::from(destination_apic_id) << 24);
        }
    }

    state.initialized = true;
    Ok(IoApicInfo {
        controller_count: state.controller_count,
        redirection_entries: total_redirection_entries,
        interrupt_source_overrides: madt.interrupt_source_overrides().len(),
    })
}

pub fn is_initialized() -> bool {
    IO_APICS.lock().initialized
}

pub fn set_masked(irq: u8, masked: bool) -> bool {
    let state = IO_APICS.lock();
    if !state.initialized || irq as usize >= LEGACY_IRQ_COUNT {
        return false;
    }
    let route = state.routes[irq as usize];
    let Some(controller) = state.controller_for(route.global_interrupt) else {
        return false;
    };
    let index = route.global_interrupt - controller.global_interrupt_base;
    let register = REDIRECTION_TABLE_BASE + index * 2;
    let mut low = unsafe { read_register(controller.base, register) };
    if masked {
        low |= REDIRECTION_MASKED;
    } else {
        low &= !REDIRECTION_MASKED;
    }
    unsafe { write_register(controller.base, register, low) };
    true
}

fn resolve_legacy_route(irq: u8, overrides: &[InterruptSourceOverride]) -> LegacyRoute {
    let mut route = LegacyRoute {
        global_interrupt: u32::from(irq),
        active_low: false,
        level_triggered: false,
    };
    if let Some(source_override) = overrides
        .iter()
        .find(|entry| entry.bus == 0 && entry.source == irq)
    {
        route.global_interrupt = source_override.global_system_interrupt;
        route.active_low = source_override.polarity == InterruptPolarity::ActiveLow;
        route.level_triggered = source_override.trigger_mode == InterruptTriggerMode::Level;
    }
    route
}

fn unmap_controllers(state: &mut IoApicState, mmio: &dyn MmioService) {
    for controller in state.controllers[..state.controller_count].iter() {
        let _ = mmio.unmap(controller.mapping);
    }
    state.controller_count = 0;
    state.controllers = [Controller::EMPTY; MAXIMUM_IO_APICS];
}

unsafe fn read_register(base: usize, register: u32) -> u32 {
    unsafe { (base as *mut u32).write_volatile(register) };
    compiler_fence(Ordering::SeqCst);
    let value = unsafe { ((base + 0x10) as *const u32).read_volatile() };
    compiler_fence(Ordering::SeqCst);
    value
}

unsafe fn write_register(base: usize, register: u32, value: u32) {
    unsafe { (base as *mut u32).write_volatile(register) };
    compiler_fence(Ordering::SeqCst);
    unsafe { ((base + 0x10) as *mut u32).write_volatile(value) };
    compiler_fence(Ordering::SeqCst);
}

unsafe fn write_redirection(controller: Controller, index: u32, low: u32, high: u32) {
    let register = REDIRECTION_TABLE_BASE + index * 2;
    unsafe {
        write_register(controller.base, register + 1, high);
        write_register(controller.base, register, low);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_isa_defaults_and_source_overrides() {
        let source_override = InterruptSourceOverride {
            bus: 0,
            source: 9,
            global_system_interrupt: 20,
            polarity: InterruptPolarity::ActiveLow,
            trigger_mode: InterruptTriggerMode::Level,
        };

        assert_eq!(
            resolve_legacy_route(1, &[source_override]),
            LegacyRoute {
                global_interrupt: 1,
                active_low: false,
                level_triggered: false,
            }
        );
        assert_eq!(
            resolve_legacy_route(9, &[source_override]),
            LegacyRoute {
                global_interrupt: 20,
                active_low: true,
                level_triggered: true,
            }
        );
    }
}
