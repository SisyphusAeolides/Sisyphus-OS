mod apic;
mod exceptions;
mod idt;
mod ioapic;
mod irq;
pub mod neuromorphic;
mod pic;
pub mod synaptic;

use core::fmt::Write;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::arch::x86_64::halt;
use crate::serial::SerialPort;

pub use apic::{
    DeadlineClock, DeadlineLease, DeadlineState, LocalApicDeadlineClock, LocalApicInfo,
    LocalApicTimerInfo, TimerError,
};
pub use idt::{IdtError, IdtInfo};
pub use ioapic::{IoApicError, IoApicInfo};
pub use irq::{KernelIrq, kernel_irq};

core::arch::global_asm!(include_str!("stubs.S"), options(att_syntax));

const COM1: u16 = 0x3f8;
const LEGACY_IRQ_VECTOR_BASE: usize = 32;
const LEGACY_IRQ_VECTOR_END: usize = 48;
pub const APIC_TEST_VECTOR: u8 = 48;
pub const APIC_TIMER_VECTOR: u8 = 49;

static BREAKPOINT_HITS: AtomicUsize = AtomicUsize::new(0);
static USER_PROBE_HITS: AtomicUsize = AtomicUsize::new(0);
static APIC_TEST_HITS: AtomicUsize = AtomicUsize::new(0);
static APIC_TIMER_HITS: AtomicUsize = AtomicUsize::new(0);
static IST_PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);
static IST_PROBE_HITS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
pub struct InterruptFrame {
    pub r15: usize,
    pub r14: usize,
    pub r13: usize,
    pub r12: usize,
    pub r11: usize,
    pub r10: usize,
    pub r9: usize,
    pub r8: usize,
    pub rbp: usize,
    pub rdi: usize,
    pub rsi: usize,
    pub rdx: usize,
    pub rcx: usize,
    pub rbx: usize,
    pub rax: usize,
    pub vector: usize,
    pub error_code: usize,
    pub instruction_pointer: usize,
    pub code_segment: usize,
    pub flags: usize,
}

/// Installs the IDT and initializes the masked legacy PIC routing path.
///
/// # Safety
///
/// This must be called once on the bootstrap CPU before interrupts are enabled.
pub unsafe fn initialize() -> Result<IdtInfo, IdtError> {
    unsafe {
        let info = idt::initialize()?;
        pic::initialize();
        Ok(info)
    }
}

pub fn enable() {
    unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
}

pub fn disable() {
    unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)) };
}

pub fn trigger_breakpoint() {
    unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
}

/// Exercises the NMI descriptor's IST switch once from Ring 0.
///
/// This is a software-vector bootstrap probe: it verifies the IDT/TSS stack
/// switch without manufacturing a real non-maskable hardware event.
pub fn trigger_ist_probe() -> bool {
    if IST_PROBE_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let before = IST_PROBE_HITS.load(Ordering::Acquire);
    unsafe { core::arch::asm!("int 2") };
    !IST_PROBE_ACTIVE.load(Ordering::Acquire)
        && IST_PROBE_HITS.load(Ordering::Acquire) == before.saturating_add(1)
}

pub fn breakpoint_hits() -> usize {
    BREAKPOINT_HITS.load(Ordering::Relaxed)
}

pub fn user_probe_hits() -> usize {
    USER_PROBE_HITS.load(Ordering::Relaxed)
}

pub fn apic_capabilities() -> (bool, bool) {
    let features = core::arch::x86_64::__cpuid(1);
    let local_apic = features.edx & (1 << 9) != 0;
    let x2apic = features.ecx & (1 << 21) != 0;
    (local_apic, x2apic)
}

/// Initializes the local xAPIC through the installed MMIO mapper.
///
/// # Safety
///
/// This must run once on the bootstrap CPU while interrupts are disabled.
pub unsafe fn initialize_local_apic(
    mmio: &dyn crate::shim::MmioService,
) -> Result<LocalApicInfo, sisyphus_driver_abi::Status> {
    unsafe { apic::initialize(mmio) }
}

pub fn send_apic_test_ipi() -> sisyphus_driver_abi::Status {
    apic::send_self_ipi(APIC_TEST_VECTOR)
}

pub fn apic_test_hits() -> usize {
    APIC_TEST_HITS.load(Ordering::Relaxed)
}

/// Calibrates the bootstrap processor's local APIC timer and retains it as a
/// masked one-shot deadline source. The returned owner must later be consumed
/// into periodic mode before timer interrupts are enabled.
///
/// # Safety
///
/// This must be called on the bootstrap CPU with interrupts disabled and with
/// exclusive ownership of PIT channel 2 and the local APIC timer.
pub unsafe fn initialize_local_apic_deadline_clock() -> Result<LocalApicDeadlineClock, TimerError> {
    APIC_TIMER_HITS.store(0, Ordering::Relaxed);
    unsafe { apic::calibrate_local_apic_deadline_clock() }
}

