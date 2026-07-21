use core::sync::atomic::{Ordering, compiler_fence};

use sisyphus_driver_abi::{STATUS_BUSY, STATUS_IO_ERROR, STATUS_OK, STATUS_UNSUPPORTED, Status};

use crate::arch::x86_64::{read_msr, write_msr};
use crate::shim::MmioService;
use crate::sync::SpinLock;

const IA32_APIC_BASE: u32 = 0x1b;
const APIC_GLOBAL_ENABLE: u64 = 1 << 11;
const X2APIC_ENABLE: u64 = 1 << 10;
const APIC_BASE_MASK: u64 = 0x000f_ffff_ffff_f000;
const REGISTER_ID: usize = 0x20;
const REGISTER_VERSION: usize = 0x30;
const REGISTER_EOI: usize = 0xb0;
const REGISTER_SPURIOUS: usize = 0xf0;
const REGISTER_ICR_LOW: usize = 0x300;
const SOFTWARE_ENABLE: u32 = 1 << 8;
const DELIVERY_PENDING: u32 = 1 << 12;
const DESTINATION_SELF: u32 = 1 << 18;
const SPURIOUS_VECTOR: u8 = 0xff;
const IPI_TIMEOUT: usize = 1_000_000;

#[derive(Clone, Copy)]
struct LocalApicState {
    base: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalApicInfo {
    pub id: u8,
    pub version: u8,
    pub physical_address: u64,
}

static LOCAL_APIC: SpinLock<Option<LocalApicState>> = SpinLock::new(None);

/// Enables and maps the local xAPIC through Boulder's uncached MMIO service.
///
/// # Safety
///
/// The caller must have exclusive control of the local APIC MSR and must keep
/// the supplied MMIO service alive for the remainder of kernel execution.
pub unsafe fn initialize(mmio: &dyn MmioService) -> Result<LocalApicInfo, Status> {
    let mut state = LOCAL_APIC.lock();
    if state.is_some() {
        return Err(STATUS_BUSY);
    }
    let features = core::arch::x86_64::__cpuid(1);
    if features.edx & (1 << 9) == 0 {
        return Err(STATUS_UNSUPPORTED);
    }

    let mut apic_base = unsafe { read_msr(IA32_APIC_BASE) };
    if apic_base & X2APIC_ENABLE != 0 {
        return Err(STATUS_UNSUPPORTED);
    }
    if apic_base & APIC_GLOBAL_ENABLE == 0 {
        apic_base |= APIC_GLOBAL_ENABLE;
        unsafe { write_msr(IA32_APIC_BASE, apic_base) };
    }
    let physical_address = apic_base & APIC_BASE_MASK;
    let mapping = mmio.map(physical_address, 4096, 0)?;
    let base = mapping.pointer.as_ptr() as usize;

    let id = (unsafe { read_register(base, REGISTER_ID) } >> 24) as u8;
    let version = unsafe { read_register(base, REGISTER_VERSION) } as u8;
    let spurious = unsafe { read_register(base, REGISTER_SPURIOUS) };
    unsafe {
        write_register(
            base,
            REGISTER_SPURIOUS,
            spurious | SOFTWARE_ENABLE | u32::from(SPURIOUS_VECTOR),
        );
    }
    *state = Some(LocalApicState { base });

    Ok(LocalApicInfo {
        id,
        version,
        physical_address,
    })
}

pub fn send_self_ipi(vector: u8) -> Status {
    if vector < 32 {
        return STATUS_UNSUPPORTED;
    }
    let Some(state) = *LOCAL_APIC.lock() else {
        return STATUS_UNSUPPORTED;
    };
    for _ in 0..IPI_TIMEOUT {
        if unsafe { read_register(state.base, REGISTER_ICR_LOW) } & DELIVERY_PENDING == 0 {
            unsafe {
                write_register(
                    state.base,
                    REGISTER_ICR_LOW,
                    DESTINATION_SELF | u32::from(vector),
                );
            }
            return STATUS_OK;
        }
        core::hint::spin_loop();
    }
    STATUS_IO_ERROR
}

pub fn end_of_interrupt() {
    if let Some(state) = *LOCAL_APIC.lock() {
        unsafe { write_register(state.base, REGISTER_EOI, 0) };
    }
}

unsafe fn read_register(base: usize, offset: usize) -> u32 {
    compiler_fence(Ordering::SeqCst);
    let value = unsafe { ((base + offset) as *const u32).read_volatile() };
    compiler_fence(Ordering::SeqCst);
    value
}

unsafe fn write_register(base: usize, offset: usize, value: u32) {
    compiler_fence(Ordering::SeqCst);
    unsafe { ((base + offset) as *mut u32).write_volatile(value) };
    compiler_fence(Ordering::SeqCst);
}
