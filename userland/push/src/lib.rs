#![no_std]
extern crate alloc;

pub mod aegis;
pub mod chronos;
pub mod gordian;
pub mod init;
pub mod logger;
pub mod morpheus;
pub mod noosphere;
pub mod service;
#[macro_export]
macro_rules! push_log {
    ($($argument:tt)*) => {{
        $crate::logger::write_line(format_args!($($argument)*));
    }};
}
