use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_DATA_SELECTOR: u16 = 0x1b;
pub const USER_CODE_SELECTOR: u16 = 0x23;
#[cfg(target_os = "none")]
const TSS_SELECTOR: u16 = 0x28;
#[cfg(target_os = "none")]
const GDT_ENTRIES: usize = 7;
#[cfg(target_os = "none")]
const KERNEL_ENTRY_STACK_BYTES: usize = 16 * 1024;
#[cfg(target_os = "none")]
const USER_ADDRESS_LIMIT: usize = 0x0000_8000_0000_0000;

#[cfg(target_os = "none")]
core::arch::global_asm!(include_str!("ring3.S"), options(att_syntax));
#[cfg(target_os = "none")]
core::arch::global_asm!(include_str!("syscall.S"), options(att_syntax));

#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    io_bitmap_offset: u16,
}

impl TaskStateSegment {
    #[cfg(target_os = "none")]
    const EMPTY: Self = Self {
        reserved0: 0,
        rsp0: 0,
        rsp1: 0,
        rsp2: 0,
        reserved1: 0,
        ist: [0; 7],
        reserved2: 0,
        reserved3: 0,
        io_bitmap_offset: size_of::<Self>() as u16,
    };
}

#[repr(C, align(16))]
#[cfg(target_os = "none")]
struct GlobalDescriptorTable {
    entries: [u64; GDT_ENTRIES],
}

#[repr(C, align(16))]
#[cfg(target_os = "none")]
struct KernelEntryStack {
    bytes: [u8; KERNEL_ENTRY_STACK_BYTES],
}

#[repr(C, packed)]
#[cfg(target_os = "none")]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[cfg(target_os = "none")]
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable {
    entries: [0; GDT_ENTRIES],
};
#[cfg(target_os = "none")]
static mut TSS: TaskStateSegment = TaskStateSegment::EMPTY;
#[cfg(target_os = "none")]
static mut KERNEL_ENTRY_STACK: KernelEntryStack = KernelEntryStack {
    bytes: [0; KERNEL_ENTRY_STACK_BYTES],
};