pub fn apic_timer_hits() -> usize {
    APIC_TIMER_HITS.load(Ordering::Relaxed)
}

/// Maps and initializes every I/O APIC described by ACPI.
///
/// # Safety
///
/// The MADT must describe the active platform, the local APIC must already be
/// initialized, and interrupts must remain disabled during reconfiguration.
pub unsafe fn initialize_io_apics(
    madt: &crate::boot::acpi::MadtInfo,
    mmio: &dyn crate::shim::MmioService,
    destination_apic_id: u8,
) -> Result<IoApicInfo, IoApicError> {
    unsafe { ioapic::initialize(madt, mmio, destination_apic_id) }
}

fn set_irq_masked(irq: u8, masked: bool) {
    if !ioapic::set_masked(irq, masked) {
        pic::set_masked(irq, masked);
    }
}

#[unsafe(no_mangle)]
extern "C" fn boulder_interrupt_dispatch(frame: *mut InterruptFrame) -> usize {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        halt();
    };
    match frame.vector {
        2 if IST_PROBE_ACTIVE.swap(false, Ordering::AcqRel) => {
            let frame_start = frame as *mut InterruptFrame as u64;
            let frame_end = frame_start
                .checked_add(size_of::<InterruptFrame>() as u64)
                .and_then(|end| end.checked_add(2 * size_of::<usize>() as u64));
            if frame_end.is_none_or(|end| {
                !crate::arch::x86_64::privilege::active_ist_range_is_contained(
                    crate::arch::x86_64::privilege::NMI_IST_INDEX,
                    frame_start,
                    end,
                )
            }) {
                halt();
            }
            IST_PROBE_HITS.fetch_add(1, Ordering::Release);
        }
        2 | 8 | 18 => {
            let mut serial = unsafe { SerialPort::initialize(COM1) };
            let class = match frame.vector {
                2 => "non-maskable interrupt",
                8 => "double fault",
                18 => "machine check",
                _ => unreachable!(),
            };
            let _ = writeln!(
                serial,
                "Boulder contained {class}: vector={} error={:#x} rip={:#x}",
                frame.vector, frame.error_code, frame.instruction_pointer
            );
            halt();
        }
        3 => {
            if frame.code_segment & 3 == 3 {
                let user_code_segment = frame.code_segment;
                let user_instruction_pointer = frame.instruction_pointer;
                if crate::arch::x86_64::privilege::complete_user_probe() {
                    USER_PROBE_HITS.fetch_add(1, Ordering::Relaxed);
                    let mut serial = unsafe { SerialPort::initialize(COM1) };
                    let _ = writeln!(
                        serial,
                        "Boulder: Ring 3 trap rip={user_instruction_pointer:#x} cs={user_code_segment:#x}, returning through RSP0",
                    );
                    return 1;
                } else {
                    halt();
                }
            } else {
                BREAKPOINT_HITS.fetch_add(1, Ordering::Relaxed);
            }
        }
        14 => {
            let fault = exceptions::page_fault(frame.error_code);
            let mut serial = unsafe { SerialPort::initialize(COM1) };
            let _ = writeln!(
                serial,
                "Boulder page fault: address={:#x} error={:#x} present={} write={} user={} execute={}",
                fault.address,
                fault.error_code,
                fault.protection_violation,
                fault.write,
                fault.user,
                fault.instruction_fetch
            );
            halt();
        }
        LEGACY_IRQ_VECTOR_BASE..LEGACY_IRQ_VECTOR_END => {
            let irq = (frame.vector - LEGACY_IRQ_VECTOR_BASE) as u8;
            kernel_irq().dispatch(irq);
            if ioapic::is_initialized() {
                apic::end_of_interrupt();
            } else {
                pic::end_of_interrupt(irq);
            }
        }
        48 => {
            APIC_TEST_HITS.fetch_add(1, Ordering::Relaxed);
            apic::end_of_interrupt();
        }
        49 => {
            APIC_TIMER_HITS.fetch_add(1, Ordering::Relaxed);

            let wall_tick = <crate::arch::Active as crate::arch::Architecture>::counter_sample();

            crate::nexus_deferred::request_from_irq(wall_tick);
            crate::process::preemption::request_from_timer_irq(wall_tick);

            apic::end_of_interrupt();
        }
        255 => {}
        _ => {
            let mut serial = unsafe { SerialPort::initialize(COM1) };
            let _ = writeln!(
                serial,
                "Boulder exception: vector={} error={:#x} rip={:#x}",
                frame.vector, frame.error_code, frame.instruction_pointer
            );
            halt();
        }
    }
    0
}
