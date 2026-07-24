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

#[derive(Debug, Eq, PartialEq)]
pub struct DeadlineLease {
    apic_id: u32,
    generation: u64,
    live: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineState {
    Pending,
    Expired,
}

pub trait DeadlineClock {
    fn arm(&mut self, duration_ns: u64) -> Result<DeadlineLease, TimerError>;
    fn poll(&mut self, lease: &mut DeadlineLease) -> Result<DeadlineState, TimerError>;
    fn cancel(&mut self, lease: DeadlineLease) -> Result<(), TimerError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveDeadline {
    generation: u64,
    remaining_ticks: u64,
    armed_ticks: u32,
}

/// Exclusive, calibrated ownership of the bootstrap processor's local APIC
/// timer while it is still masked and operating as a one-shot deadline source.
///
/// This is deliberately not a monotonic clock: reprogramming the APIC counter
/// destroys its previous epoch. The owner can be consumed exactly once to
/// enter the scheduler's periodic mode.
#[derive(Debug)]
pub struct LocalApicDeadlineClock {
    base: usize,
    apic_id: u32,
    ticks_per_second: u64,
    generation: u64,
    active: Option<ActiveDeadline>,
    transitioned: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerError {
    LocalApicUnavailable,
    InvalidPeriod,
    InvalidDeadline,
    CalibrationTimeout,
    CalibrationFailed,
    ArithmeticOverflow,
    DeadlineBusy,
    StaleDeadline,
    WrongProcessor,
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

/// Calibrates the local APIC timer against PIT channel 2 and returns exclusive
/// ownership of it as a masked, one-shot relative deadline source.
///
/// # Safety
///
/// The caller must own PIT channel 2 and the local APIC timer while interrupts
/// are disabled.
pub unsafe fn calibrate_local_apic_deadline_clock() -> Result<LocalApicDeadlineClock, TimerError> {
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
            TIMER_MASKED | u32::from(SPURIOUS_VECTOR),
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
    let ticks_per_second = ceil_mul_div(
        u64::from(elapsed),
        u64::from(PIT_FREQUENCY_HZ),
        u64::from(CALIBRATION_PIT_DIVISOR),
    )
    .ok_or(TimerError::ArithmeticOverflow)?;
    if ticks_per_second == 0 {
        return Err(TimerError::CalibrationFailed);
    }

    Ok(LocalApicDeadlineClock {
        base: state.base,
        apic_id: crate::arch::x86_64::current_hardware_thread_id(),
        ticks_per_second,
        generation: 0,
        active: None,
        transitioned: false,
    })
}

impl LocalApicDeadlineClock {
    pub const fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    /// Consumes deadline ownership and installs the scheduler's periodic timer.
    /// On validation failure, the still-usable deadline owner is returned.
    pub fn start_periodic(
        mut self,
        vector: u8,
        period_milliseconds: u32,
    ) -> Result<LocalApicTimerInfo, (TimerError, Self)> {
        if vector < 32 || period_milliseconds == 0 || self.active.is_some() {
            let error = if self.active.is_some() {
                TimerError::DeadlineBusy
            } else {
                TimerError::InvalidPeriod
            };
            return Err((error, self));
        }
        if !self.is_current_processor() {
            return Err((TimerError::WrongProcessor, self));
        }
        let duration_ns = match u64::from(period_milliseconds).checked_mul(1_000_000) {
            Some(duration) => duration,
            None => return Err((TimerError::ArithmeticOverflow, self)),
        };
        let initial_count = match deadline_ticks(duration_ns, self.ticks_per_second)
            .and_then(|ticks| u32::try_from(ticks).ok())
            .filter(|count| *count != 0)
        {
            Some(count) => count,
            None => return Err((TimerError::InvalidPeriod, self)),
        };

        unsafe {
            write_register(self.base, REGISTER_TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
            write_register(
                self.base,
                REGISTER_LVT_TIMER,
                TIMER_PERIODIC | u32::from(vector),
            );
            write_register(self.base, REGISTER_TIMER_INITIAL_COUNT, initial_count);
        }
        self.transitioned = true;
        Ok(LocalApicTimerInfo {
            ticks_per_second: self.ticks_per_second,
            period_milliseconds,
            initial_count,
        })
    }

    fn is_current_processor(&self) -> bool {
        crate::arch::x86_64::current_hardware_thread_id() == self.apic_id
    }

    fn validate_lease(&self, lease: &DeadlineLease) -> Result<ActiveDeadline, TimerError> {
        if !self.is_current_processor() {
            return Err(TimerError::WrongProcessor);
        }
        let active = self.active.ok_or(TimerError::StaleDeadline)?;
        if !lease.live || lease.apic_id != self.apic_id || lease.generation != active.generation {
            return Err(TimerError::StaleDeadline);
        }
        Ok(active)
    }

    fn program_one_shot(&self, ticks: u32) {
        unsafe {
            write_register(self.base, REGISTER_TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
            write_register(
                self.base,
                REGISTER_LVT_TIMER,
                TIMER_MASKED | u32::from(SPURIOUS_VECTOR),
            );
            write_register(self.base, REGISTER_TIMER_INITIAL_COUNT, ticks);
        }
    }
}

impl DeadlineClock for LocalApicDeadlineClock {
    fn arm(&mut self, duration_ns: u64) -> Result<DeadlineLease, TimerError> {
        if duration_ns == 0 {
            return Err(TimerError::InvalidDeadline);
        }
        if !self.is_current_processor() {
            return Err(TimerError::WrongProcessor);
        }
        if self.active.is_some() {
            return Err(TimerError::DeadlineBusy);
        }
        let remaining_ticks = deadline_ticks(duration_ns, self.ticks_per_second)
            .ok_or(TimerError::ArithmeticOverflow)?;
        let armed_ticks = remaining_ticks.min(u64::from(u32::MAX)) as u32;
        if armed_ticks == 0 {
            return Err(TimerError::InvalidDeadline);
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.generation = 1;
        }
        self.active = Some(ActiveDeadline {
            generation: self.generation,
            remaining_ticks,
            armed_ticks,
        });
        self.program_one_shot(armed_ticks);
        Ok(DeadlineLease {
            apic_id: self.apic_id,
            generation: self.generation,
            live: true,
        })
    }

    fn poll(&mut self, lease: &mut DeadlineLease) -> Result<DeadlineState, TimerError> {
        let active = self.validate_lease(lease)?;
        let current = unsafe { read_register(self.base, REGISTER_TIMER_CURRENT_COUNT) };
        if current != 0 {
            return Ok(DeadlineState::Pending);
        }
        let (next, state) = advance_expired_chunk(active);
        self.active = next;
        if let Some(next) = next {
            self.program_one_shot(next.armed_ticks);
        } else {
            lease.live = false;
        }
        Ok(state)
    }

    fn cancel(&mut self, lease: DeadlineLease) -> Result<(), TimerError> {
        self.validate_lease(&lease)?;
        unsafe { write_register(self.base, REGISTER_TIMER_INITIAL_COUNT, 0) };
        self.active = None;
        Ok(())
    }
}

impl Drop for LocalApicDeadlineClock {
    fn drop(&mut self) {
        if !self.transitioned {
            unsafe {
                write_register(
                    self.base,
                    REGISTER_LVT_TIMER,
                    TIMER_MASKED | u32::from(SPURIOUS_VECTOR),
                );
                write_register(self.base, REGISTER_TIMER_INITIAL_COUNT, 0);
            }
        }
    }
}

fn deadline_ticks(duration_ns: u64, ticks_per_second: u64) -> Option<u64> {
    if duration_ns == 0 || ticks_per_second == 0 {
        return None;
    }
    ceil_mul_div(duration_ns, ticks_per_second, 1_000_000_000)
}

fn advance_expired_chunk(active: ActiveDeadline) -> (Option<ActiveDeadline>, DeadlineState) {
    if active.remaining_ticks <= u64::from(active.armed_ticks) {
        return (None, DeadlineState::Expired);
    }
    let remaining_ticks = active.remaining_ticks - u64::from(active.armed_ticks);
    let armed_ticks = remaining_ticks.min(u64::from(u32::MAX)) as u32;
    (
        Some(ActiveDeadline {
            remaining_ticks,
            armed_ticks,
            ..active
        }),
        DeadlineState::Pending,
    )
}

fn ceil_mul_div(value: u64, multiplier: u64, divisor: u64) -> Option<u64> {
    if divisor == 0 {
        return None;
    }
    let numerator = u128::from(value).checked_mul(u128::from(multiplier))?;
    let quotient = numerator
        .checked_add(u128::from(divisor) - 1)?
        .checked_div(u128::from(divisor))?;
    u64::try_from(quotient).ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_clock(registers: &mut [u32; 1024], ticks_per_second: u64) -> LocalApicDeadlineClock {
        LocalApicDeadlineClock {
            base: registers.as_mut_ptr() as usize,
            apic_id: crate::arch::x86_64::current_hardware_thread_id(),
            ticks_per_second,
            generation: 0,
            active: None,
            transitioned: false,
        }
    }

    #[test]
    fn pit_calibration_uses_the_programmed_divisor() {
        let elapsed = 25_000_u64;
        assert_eq!(
            ceil_mul_div(
                elapsed,
                u64::from(PIT_FREQUENCY_HZ),
                u64::from(CALIBRATION_PIT_DIVISOR),
            ),
            Some(2_500_172)
        );
        assert_ne!(
            ceil_mul_div(
                elapsed,
                u64::from(PIT_FREQUENCY_HZ),
                u64::from(CALIBRATION_PIT_DIVISOR),
            ),
            Some(elapsed * 100)
        );
    }

    #[test]
    fn deadline_conversion_rounds_up_without_early_expiry() {
        assert_eq!(deadline_ticks(16_000_000, 2_500_382), Some(40_007));
        assert_eq!(deadline_ticks(1_000_000_000, 2_500_382), Some(2_500_382));
        assert_eq!(deadline_ticks(1, 1), Some(1));
    }

    #[test]
    fn conversion_fails_closed_on_zero_or_overflow() {
        assert_eq!(deadline_ticks(0, 1), None);
        assert_eq!(deadline_ticks(1, 0), None);
        assert_eq!(ceil_mul_div(u64::MAX, u64::MAX, 1), None);
        assert_eq!(ceil_mul_div(1, 1, 0), None);
    }

    #[test]
    fn long_deadlines_are_split_into_full_width_chunks() {
        let first = ActiveDeadline {
            generation: 7,
            remaining_ticks: u64::from(u32::MAX) + 17,
            armed_ticks: u32::MAX,
        };
        let (second, state) = advance_expired_chunk(first);
        assert_eq!(state, DeadlineState::Pending);
        assert_eq!(
            second,
            Some(ActiveDeadline {
                generation: 7,
                remaining_ticks: 17,
                armed_ticks: 17,
            })
        );
        assert_eq!(
            advance_expired_chunk(second.unwrap()),
            (None, DeadlineState::Expired)
        );
    }

    #[test]
    fn one_shot_owner_rejects_aliasing_and_stale_leases() {
        let mut registers = [0_u32; 1024];
        let mut clock = test_clock(&mut registers, 1_000);
        let mut first = clock.arm(16_000_000).unwrap();
        assert_eq!(
            registers[REGISTER_TIMER_INITIAL_COUNT / 4],
            16,
            "16 ms at 1 kHz must arm exactly sixteen ticks"
        );
        assert_eq!(clock.arm(1), Err(TimerError::DeadlineBusy));

        registers[REGISTER_TIMER_CURRENT_COUNT / 4] = 1;
        assert_eq!(clock.poll(&mut first), Ok(DeadlineState::Pending));
        registers[REGISTER_TIMER_CURRENT_COUNT / 4] = 0;
        assert_eq!(clock.poll(&mut first), Ok(DeadlineState::Expired));
        assert_eq!(clock.poll(&mut first), Err(TimerError::StaleDeadline));

        let second = clock.arm(1_000_000).unwrap();
        assert_ne!(first.generation, second.generation);
        clock.cancel(second).unwrap();
        assert_eq!(registers[REGISTER_TIMER_INITIAL_COUNT / 4], 0);
    }

    #[test]
    fn periodic_transition_consumes_only_an_idle_deadline_owner() {
        let mut registers = [0_u32; 1024];
        let mut clock = test_clock(&mut registers, 1_000_000);
        let lease = clock.arm(1_000_000).unwrap();
        let (error, mut clock) = clock.start_periodic(49, 10).unwrap_err();
        assert_eq!(error, TimerError::DeadlineBusy);
        clock.cancel(lease).unwrap();

        let info = clock.start_periodic(49, 10).unwrap();
        assert_eq!(info.initial_count, 10_000);
        assert_eq!(registers[REGISTER_LVT_TIMER / 4], TIMER_PERIODIC | 49);
        assert_eq!(registers[REGISTER_TIMER_INITIAL_COUNT / 4], 10_000);
    }
}