#[cfg(target_os = "none")]
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "none")]
static PROBE_RETURNED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrivilegeInfo {
    pub kernel_stack_top: usize,
    pub user_code_selector: u16,
    pub user_data_selector: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivilegeError {
    AlreadyInitialized,
    NotInitialized,
    InvalidAddress,
    InvalidPageTableRoot,
    ProbeBusy,
    ProbeDidNotReturn,
    DescriptorLoadFailed,
    SyscallUnavailable,
    SyscallConfigurationFailed,
    UnsupportedHost,
}

/// Installs a higher-half GDT and a 64-bit TSS with a dedicated RSP0 stack.
///
/// # Safety
///
/// This must run once on the bootstrap CPU with interrupts disabled. The
/// higher-half kernel image, GDT, TSS, and entry stack must remain mapped in
/// every process address space that can execute on this CPU.
#[cfg(target_os = "none")]
pub unsafe fn initialize() -> Result<PrivilegeInfo, PrivilegeError> {
    if INITIALIZED.swap(true, Ordering::AcqRel) {
        return Err(PrivilegeError::AlreadyInitialized);
    }

    // SAFETY: Serialized initialization is the only code obtaining these raw
    // mutable addresses before hardware begins consuming the structures.
    let stack_base =
        unsafe { core::ptr::addr_of_mut!(KERNEL_ENTRY_STACK.bytes).cast::<u8>() as usize };
    let stack_top = stack_base + KERNEL_ENTRY_STACK_BYTES;
    let tss_pointer = core::ptr::addr_of_mut!(TSS);
    // SAFETY: Serialized bootstrap has exclusive access to these static
    // descriptor objects before LGDT/LTR publish them to hardware.
    unsafe {
        core::ptr::addr_of_mut!((*tss_pointer).rsp0).write_unaligned(stack_top as u64);
        core::ptr::addr_of_mut!((*tss_pointer).io_bitmap_offset)
            .write_unaligned(size_of::<TaskStateSegment>() as u16);
    }

    let (tss_low, tss_high) = tss_descriptor(tss_pointer as u64);
    // SAFETY: The GDT has not yet been published and bootstrap is serialized.
    let entries = unsafe { core::ptr::addr_of_mut!(GDT.entries).cast::<u64>() };
    // SAFETY: `entries` names all seven writable GDT slots exclusively owned
    // by this initialization phase.
    unsafe {
        entries.add(0).write(0);
        entries.add(1).write(0x00af_9a00_0000_ffff);
        entries.add(2).write(0x00cf_9200_0000_ffff);
        // SYSRET derives SS from STAR[63:48] + 8 and CS from +16. Keeping
        // user data immediately before user code makes those architectural
        // selectors exactly 0x1b and 0x23.
        entries.add(3).write(0x00cf_f200_0000_ffff);
        entries.add(4).write(0x00af_fa00_0000_ffff);
        entries.add(5).write(tss_low);
        entries.add(6).write(tss_high);
    }

    let pointer = DescriptorTablePointer {
        limit: (size_of::<GlobalDescriptorTable>() - 1) as u16,
        base: core::ptr::addr_of!(GDT) as u64,
    };
    // SAFETY: The descriptors and TSS were fully initialized above and remain
    // static for the lifetime of the kernel.
    unsafe {
        core::arch::asm!(
            "lgdt [{}]",
            "mov ax, {kernel_data:x}",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "xor eax, eax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ax, {tss:x}",
            "ltr ax",
            in(reg) &pointer,
            kernel_data = in(reg) KERNEL_DATA_SELECTOR,
            tss = in(reg) TSS_SELECTOR,
            out("ax") _,
            options(nostack),
        );
    }
    let loaded_tss: u16;
    // SAFETY: STR is a non-mutating check of the task register just loaded.
    unsafe {
        core::arch::asm!(
            "str {selector:x}",
            selector = out(reg) loaded_tss,
            options(nomem, nostack, preserves_flags),
        );
    }
    if loaded_tss != TSS_SELECTOR {
        INITIALIZED.store(false, Ordering::Release);
        return Err(PrivilegeError::DescriptorLoadFailed);
    }

    if let Err(error) = unsafe { configure_syscall_entry() } {
        INITIALIZED.store(false, Ordering::Release);
        return Err(error);
    }

    Ok(PrivilegeInfo {
        kernel_stack_top: stack_top,
        user_code_selector: USER_CODE_SELECTOR,
        user_data_selector: USER_DATA_SELECTOR,
    })
}

/// Enables the native SYSCALL/SYSRET gate for the bootstrap CPU.
///
/// # Safety
///
/// The GDT above must be active, `boulder_syscall_entry` and its dedicated
/// stack must remain mapped in every process address space, and callers must
/// serialize MSR programming on the target hardware thread.
#[cfg(target_os = "none")]
unsafe fn configure_syscall_entry() -> Result<(), PrivilegeError> {
    const EFER: u32 = 0xc000_0080;
    const STAR: u32 = 0xc000_0081;
    const LSTAR: u32 = 0xc000_0082;
    const FMASK: u32 = 0xc000_0084;
    const EFER_SCE: u64 = 1;
    const RFLAGS_TRAP: u64 = 1 << 8;
    const RFLAGS_INTERRUPT: u64 = 1 << 9;
    const RFLAGS_DIRECTION: u64 = 1 << 10;
    const RFLAGS_ALIGNMENT_CHECK: u64 = 1 << 18;

    let maximum_extended_leaf = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    if maximum_extended_leaf < 0x8000_0001
        || core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 11) == 0
    {
        return Err(PrivilegeError::SyscallUnavailable);
    }

    unsafe extern "C" {
        fn boulder_syscall_entry();
    }
    let entry_address = boulder_syscall_entry as *const () as usize as u64;
    let star = (u64::from(KERNEL_CODE_SELECTOR) << 32) | (0x10_u64 << 48);
    let mask = RFLAGS_TRAP | RFLAGS_INTERRUPT | RFLAGS_DIRECTION | RFLAGS_ALIGNMENT_CHECK;
    let efer = unsafe { super::read_msr(EFER) };
    unsafe {
        super::write_msr(STAR, star);
        super::write_msr(LSTAR, entry_address);
        super::write_msr(FMASK, mask);
        super::write_msr(EFER, efer | EFER_SCE);
    }

    if unsafe { super::read_msr(STAR) } != star
        || unsafe { super::read_msr(LSTAR) } != entry_address
        || unsafe { super::read_msr(FMASK) } != mask
        || unsafe { super::read_msr(EFER) } & EFER_SCE == 0
    {
        return Err(PrivilegeError::SyscallConfigurationFailed);
    }
    Ok(())
}

