use crate::arch::x86_64::{inb, outb};
use crate::sync::SpinLock;

const MASTER_COMMAND: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_COMMAND: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;
const END_OF_INTERRUPT: u8 = 0x20;

struct Pic8259 {
    masks: u16,
    initialized: bool,
}

impl Pic8259 {
    const fn new() -> Self {
        Self {
            masks: u16::MAX,
            initialized: false,
        }
    }

    unsafe fn initialize(&mut self) {
        unsafe {
            outb(MASTER_COMMAND, 0x11);
            io_wait();
            outb(SLAVE_COMMAND, 0x11);
            io_wait();
            outb(MASTER_DATA, 32);
            io_wait();
            outb(SLAVE_DATA, 40);
            io_wait();
            outb(MASTER_DATA, 1 << 2);
            io_wait();
            outb(SLAVE_DATA, 2);
            io_wait();
            outb(MASTER_DATA, 0x01);
            io_wait();
            outb(SLAVE_DATA, 0x01);
            io_wait();
            outb(MASTER_DATA, 0xff);
            outb(SLAVE_DATA, 0xff);
        }
        self.masks = u16::MAX;
        self.initialized = true;
    }

    unsafe fn set_masked(&mut self, irq: u8, masked: bool) {
        if irq >= 16 || !self.initialized {
            return;
        }
        let bit = 1_u16 << irq;
        if masked {
            self.masks |= bit;
        } else {
            self.masks &= !bit;
        }
        if irq >= 8 {
            if masked && self.masks & 0xff00 == 0xff00 {
                self.masks |= 1 << 2;
            } else if !masked {
                self.masks &= !(1 << 2);
            }
        }
        unsafe {
            outb(MASTER_DATA, self.masks as u8);
            outb(SLAVE_DATA, (self.masks >> 8) as u8);
        }
    }

    unsafe fn end_of_interrupt(&self, irq: u8) {
        if irq >= 8 {
            unsafe { outb(SLAVE_COMMAND, END_OF_INTERRUPT) };
        }
        unsafe { outb(MASTER_COMMAND, END_OF_INTERRUPT) };
    }
}

static PIC: SpinLock<Pic8259> = SpinLock::new(Pic8259::new());

/// Remaps both legacy PICs to vectors 32 through 47 and masks every line.
///
/// # Safety
///
/// This must run with interrupts disabled and exclusive ownership of the 8259
/// command/data ports.
pub unsafe fn initialize() {
    unsafe { PIC.lock().initialize() };
}

pub fn set_masked(irq: u8, masked: bool) {
    unsafe { PIC.lock().set_masked(irq, masked) };
}

pub fn end_of_interrupt(irq: u8) {
    unsafe { PIC.lock().end_of_interrupt(irq) };
}

unsafe fn io_wait() {
    unsafe { outb(0x80, 0) };
}

#[allow(dead_code)]
fn read_masks() -> u16 {
    unsafe { u16::from(inb(MASTER_DATA)) | (u16::from(inb(SLAVE_DATA)) << 8) }
}
