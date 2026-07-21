mod apic;
mod idt;
mod irq;
mod pic;

use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::x86_64::halt;
use crate::serial::SerialPort;

pub use apic::LocalApicInfo;
pub use irq::{KernelIrq, kernel_irq};

core::arch::global_asm!(include_str!("stubs.S"), options(att_syntax));

const COM1: u16 = 0x3f8;
const LEGACY_IRQ_VECTOR_BASE: usize = 32;
const LEGACY_IRQ_VECTOR_END: usize = 48;
pub const APIC_TEST_VECTOR: u8 = 48;

static BREAKPOINT_HITS: AtomicUsize = AtomicUsize::new(0);
static APIC_TEST_HITS: AtomicUsize = AtomicUsize::new(0);

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

#[unsafe(no_mangle)]
extern "C" fn boulder_interrupt_dispatch(frame: *mut InterruptFrame) {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        halt();
    };
    match frame.vector {
        3 => {
            BREAKPOINT_HITS.fetch_add(1, Ordering::Relaxed);
        }
        LEGACY_IRQ_VECTOR_BASE..LEGACY_IRQ_VECTOR_END => {
            let irq = (frame.vector - LEGACY_IRQ_VECTOR_BASE) as u8;
            kernel_irq().dispatch(irq);
            pic::end_of_interrupt(irq);
        }
        48 => {
            APIC_TEST_HITS.fetch_add(1, Ordering::Relaxed);
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
}
