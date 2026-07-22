#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let _ = slope::io::write(1, b"[CREST] visual cortex awaiting a GPU capability\n");
    loop {
        if slope::process::yield_now().is_err() {
            core::hint::spin_loop();
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    let _ = slope::io::write(1, b"[CREST] critical cortex failure\n");
    loop {
        let _ = slope::process::request_exit(1);
        let _ = slope::process::yield_now();
    }
}
