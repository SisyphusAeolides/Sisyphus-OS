use core::mem::size_of;

const IDT_ENTRIES: usize = 256;
const INTERRUPT_STUBS: usize = 50;
const KERNEL_CODE_SELECTOR: u16 = 0x08;
const INTERRUPT_GATE: u8 = 0x8e;
const USER_INTERRUPT_GATE: u8 = 0xee;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const MISSING: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    };

    const fn interrupt_gate(handler: usize, attributes: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: KERNEL_CODE_SELECTOR,
            ist: 0,
            attributes,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

unsafe extern "C" {
    static isr_stub_table: [usize; INTERRUPT_STUBS];
    fn isr_unhandled();
}

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::MISSING; IDT_ENTRIES];

/// Installs Boulder's exception and legacy IRQ descriptor table.
///
/// # Safety
///
/// This must run once on the bootstrap CPU while interrupts are disabled. The
/// code selector and assembly stubs must match the active GDT and stack format.
pub unsafe fn initialize() {
    let idt = core::ptr::addr_of_mut!(IDT).cast::<IdtEntry>();
    let fallback = IdtEntry::interrupt_gate(isr_unhandled as *const () as usize, INTERRUPT_GATE);
    for index in 0..IDT_ENTRIES {
        unsafe { idt.add(index).write(fallback) };
    }

    let stubs = core::ptr::addr_of!(isr_stub_table).cast::<usize>();
    for index in 0..INTERRUPT_STUBS {
        let handler = unsafe { stubs.add(index).read() };
        let attributes = if index == 3 {
            USER_INTERRUPT_GATE
        } else {
            INTERRUPT_GATE
        };
        unsafe {
            idt.add(index)
                .write(IdtEntry::interrupt_gate(handler, attributes))
        };
    }

    let pointer = IdtPointer {
        limit: (size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16,
        base: idt as u64,
    };
    unsafe {
        core::arch::asm!(
            "lidt [{}]",
            in(reg) &pointer,
            options(readonly, nostack, preserves_flags),
        );
    }
}

const _: () = assert!(size_of::<IdtEntry>() == 16);
