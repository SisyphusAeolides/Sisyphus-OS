use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use super::cpu_local::CpuLocalError;
#[cfg(target_os = "none")]
use super::cpu_local::{self, CpuRegistration};
use crate::ring_authority::CommittedTransition;
#[cfg(target_os = "none")]
use crate::ring_authority::{PrivilegeRing, TransitionGate};

pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_DATA_SELECTOR: u16 = 0x1b;
pub const USER_CODE_SELECTOR: u16 = 0x23;
#[cfg(target_os = "none")]
const TSS_SELECTOR: u16 = 0x28;
#[cfg(target_os = "none")]
const GDT_ENTRIES: usize = 7;
#[cfg(target_os = "none")]
const USER_ADDRESS_LIMIT: usize = 0x0000_8000_0000_0000;
pub(crate) const DOUBLE_FAULT_IST_INDEX: u8 = 1;
pub(crate) const NMI_IST_INDEX: u8 = 2;
pub(crate) const MACHINE_CHECK_IST_INDEX: u8 = 3;
#[cfg(any(target_os = "none", test))]
const FAULT_STACK_BYTES: usize = 16 * 1024;
#[cfg(any(target_os = "none", test))]
const FAULT_STACK_ALIGNMENT: usize = 4096;

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

#[repr(C, align(4096))]
#[cfg(target_os = "none")]
struct FaultStack {
    bytes: [u8; FAULT_STACK_BYTES],
}

