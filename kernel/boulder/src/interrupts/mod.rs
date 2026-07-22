mod apic;
mod exceptions;
mod idt;
mod ioapic;
mod irq;
mod pic;
pub mod neuromorphic;
pub mod synaptic;

use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::x86_64::halt;
use crate::serial::SerialPort;

pub use apic::{LocalApicInfo, LocalApicTimerInfo, TimerError};
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
pub unsafe fn initialize() {
    unsafe {
        idt::initialize();
        pic::initialize();
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

/// Calibrates and starts Boulder's periodic local APIC timer.
///
/// # Safety
///
/// This must be called on the bootstrap CPU with interrupts disabled and with
/// exclusive ownership of PIT channel 2.
pub unsafe fn initialize_local_apic_timer(
    period_milliseconds: u32,
) -> Result<LocalApicTimerInfo, TimerError> {
    APIC_TIMER_HITS.store(0, Ordering::Relaxed);
    unsafe { apic::calibrate_and_start_timer(APIC_TIMER_VECTOR, period_milliseconds) }
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

            let wall_tick =
                <crate::arch::Active as crate::arch::Architecture>
                    ::counter_sample();

            crate::nexus_deferred::request_from_irq(wall_tick);

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
