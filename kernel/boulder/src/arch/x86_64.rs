/// Writes one byte to an x86 I/O port.
///
/// # Safety
///
/// The caller must ensure that the port exists, accepts byte writes, and that
/// this access does not violate ownership of the underlying device.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Reads one byte from an x86 I/O port.
///
/// # Safety
///
/// The caller must ensure that the port exists, permits byte reads, and that
/// this access does not violate ownership of the underlying device.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// Invalidates the local TLB entry for one virtual address.
///
/// # Safety
///
/// The caller must ensure page-table updates for `address` are complete and
/// synchronized before invalidating the translation.
pub unsafe fn invalidate_page(address: usize) {
    unsafe {
        core::arch::asm!(
            "invlpg [{}]",
            in(reg) address,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads a model-specific register.
///
/// # Safety
///
/// `register` must exist and permit reads at the current privilege level.
pub unsafe fn read_msr(register: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") register,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Writes a model-specific register.
///
/// # Safety
///
/// `register` and `value` must form a valid writable MSR operation for the
/// current CPU mode. Invalid writes can fault or destabilize the processor.
pub unsafe fn write_msr(register: u32, value: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") register,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

pub fn halt() -> ! {
    loop {
        unsafe {
            core::arch::asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}

pub fn idle() -> ! {
    loop {
        unsafe {
            core::arch::asm!("sti", "hlt", options(nomem, nostack));
        }
    }
}
