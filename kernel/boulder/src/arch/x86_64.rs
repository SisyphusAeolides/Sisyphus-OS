use crate::arch::{Architecture, InterruptState};

pub mod privilege;

pub struct X86_64;

impl Architecture for X86_64 {
    const NAME: &'static str = "x86_64";
    const PAGE_SHIFT: usize = 12;
    const CACHE_LINE_SIZE: usize = 64;
    const MAXIMUM_CPUS: usize = 256;

    fn hardware_thread_id() -> u32 {
        let maximum_leaf = core::arch::x86_64::__cpuid(0).eax;
        for leaf in [0x1f, 0x0b] {
            if maximum_leaf >= leaf {
                let topology = core::arch::x86_64::__cpuid_count(leaf, 0);
                if topology.ebx != 0 {
                    return topology.edx;
                }
            }
        }
        core::arch::x86_64::__cpuid(1).ebx >> 24
    }

    fn counter_sample() -> u64 {
        let low: u32;
        let high: u32;
        // SAFETY: RDTSC is available on x86_64. The value is a local ordering
        // and accounting source until timer calibration establishes units.
        unsafe {
            core::arch::asm!(
                "rdtsc",
                out("eax") low,
                out("edx") high,
                options(nomem, nostack),
            );
        }
        (u64::from(high) << 32) | u64::from(low)
    }

    fn spin_wait() {
        core::hint::spin_loop();
    }

    fn halt() -> ! {
        halt()
    }

    fn save_and_disable_interrupts() -> InterruptState {
        let flags: usize;
        // SAFETY: Boulder executes this at ring 0. Reading RFLAGS and clearing
        // IF does not alter memory or the stack visible to Rust.
        unsafe {
            core::arch::asm!(
                "pushfq",
                "pop {flags}",
                "cli",
                flags = out(reg) flags,
                options(nomem),
            );
        }
        InterruptState::new(flags & (1 << 9) != 0)
    }

    unsafe fn restore_interrupts(state: InterruptState) {
        if state.interrupts_were_enabled() {
            // SAFETY: The method contract requires a matching state captured
            // on this hardware thread.
            unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        } else {
            // SAFETY: Keeping maskable interrupts disabled restores the saved
            // state without modifying unrelated RFLAGS bits.
            unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
        }
    }

    unsafe fn invalidate_local_page(virtual_address: usize) {
        // SAFETY: Forwarded under this method's page-table synchronization
        // contract.
        unsafe { invalidate_page(virtual_address) };
    }
}

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

/// Writes one 32-bit value to an x86 I/O port.
///
/// # Safety
///
/// The caller must ensure that the port exists, accepts double-word writes,
/// and is exclusively owned for the duration of this access.
pub unsafe fn outl(port: u16, value: u32) {
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Reads one 32-bit value from an x86 I/O port.
///
/// # Safety
///
/// The caller must ensure that the port exists, permits double-word reads,
/// and is exclusively owned for the duration of this access.
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!(
            "in eax, dx",
            in("dx") port,
            out("eax") value,
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

/// Enables the architectural no-execute page-table bit.
///
/// # Safety
///
/// The caller must execute at ring 0 during serialized bootstrap before any
/// page tables containing the no-execute bit can become active.
pub unsafe fn enable_execute_disable() -> Result<(), ExecuteDisableError> {
    let maximum_extended_leaf = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    if maximum_extended_leaf < 0x8000_0001
        || core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 20) == 0
    {
        return Err(ExecuteDisableError::Unsupported);
    }
    const EFER: u32 = 0xc000_0080;
    const EFER_NXE: u64 = 1 << 11;
    // SAFETY: The caller established ring-0 bootstrap context and CPUID
    // confirms support for EFER.NXE.
    let value = unsafe { read_msr(EFER) };
    unsafe { write_msr(EFER, value | EFER_NXE) };
    if unsafe { read_msr(EFER) } & EFER_NXE == 0 {
        return Err(ExecuteDisableError::EnableFailed);
    }
    Ok(())
}

/// Returns the physical frame containing the active level-four page table.
///
/// # Safety
///
/// The caller must execute at ring 0 on x86_64 and must treat the returned
/// root as shared hardware state unless it owns the required synchronization.
pub unsafe fn active_page_table_root() -> u64 {
    let value: u64;
    // SAFETY: The caller guarantees privileged x86_64 execution.
    unsafe {
        core::arch::asm!(
            "mov {}, cr3",
            out(reg) value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value & 0x000f_ffff_ffff_f000
}

/// Installs a level-four page-table root and flushes non-global translations.
///
/// # Safety
///
/// `root` must be a page-aligned physical address naming a valid x86_64
/// level-four hierarchy. That hierarchy must map the currently executing
/// code, stack, and every memory location needed before another root is
/// installed. The caller must serialize the switch against interrupt and
/// scheduler entry and must own any required cross-CPU protocol.
pub unsafe fn load_page_table_root(root: u64) {
    // SAFETY: The caller establishes the complete CR3-switch contract above.
    unsafe {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) root,
            options(nostack, preserves_flags),
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecuteDisableError {
    Unsupported,
    EnableFailed,
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
