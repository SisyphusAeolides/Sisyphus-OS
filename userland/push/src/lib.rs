#![no_std]

pub mod aegis;
pub mod gordian;
pub mod logger;
pub mod service;

#[macro_export]
macro_rules! push_log {
    ($($argument:tt)*) => {{
        $crate::logger::write_line(format_args!($($argument)*));
    }};
}
