use core::fmt::{self, Write};

const SYSCALL_WRITE_LIMIT: usize = 256;

pub struct ChunkedLogger {
    buffer: [u8; SYSCALL_WRITE_LIMIT],
    cursor: usize,
}

impl ChunkedLogger {
    pub const fn new() -> Self {
        Self {
            buffer: [0; SYSCALL_WRITE_LIMIT],
            cursor: 0,
        }
    }

    pub fn flush(&mut self) {
        if self.cursor != 0 {
            let _ = slope::io::write(1, &self.buffer[..self.cursor]);
            self.cursor = 0;
        }
    }
}

impl Default for ChunkedLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for ChunkedLogger {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut remaining = text.as_bytes();
        while !remaining.is_empty() {
            let available = self.buffer.len() - self.cursor;
            let length = core::cmp::min(available, remaining.len());
            self.buffer[self.cursor..self.cursor + length].copy_from_slice(&remaining[..length]);
            self.cursor += length;
            remaining = &remaining[length..];
            if self.cursor == self.buffer.len() {
                self.flush();
            }
        }
        Ok(())
    }
}

pub fn write_line(arguments: fmt::Arguments<'_>) {
    let mut logger = ChunkedLogger::new();
    let _ = logger.write_fmt(arguments);
    let _ = logger.write_char('\n');
    logger.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logger_storage_matches_the_kernel_write_bound() {
        let logger = ChunkedLogger::new();
        assert_eq!(logger.buffer.len(), 256);
        assert_eq!(logger.cursor, 0);
    }
}