/// Reports that descriptor installation is unavailable in host tests.
///
/// # Safety
///
/// This has the same serialized initialization contract as the bare-metal
/// implementation.
#[cfg(not(target_os = "none"))]
pub unsafe fn initialize() -> Result<PrivilegeInfo, PrivilegeError> {
    Err(PrivilegeError::UnsupportedHost)
}

/// Transfers into a measured user entry point and requires a user breakpoint
/// to return through the TSS RSP0 stack.
///
/// # Safety
///
/// `page_table_root` must own valid user mappings for `entry_point` and the
/// stack below `stack_pointer`, while inheriting all active higher-half kernel
/// mappings. Interrupts and scheduling must remain disabled for the complete
/// transfer, and the caller must retain the process hierarchy until return.
#[cfg(target_os = "none")]
pub unsafe fn run_user_probe(
    entry_point: usize,
    stack_pointer: usize,
    page_table_root: u64,
) -> Result<(), PrivilegeError> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return Err(PrivilegeError::NotInitialized);
    }
    if !(0x1000..USER_ADDRESS_LIMIT).contains(&entry_point)
        || !(0x1000..USER_ADDRESS_LIMIT).contains(&stack_pointer)
    {
        return Err(PrivilegeError::InvalidAddress);
    }
    if page_table_root == 0 || page_table_root & 0xfff != 0 {
        return Err(PrivilegeError::InvalidPageTableRoot);
    }
    if PROBE_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(PrivilegeError::ProbeBusy);
    }
    PROBE_RETURNED.store(false, Ordering::Release);

    unsafe extern "C" {
        fn boulder_enter_ring3_probe(
            entry_point: usize,
            stack_pointer: usize,
            page_table_root: u64,
        );
    }
    // SAFETY: The caller and validation above establish the assembly entry
    // contract. The user breakpoint handler redirects return to the saved
    // kernel continuation before this call completes.
    unsafe { boulder_enter_ring3_probe(entry_point, stack_pointer, page_table_root) };

    PROBE_ACTIVE.store(false, Ordering::Release);
    if PROBE_RETURNED.swap(false, Ordering::AcqRel) {
        Ok(())
    } else {
        Err(PrivilegeError::ProbeDidNotReturn)
    }
}

/// Reports that a hardware privilege transfer is unavailable in host tests.
///
/// # Safety
///
/// Callers must still satisfy the bare-metal address-space contract so this
/// fallback cannot weaken call-site reasoning.
#[cfg(not(target_os = "none"))]
pub unsafe fn run_user_probe(
    _entry_point: usize,
    _stack_pointer: usize,
    _page_table_root: u64,
) -> Result<(), PrivilegeError> {
    Err(PrivilegeError::UnsupportedHost)
}

/// Marks an active Ring 3 probe ready for the assembly return trampoline.
pub fn complete_user_probe() -> bool {
    if !PROBE_ACTIVE.swap(false, Ordering::AcqRel) {
        return false;
    }

    #[cfg(target_os = "none")]
    {
        PROBE_RETURNED.store(true, Ordering::Release);
        true
    }

    #[cfg(not(target_os = "none"))]
    {
        false
    }
}

#[cfg(any(target_os = "none", test))]
const fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (size_of::<TaskStateSegment>() - 1) as u64;
    let low = (limit & 0xffff)
        | ((base & 0x00ff_ffff) << 16)
        | (0x89_u64 << 40)
        | (((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    (low, base >> 32)
}

const _: () = assert!(size_of::<TaskStateSegment>() == 104);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_full_width_available_tss_descriptor() {
        let base = 0xffff_ffff_8123_4000;
        let (low, high) = tss_descriptor(base);
        assert_eq!((low >> 40) & 0xff, 0x89);
        assert_eq!(low & 0xffff, 103);
        assert_eq!((low >> 16) & 0x00ff_ffff, base & 0x00ff_ffff);
        assert_eq!((low >> 56) & 0xff, (base >> 24) & 0xff);
        assert_eq!(high, base >> 32);
    }

    #[test]
    fn user_selectors_have_ring_three_request_privilege() {
        assert_eq!(USER_CODE_SELECTOR & 3, 3);
        assert_eq!(USER_DATA_SELECTOR & 3, 3);
        assert_eq!(KERNEL_CODE_SELECTOR & 3, 0);
    }

    #[test]
    fn user_selectors_have_the_sysret_required_order() {
        assert_eq!(USER_DATA_SELECTOR, 0x1b);
        assert_eq!(USER_CODE_SELECTOR, USER_DATA_SELECTOR + 8);
    }
}
