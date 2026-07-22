use core::fmt::{self, Write};

use crate::arch::x86_64::{inb, outb};

pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    /// Creates and initializes a 16550-compatible serial port.
    ///
    /// # Safety
    ///
    /// `base` must identify a 16550-compatible UART exclusively owned by this
    /// writer. On a standard PC, COM1 uses `0x3f8`.
    pub unsafe fn initialize(base: u16) -> Self {
        unsafe {
            outb(base + 1, 0x00);
            outb(base + 3, 0x80);
            outb(base, 0x03);
            outb(base + 1, 0x00);
            outb(base + 3, 0x03);
            outb(base + 2, 0xc7);
            outb(base + 4, 0x0b);
        }
        Self { base }
    }

    fn write_byte(&mut self, byte: u8) {
        while unsafe { inb(self.base + 5) } & 0x20 == 0 {
            core::hint::spin_loop();
        }
        unsafe { outb(self.base, byte) };
    }

    /// Writes an arbitrary byte sequence to the UART.
    ///
    /// Newlines receive the same carriage-return translation as formatted
    /// kernel output; bytes do not need to be valid UTF-8.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.write_bytes(text.as_bytes());
        Ok(())
    }
}