#[cfg(target_os = "none")]
impl FaultStack {
    const EMPTY: Self = Self {
        bytes: [0; FAULT_STACK_BYTES],
    };
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
static mut DOUBLE_FAULT_STACK: FaultStack = FaultStack::EMPTY;
#[cfg(target_os = "none")]
static mut NMI_STACK: FaultStack = FaultStack::EMPTY;
#[cfg(target_os = "none")]
static mut MACHINE_CHECK_STACK: FaultStack = FaultStack::EMPTY;
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
    pub apic_id: u32,
    pub logical_cpu_id: u32,
    pub cpu_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultStackBounds {
    pub base: u64,
    pub top: u64,
    pub ist_index: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultStackInfo {
    pub double_fault: FaultStackBounds,
    pub non_maskable_interrupt: FaultStackBounds,
    pub machine_check: FaultStackBounds,
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
    InvalidFaultStack,
    OverlappingFaultStacks,
    FaultStackBindingMismatch,
    InvalidRingTransition,
    CpuLocal(CpuLocalError),
    UnsupportedHost,
}

/// Installs a higher-half GDT and a 64-bit TSS with dedicated RSP0, #DF, NMI,
/// and #MC stacks.
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

    // The current boot path brings up only the BSP. Logical identity zero is
    // therefore an explicit bootstrap assignment, not an SMP-online claim.
    let apic_id = super::current_hardware_thread_id();
    let logical_cpu_id = 0;
    let stack_top = match cpu_local::entry_stack_top(logical_cpu_id) {
        Ok(stack) => stack as usize,
        Err(error) => {
            INITIALIZED.store(false, Ordering::Release);
            return Err(PrivilegeError::CpuLocal(error));
        }
    };
    let tss_pointer = core::ptr::addr_of_mut!(TSS);
    let fault_stacks = match fault_stack_info() {
        Ok(info) => info,
        Err(error) => {
            INITIALIZED.store(false, Ordering::Release);
            return Err(error);
        }
    };
    // SAFETY: Serialized bootstrap has exclusive access to these static
    // descriptor objects before LGDT/LTR publish them to hardware.
    unsafe {
        core::ptr::addr_of_mut!((*tss_pointer).rsp0).write_unaligned(stack_top as u64);
        core::ptr::addr_of_mut!((*tss_pointer).ist)
            .cast::<u64>()
            .add(usize::from(DOUBLE_FAULT_IST_INDEX - 1))
            .write_unaligned(fault_stacks.double_fault.top);
        core::ptr::addr_of_mut!((*tss_pointer).ist)
            .cast::<u64>()
            .add(usize::from(NMI_IST_INDEX - 1))
            .write_unaligned(fault_stacks.non_maskable_interrupt.top);
        core::ptr::addr_of_mut!((*tss_pointer).ist)
            .cast::<u64>()
            .add(usize::from(MACHINE_CHECK_IST_INDEX - 1))
            .write_unaligned(fault_stacks.machine_check.top);
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

    // SAFETY: This CPU has just loaded the exact static TSS pointer and RSP0
    // registered here; syscall MSRs are still disabled during publication.
    let registration = match unsafe {
        cpu_local::register_current_cpu(
            apic_id,
            logical_cpu_id,
            tss_pointer as u64,
            stack_top as u64,
        )
    } {
        Ok(registration) => registration,
        Err(error) => {
            INITIALIZED.store(false, Ordering::Release);
            return Err(PrivilegeError::CpuLocal(error));
        }
    };

    if let Err(error) = unsafe { configure_syscall_entry(registration) } {
        let _ = cpu_local::revoke(registration);
        INITIALIZED.store(false, Ordering::Release);
        return Err(error);
    }

    Ok(PrivilegeInfo {
        kernel_stack_top: stack_top,
        user_code_selector: USER_CODE_SELECTOR,
        user_data_selector: USER_DATA_SELECTOR,
        apic_id,
        logical_cpu_id,
        cpu_generation: registration.generation,
    })
}

#[cfg(target_os = "none")]
fn fault_stack_info() -> Result<FaultStackInfo, PrivilegeError> {
    let info = FaultStackInfo {
        double_fault: stack_bounds(
            core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK).cast::<u8>() as u64,
            DOUBLE_FAULT_IST_INDEX,
        )?,
        non_maskable_interrupt: stack_bounds(
            core::ptr::addr_of_mut!(NMI_STACK).cast::<u8>() as u64,
            NMI_IST_INDEX,
        )?,
        machine_check: stack_bounds(
            core::ptr::addr_of_mut!(MACHINE_CHECK_STACK).cast::<u8>() as u64,
            MACHINE_CHECK_IST_INDEX,
        )?,
    };
    validate_fault_stack_layout(info)?;
    Ok(info)
}

#[cfg(any(target_os = "none", test))]
fn stack_bounds(base: u64, ist_index: u8) -> Result<FaultStackBounds, PrivilegeError> {
    let Some(top) = base.checked_add(FAULT_STACK_BYTES as u64) else {
        return Err(PrivilegeError::InvalidFaultStack);
    };
    Ok(FaultStackBounds {
        base,
        top,
        ist_index,
    })
}

#[cfg(any(target_os = "none", test))]
fn validate_fault_stack_layout(info: FaultStackInfo) -> Result<(), PrivilegeError> {
    let stacks = [
        info.double_fault,
        info.non_maskable_interrupt,
        info.machine_check,
    ];
    let expected_indices = [
        DOUBLE_FAULT_IST_INDEX,
        NMI_IST_INDEX,
        MACHINE_CHECK_IST_INDEX,
    ];
    for (stack, expected_index) in stacks.into_iter().zip(expected_indices) {
        if stack.ist_index != expected_index
            || stack.base == 0
            || stack.base & (FAULT_STACK_ALIGNMENT as u64 - 1) != 0
            || stack.top & 0xf != 0
            || stack.top.checked_sub(stack.base) != Some(FAULT_STACK_BYTES as u64)
            || !is_canonical_address(stack.base)
            || !is_canonical_address(stack.top - 1)
        {
            return Err(PrivilegeError::InvalidFaultStack);
        }
    }
    for left in 0..stacks.len() {
        for right in left + 1..stacks.len() {
            if stacks[left].base < stacks[right].top && stacks[right].base < stacks[left].top {
                return Err(PrivilegeError::OverlappingFaultStacks);
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "none", test))]
const fn is_canonical_address(address: u64) -> bool {
    let upper = address >> 48;
    if address & (1 << 47) == 0 {
        upper == 0
    } else {
        upper == 0xffff
    }
}

/// Verifies that the active BSP TSS still names the three immutable fault stacks.
#[cfg(target_os = "none")]
pub(crate) fn validate_ist_bindings() -> Result<FaultStackInfo, PrivilegeError> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return Err(PrivilegeError::NotInitialized);
    }
    let loaded_tss: u16;
    let mut loaded_gdt = DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: STR and SGDT only snapshot descriptor state on the current CPU.
    unsafe {
        core::arch::asm!(
            "str {selector:x}",
            "sgdt [{gdt}]",
            selector = out(reg) loaded_tss,
            gdt = in(reg) &mut loaded_gdt,
            options(nostack, preserves_flags),
        );
    }
    let loaded_gdt_pointer = core::ptr::addr_of!(loaded_gdt);
    let loaded_gdt_limit =
        unsafe { core::ptr::addr_of!((*loaded_gdt_pointer).limit).read_unaligned() };
    let loaded_gdt_base =
        unsafe { core::ptr::addr_of!((*loaded_gdt_pointer).base).read_unaligned() };
    if loaded_tss != TSS_SELECTOR
        || loaded_gdt_limit != (size_of::<GlobalDescriptorTable>() - 1) as u16
        || loaded_gdt_base != core::ptr::addr_of!(GDT) as u64
    {
        return Err(PrivilegeError::DescriptorLoadFailed);
    }
    let tss = core::ptr::addr_of!(TSS);
    let expected_descriptor = tss_descriptor(tss as u64);
    let gdt_entries = core::ptr::addr_of!(GDT).cast::<u64>();
    let active_descriptor = unsafe { (gdt_entries.add(5).read(), gdt_entries.add(6).read()) };
    // LTR atomically changes an available 64-bit TSS descriptor (type 9) to
    // busy (type 11), so the active image must differ by exactly that bit.
    let expected_busy_descriptor = (expected_descriptor.0 | (1 << 41), expected_descriptor.1);
    if active_descriptor != expected_busy_descriptor {
        return Err(PrivilegeError::DescriptorLoadFailed);
    }
    let info = fault_stack_info()?;
    // SAFETY: `tss` is the address of the live static TSS; `addr_of!` forms a
    // raw field pointer without creating a reference to the packed object.
    let ist = unsafe { core::ptr::addr_of!((*tss).ist).cast::<u64>() };
    // SAFETY: The static TSS is live and immutable after serialized privilege
    // initialization; packed fields are read without assuming alignment.
    let bindings = unsafe {
        [
            ist.add(usize::from(DOUBLE_FAULT_IST_INDEX - 1))
                .read_unaligned(),
            ist.add(usize::from(NMI_IST_INDEX - 1)).read_unaligned(),
            ist.add(usize::from(MACHINE_CHECK_IST_INDEX - 1))
                .read_unaligned(),
        ]
    };
    validate_fault_stack_bindings(info, bindings)?;
    Ok(info)
}

