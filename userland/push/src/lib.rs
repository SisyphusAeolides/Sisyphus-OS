#![no_std]
extern crate alloc;

pub mod init;
pub mod aegis;
pub mod gordian;
pub mod logger;
pub mod service;
pub mod noosphere;
pub mod chronos;
pub mod morpheus;
#[macro_export]
macro_rules! push_log {
    ($($argument:tt)*) => {{
        $crate::logger::write_line(format_args!($($argument)*));
    }};
}
