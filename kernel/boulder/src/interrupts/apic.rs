use core::sync::atomic::{Ordering, compiler_fence};

use sisyphus_driver_abi::{STATUS_BUSY, STATUS_IO_ERROR, STATUS_OK, STATUS_UNSUPPORTED, Status};

use crate::arch::x86_64::{inb, outb, read_msr, write_msr};
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
const REGISTER_LVT_TIMER: usize = 0x320;
const REGISTER_TIMER_INITIAL_COUNT: usize = 0x380;
const REGISTER_TIMER_CURRENT_COUNT: usize = 0x390;
const REGISTER_TIMER_DIVIDE: usize = 0x3e0;
const SOFTWARE_ENABLE: u32 = 1 << 8;
const DELIVERY_PENDING: u32 = 1 << 12;
const DESTINATION_SELF: u32 = 1 << 18;
const SPURIOUS_VECTOR: u8 = 0xff;
const IPI_TIMEOUT: usize = 1_000_000;
const TIMER_MASKED: u32 = 1 << 16;
const TIMER_PERIODIC: u32 = 1 << 17;
const TIMER_DIVIDE_BY_16: u32 = 0x3;
const PIT_CHANNEL_2: u16 = 0x42;
const PIT_COMMAND: u16 = 0x43;
const PIT_SPEAKER_CONTROL: u16 = 0x61;
const PIT_CHANNEL_2_MODE_0: u8 = 0xb0;
const PIT_FREQUENCY_HZ: u32 = 1_193_182;
const CALIBRATION_MILLISECONDS: u32 = 10;
const CALIBRATION_PIT_DIVISOR: u16 = (PIT_FREQUENCY_HZ / (1000 / CALIBRATION_MILLISECONDS)) as u16;
const CALIBRATION_TIMEOUT: usize = 100_000_000;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalApicTimerInfo {
    pub ticks_per_second: u64,
    pub period_milliseconds: u32,
    pub initial_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerError {
    LocalApicUnavailable,
    InvalidPeriod,
    CalibrationTimeout,
    CalibrationFailed,
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

/// Calibrates the local APIC timer against PIT channel 2 and starts it in
/// periodic mode.
///
/// # Safety
///
/// The caller must own PIT channel 2 and the local APIC timer while interrupts
/// are disabled. `vector` must name an installed interrupt gate.
pub unsafe fn calibrate_and_start_timer(
    vector: u8,
    period_milliseconds: u32,
) -> Result<LocalApicTimerInfo, TimerError> {
    if vector < 32 || period_milliseconds == 0 {
        return Err(TimerError::InvalidPeriod);
    }
    let Some(state) = *LOCAL_APIC.lock() else {
        return Err(TimerError::LocalApicUnavailable);
    };

    let speaker_control = unsafe { inb(PIT_SPEAKER_CONTROL) };
    unsafe {
        outb(PIT_SPEAKER_CONTROL, speaker_control & !0x03);
        outb(PIT_COMMAND, PIT_CHANNEL_2_MODE_0);
        outb(PIT_CHANNEL_2, CALIBRATION_PIT_DIVISOR as u8);
        outb(PIT_CHANNEL_2, (CALIBRATION_PIT_DIVISOR >> 8) as u8);
        write_register(state.base, REGISTER_TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
        write_register(
            state.base,
            REGISTER_LVT_TIMER,
            TIMER_MASKED | u32::from(vector),
        );
        write_register(state.base, REGISTER_TIMER_INITIAL_COUNT, u32::MAX);
        outb(PIT_SPEAKER_CONTROL, (speaker_control & !0x02) | 0x01);
    }

    let mut completed = false;
    for _ in 0..CALIBRATION_TIMEOUT {
        if unsafe { inb(PIT_SPEAKER_CONTROL) } & 0x20 != 0 {
            completed = true;
            break;
        }
        core::hint::spin_loop();
    }
    let current_count = unsafe { read_register(state.base, REGISTER_TIMER_CURRENT_COUNT) };
    unsafe {
        write_register(state.base, REGISTER_TIMER_INITIAL_COUNT, 0);
        outb(PIT_SPEAKER_CONTROL, speaker_control);
    }
    if !completed {
        return Err(TimerError::CalibrationTimeout);
    }

    let elapsed = u32::MAX - current_count;
    if elapsed == 0 {
        return Err(TimerError::CalibrationFailed);
    }
    let ticks_per_second = u64::from(elapsed) * (1000 / u64::from(CALIBRATION_MILLISECONDS));
    let initial_count = ticks_per_second
        .checked_mul(u64::from(period_milliseconds))
        .and_then(|ticks| ticks.checked_div(1000))
        .and_then(|ticks| u32::try_from(ticks).ok())
        .filter(|count| *count != 0)
        .ok_or(TimerError::InvalidPeriod)?;

    unsafe {
        write_register(state.base, REGISTER_TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
        write_register(
            state.base,
            REGISTER_LVT_TIMER,
            TIMER_PERIODIC | u32::from(vector),
        );
        write_register(state.base, REGISTER_TIMER_INITIAL_COUNT, initial_count);
    }
    Ok(LocalApicTimerInfo {
        ticks_per_second,
        period_milliseconds,
        initial_count,
    })
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