#[cfg(any(target_os = "none", test))]
fn validate_fault_stack_bindings(
    info: FaultStackInfo,
    bindings: [u64; 3],
) -> Result<(), PrivilegeError> {
    if bindings
        == [
            info.double_fault.top,
            info.non_maskable_interrupt.top,
            info.machine_check.top,
        ]
    {
        Ok(())
    } else {
        Err(PrivilegeError::FaultStackBindingMismatch)
    }
}

#[cfg(any(target_os = "none", test))]
fn fault_stack_range_is_contained(
    info: FaultStackInfo,
    ist_index: u8,
    start: u64,
    end: u64,
) -> bool {
    let stack = match ist_index {
        DOUBLE_FAULT_IST_INDEX => info.double_fault,
        NMI_IST_INDEX => info.non_maskable_interrupt,
        MACHINE_CHECK_IST_INDEX => info.machine_check,
        _ => return false,
    };
    start >= stack.base && start < end && end <= stack.top
}

#[cfg(target_os = "none")]
pub(crate) fn active_ist_range_is_contained(ist_index: u8, start: u64, end: u64) -> bool {
    fault_stack_info().is_ok_and(|info| fault_stack_range_is_contained(info, ist_index, start, end))
}

#[cfg(not(target_os = "none"))]
pub(crate) fn active_ist_range_is_contained(_ist_index: u8, _start: u64, _end: u64) -> bool {
    false
}

#[cfg(not(target_os = "none"))]
pub(crate) fn validate_ist_bindings() -> Result<FaultStackInfo, PrivilegeError> {
    Err(PrivilegeError::UnsupportedHost)
}

/// Enables the native SYSCALL/SYSRET gate for the bootstrap CPU.
///
/// # Safety
///
/// The GDT above must be active, `boulder_syscall_entry` and its dedicated
/// stack must remain mapped in every process address space, and callers must
/// serialize MSR programming on the target hardware thread.
#[cfg(target_os = "none")]
unsafe fn configure_syscall_entry(registration: CpuRegistration) -> Result<(), PrivilegeError> {
    const EFER: u32 = 0xc000_0080;
    const STAR: u32 = 0xc000_0081;
    const LSTAR: u32 = 0xc000_0082;
    const FMASK: u32 = 0xc000_0084;
    const GS_BASE: u32 = 0xc000_0101;
    const KERNEL_GS_BASE: u32 = 0xc000_0102;
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
    let previous_star = unsafe { super::read_msr(STAR) };
    let previous_lstar = unsafe { super::read_msr(LSTAR) };
    let previous_fmask = unsafe { super::read_msr(FMASK) };
    let previous_gs_base = unsafe { super::read_msr(GS_BASE) };
    let previous_kernel_gs_base = unsafe { super::read_msr(KERNEL_GS_BASE) };
    unsafe {
        // User mode receives a null GS base. SWAPGS selects only the immutable
        // record registered for this hardware thread.
        super::write_msr(GS_BASE, 0);
        super::write_msr(KERNEL_GS_BASE, registration.record_pointer());
        super::write_msr(STAR, star);
        super::write_msr(LSTAR, entry_address);
        super::write_msr(FMASK, mask);
        super::write_msr(EFER, efer | EFER_SCE);
    }

    if unsafe { super::read_msr(GS_BASE) } != 0
        || unsafe { super::read_msr(KERNEL_GS_BASE) } != registration.record_pointer()
        || unsafe { super::read_msr(STAR) } != star
        || unsafe { super::read_msr(LSTAR) } != entry_address
        || unsafe { super::read_msr(FMASK) } != mask
        || unsafe { super::read_msr(EFER) } & EFER_SCE == 0
    {
        unsafe {
            super::write_msr(EFER, efer);
            super::write_msr(STAR, previous_star);
            super::write_msr(LSTAR, previous_lstar);
            super::write_msr(FMASK, previous_fmask);
            super::write_msr(GS_BASE, previous_gs_base);
            super::write_msr(KERNEL_GS_BASE, previous_kernel_gs_base);
        }
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

/// Permanently transfers the bootstrap CPU to a retained Ring 3 process.
///
/// On success this function does not return. The initial user RFLAGS enables
/// maskable interrupts; later interrupt entry uses the installed TSS RSP0.
///
/// # Safety
///
/// The caller must retain the complete process hierarchy and stack, ensure all
/// kernel mappings required by interrupts and syscalls are inherited, and
/// guarantee that no Rust value on the abandoned kernel stack requires drop.
#[cfg(target_os = "none")]
pub unsafe fn enter_user_process(
    entry_point: usize,
    stack_pointer: usize,
    transition: CommittedTransition,
) -> Result<(), PrivilegeError> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return Err(PrivilegeError::NotInitialized);
    }
    if !(0x1000..USER_ADDRESS_LIMIT).contains(&entry_point)
        || !(0x1000..USER_ADDRESS_LIMIT).contains(&stack_pointer)
    {
        return Err(PrivilegeError::InvalidAddress);
    }
    if transition.target_ring() != PrivilegeRing::User || transition.gate() != TransitionGate::Iretq
    {
        return Err(PrivilegeError::InvalidRingTransition);
    }
    let page_table_root = transition.address_space_root();
    if page_table_root == 0 || page_table_root & 0xfff != 0 {
        return Err(PrivilegeError::InvalidPageTableRoot);
    }

    unsafe extern "C" {
        fn boulder_enter_user_process(
            entry_point: usize,
            stack_pointer: usize,
            page_table_root: u64,
        ) -> !;
    }
    // SAFETY: Validation and the caller's retention contract establish the
    // assembly transfer requirements. IRETQ is the successful terminal path.
    unsafe { boulder_enter_user_process(entry_point, stack_pointer, page_table_root) }
}

/// Reports that persistent privilege transfer is unavailable in host tests.
///
/// # Safety
///
/// Callers must satisfy the bare-metal retention contract even when compiling
/// this fallback.
#[cfg(not(target_os = "none"))]
pub unsafe fn enter_user_process(
    _entry_point: usize,
    _stack_pointer: usize,
    _transition: CommittedTransition,
) -> Result<(), PrivilegeError> {
    Err(PrivilegeError::UnsupportedHost)
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

    fn test_fault_stacks(bases: [u64; 3]) -> FaultStackInfo {
        FaultStackInfo {
            double_fault: stack_bounds(bases[0], DOUBLE_FAULT_IST_INDEX).unwrap(),
            non_maskable_interrupt: stack_bounds(bases[1], NMI_IST_INDEX).unwrap(),
            machine_check: stack_bounds(bases[2], MACHINE_CHECK_IST_INDEX).unwrap(),
        }
    }

    #[test]
    fn accepts_three_aligned_bounded_non_overlapping_fault_stacks() {
        let info = test_fault_stacks([
            0xffff_ffff_9000_0000,
            0xffff_ffff_9000_4000,
            0xffff_ffff_9000_8000,
        ]);
        assert_eq!(validate_fault_stack_layout(info), Ok(()));
        assert_eq!(
            info.double_fault.top - info.double_fault.base,
            FAULT_STACK_BYTES as u64
        );
    }

    #[test]
    fn rejects_aliasing_misaligned_and_noncanonical_fault_stacks() {
        let aliased = test_fault_stacks([
            0xffff_ffff_9000_0000,
            0xffff_ffff_9000_0000,
            0xffff_ffff_9000_8000,
        ]);
        assert_eq!(
            validate_fault_stack_layout(aliased),
            Err(PrivilegeError::OverlappingFaultStacks)
        );

        let misaligned = test_fault_stacks([
            0xffff_ffff_9000_0008,
            0xffff_ffff_9000_4000,
            0xffff_ffff_9000_8000,
        ]);
        assert_eq!(
            validate_fault_stack_layout(misaligned),
            Err(PrivilegeError::InvalidFaultStack)
        );

        let noncanonical = test_fault_stacks([
            0x0000_8000_0000_0000,
            0xffff_ffff_9000_4000,
            0xffff_ffff_9000_8000,
        ]);
        assert_eq!(
            validate_fault_stack_layout(noncanonical),
            Err(PrivilegeError::InvalidFaultStack)
        );
    }

    #[test]
    fn rejects_wrong_fault_stack_authority_slot() {
        let mut info = test_fault_stacks([
            0xffff_ffff_9000_0000,
            0xffff_ffff_9000_4000,
            0xffff_ffff_9000_8000,
        ]);
        info.machine_check.ist_index = NMI_IST_INDEX;
        assert_eq!(
            validate_fault_stack_layout(info),
            Err(PrivilegeError::InvalidFaultStack)
        );
    }

    #[test]
    fn rejects_any_tss_binding_that_does_not_name_the_exact_stack_top() {
        let info = test_fault_stacks([
            0xffff_ffff_9000_0000,
            0xffff_ffff_9000_4000,
            0xffff_ffff_9000_8000,
        ]);
        let exact = [
            info.double_fault.top,
            info.non_maskable_interrupt.top,
            info.machine_check.top,
        ];
        assert_eq!(validate_fault_stack_bindings(info, exact), Ok(()));
        let mut wrong = exact;
        wrong[1] = wrong[0];
        assert_eq!(
            validate_fault_stack_bindings(info, wrong),
            Err(PrivilegeError::FaultStackBindingMismatch)
        );
    }

    #[test]
    fn containment_requires_the_complete_frame_to_stay_inside_one_stack() {
        let info = test_fault_stacks([
            0xffff_ffff_9000_0000,
            0xffff_ffff_9000_4000,
            0xffff_ffff_9000_8000,
        ]);
        assert!(fault_stack_range_is_contained(
            info,
            NMI_IST_INDEX,
            info.non_maskable_interrupt.top - 256,
            info.non_maskable_interrupt.top,
        ));
        assert!(!fault_stack_range_is_contained(
            info,
            NMI_IST_INDEX,
            info.non_maskable_interrupt.base - 1,
            info.non_maskable_interrupt.top,
        ));
        assert!(!fault_stack_range_is_contained(
            info,
            7,
            info.non_maskable_interrupt.base,
            info.non_maskable_interrupt.top,
        ));
    }
}
