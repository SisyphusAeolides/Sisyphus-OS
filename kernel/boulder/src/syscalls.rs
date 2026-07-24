use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
pub mod spectral_router;
#[cfg(target_os = "none")]
use crate::arch::x86_64::{active_page_table_root, cpu_local, current_hardware_thread_id};
#[cfg(target_os = "none")]
use crate::mmio::{EARLY_MAPPED_PHYSICAL_LIMIT, direct_map_address};
use crate::process::context::{AuthorizedUserReturn, ContextError};
use crate::process::lifecycle::ScheduledProcess;
#[cfg(target_os = "none")]
use crate::process::{lifecycle::LifecycleError, preemption};
#[cfg(target_os = "none")]
use crate::serial::SerialPort;

use aether::grimoire;

const ERROR_BAD_FILE_DESCRIPTOR: isize = -9;
#[cfg(target_os = "none")]
const ERROR_BAD_ADDRESS: isize = -14;
const ERROR_INVALID_ARGUMENT: isize = -22;
const ERROR_NOT_IMPLEMENTED: isize = -38;
#[cfg(any(target_os = "none", test))]
const USER_ADDRESS_MINIMUM: u64 = 0x1000;
#[cfg(any(target_os = "none", test))]
const USER_ADDRESS_LIMIT: u64 = 0x0000_8000_0000_0000;
#[cfg(any(target_os = "none", test))]
const PAGE_SIZE: usize = 4096;
#[cfg(any(target_os = "none", test))]
const PAGE_ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
#[cfg(any(target_os = "none", test))]
const ENTRY_PRESENT: u64 = 1 << 0;
#[cfg(any(target_os = "none", test))]
const ENTRY_USER: u64 = 1 << 2;
#[cfg(any(target_os = "none", test))]
const ENTRY_WRITABLE: u64 = 1 << 1;
#[cfg(any(target_os = "none", test))]
const ENTRY_HUGE: u64 = 1 << 7;
#[cfg(target_os = "none")]
const MAXIMUM_WRITE_BYTES: usize = 256;
#[cfg(target_os = "none")]
const COM1: u16 = 0x3f8;

static YIELD_HITS: AtomicUsize = AtomicUsize::new(0);
static LAST_YIELD_HINT: AtomicU64 = AtomicU64::new(0);
static WRITE_HITS: AtomicUsize = AtomicUsize::new(0);
static EXIT_REQUESTS: AtomicUsize = AtomicUsize::new(0);

/// The syscall entry frame combines the complete user register image with the
/// generation and epoch authority selected by the lifecycle scheduler.
pub type SyscallFrame = AuthorizedUserReturn;

/// Replaces a syscall return frame with a lifecycle-selected process. The
/// caller must activate the returned CR3 and kernel stack before returning to
/// user mode.
pub fn install_scheduled_return(
    frame: &mut SyscallFrame,
    scheduled: ScheduledProcess,
) -> Result<AuthorizedUserReturn, ContextError> {
    let authority = scheduled.authorized_return();
    authority.validate()?;
    *frame = authority;
    Ok(authority)
}

pub fn dispatch(number: usize, arguments: [usize; 6]) -> isize {
    match number {
        grimoire::SYS_YIELD => 0,
        grimoire::SYS_WRITE if arguments[0] != 1 => ERROR_BAD_FILE_DESCRIPTOR,
        grimoire::SYS_WRITE => ERROR_NOT_IMPLEMENTED,
        // Host dispatch cannot own a real process address space or switch
        // privilege levels. Process lifecycle syscalls remain unavailable in
        // this host-only scalar entry point.
        grimoire::SYS_EXIT | grimoire::SYS_SPAWN | grimoire::SYS_WAIT => ERROR_NOT_IMPLEMENTED,
        grimoire::SYS_NEXUS_ENTANGLE | grimoire::SYS_NEXUS_STATS | grimoire::SYS_NEXUS_POLICY => {
            dispatch_scalar_nexus(number, arguments.map(|value| value as u64))
        }
        _ => ERROR_NOT_IMPLEMENTED,
    }
}

pub fn yield_hits() -> usize {
    YIELD_HITS.load(Ordering::Acquire)
}

pub fn last_yield_hint() -> u64 {
    LAST_YIELD_HINT.load(Ordering::Acquire)
}

pub fn write_hits() -> usize {
    WRITE_HITS.load(Ordering::Acquire)
}

pub fn exit_requests() -> usize {
    EXIT_REQUESTS.load(Ordering::Acquire)
}

fn dispatch_scalar_nexus(number: usize, arguments: [u64; 6]) -> isize {
    use aether::nexus_wire::{NexusCommand, NexusOpcode, NexusStatus};

    let (opcode, command_arguments, capability, sequence) = match number {
        grimoire::SYS_NEXUS_ENTANGLE => (
            NexusOpcode::Entangle,
            [arguments[0], arguments[1], arguments[2], arguments[3]],
            arguments[4],
            arguments[5],
        ),
        grimoire::SYS_NEXUS_STATS => (NexusOpcode::QueryStats, [0; 4], arguments[0], arguments[1]),
        grimoire::SYS_NEXUS_POLICY => {
            let opcode = match arguments[0] {
                0 => NexusOpcode::SetCollapseThreshold,
                1 => NexusOpcode::SetPriorityMass,
                2 => NexusOpcode::OfferKairos,
                _ => return ERROR_INVALID_ARGUMENT,
            };
            (opcode, [arguments[1], 0, 0, 0], arguments[2], arguments[3])
        }
        _ => return ERROR_NOT_IMPLEMENTED,
    };

    if capability == 0 || sequence == 0 {
        return ERROR_INVALID_ARGUMENT;
    }

    let command = NexusCommand::new(opcode, sequence, capability, command_arguments);
    let reply = crate::nexus_runtime::control(
        &command,
        <crate::arch::Active as crate::arch::Architecture>::counter_sample(),
    );

    let status = match reply.validate(sequence) {
        Ok(status) => status,
        Err(_) => return -74,
    };

    match status {
        NexusStatus::Ok => isize::try_from(reply.values[0]).unwrap_or(isize::MAX),
        NexusStatus::BadFrame => -74,
        NexusStatus::Denied => -13,
        NexusStatus::Expired => -62,
        NexusStatus::InvalidArgument => -22,
        NexusStatus::Capacity => -28,
        NexusStatus::ThermalThrottle => -11,
        NexusStatus::NotReady => -19,
        NexusStatus::InternalFault => -5,
    }
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn boulder_syscall_dispatch(frame: *mut SyscallFrame) {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        crate::arch::x86_64::halt();
    };
    if validate_active_machine_entry().is_err() {
        crate::arch::x86_64::halt();
    }
    if frame.dispatch.user.validate().is_err() {
        crate::arch::x86_64::halt();
    }

    let number = frame.dispatch.user.rax as usize;
    let arguments = frame.dispatch.user.syscall_arguments();
    let scheduled = match number {
        grimoire::SYS_YIELD => {
            LAST_YIELD_HINT.store(arguments[0], Ordering::Release);
            YIELD_HITS.fetch_add(1, Ordering::AcqRel);
            let scheduled = if let Some(ticket) = preemption::take_at_safe_point() {
                let mut saved = frame.dispatch.user;
                saved.set_syscall_result(0);
                match crate::process::lifecycle::schedule_preempt(saved, ticket.authority) {
                    Ok(scheduled) => {
                        report_timer_preemption_service(ticket);
                        Ok(scheduled)
                    }
                    Err(LifecycleError::StalePreemptionAuthority) => {
                        preemption::record_stale();
                        crate::process::lifecycle::schedule_yield(saved)
                    }
                    Err(error) => Err(error),
                }
            } else {
                crate::process::lifecycle::schedule_yield(frame.dispatch.user)
            };
            match scheduled {
                Ok(scheduled) => scheduled,
                Err(_) => crate::arch::x86_64::halt(),
            }
        }
        grimoire::SYS_EXIT => {
            EXIT_REQUESTS.fetch_add(1, Ordering::AcqRel);
            preemption::retire_superseded();
            match crate::process::lifecycle::schedule_exit(arguments[0] as isize) {
                Ok(crate::process::lifecycle::ScheduleDecision::User(scheduled)) => scheduled,
                Ok(crate::process::lifecycle::ScheduleDecision::Pid0(mut idle)) => loop {
                    // SAFETY: SYSCALL entered with interrupts masked and the
                    // inherited kernel mapping retains this frame. STI's
                    // one-instruction shadow makes HLT atomic with respect to
                    // maskable wakeups; CLI restores the serialized return
                    // boundary before lifecycle state is inspected.
                    unsafe {
                        core::arch::asm!("sti", "hlt", "cli", options(nostack));
                    }
                    match crate::process::lifecycle::schedule_from_pid0(idle) {
                        Ok(crate::process::lifecycle::ScheduleDecision::User(scheduled)) => {
                            break scheduled;
                        }
                        Ok(crate::process::lifecycle::ScheduleDecision::Pid0(next)) => idle = next,
                        Err(_) => crate::arch::x86_64::halt(),
                    }
                },
                Err(_) => {
                    crate::arch::x86_64::halt();
                }
            }
        }
        _ => {
            let result = match number {
                grimoire::SYS_WRITE => write_from_user(arguments),
                grimoire::SYS_SPAWN | grimoire::SYS_WAIT => ERROR_NOT_IMPLEMENTED,
                grimoire::SYS_DISP_QUERY => kairos_query_to_user(arguments),
                grimoire::SYS_DISP_LEASE => kairos_abi_to_user(arguments),
                grimoire::SYS_NEXUS_TELEMETRY => nexus_telemetry_to_user(arguments),
                grimoire::SYS_NEXUS_CONTROL => nexus_control_from_user(arguments),
                grimoire::SYS_NEXUS_ENTANGLE
                | grimoire::SYS_NEXUS_STATS
                | grimoire::SYS_NEXUS_POLICY => dispatch_scalar_nexus(number, arguments),
                _ => ERROR_NOT_IMPLEMENTED,
            };
            let mut saved = frame.dispatch.user;
            saved.set_syscall_result(result);
            let scheduled = if let Some(ticket) = preemption::take_at_safe_point() {
                match crate::process::lifecycle::schedule_preempt(saved, ticket.authority) {
                    Ok(scheduled) => {
                        report_timer_preemption_service(ticket);
                        Ok(scheduled)
                    }
                    Err(LifecycleError::StalePreemptionAuthority) => {
                        preemption::record_stale();
                        crate::process::lifecycle::resume_current(saved)
                    }
                    Err(error) => Err(error),
                }
            } else {
                crate::process::lifecycle::resume_current(saved)
            };
            match scheduled {
                Ok(scheduled) => scheduled,
                Err(_) => crate::arch::x86_64::halt(),
            }
        }
    };
    // Timer IRQ work is consumed outside interrupt context. One pass keeps
    // syscall-exit latency bounded while ensuring periodic requests have a
    // production safe-point caller.
    let _ = crate::nexus_deferred::run_deferred(1);
    if install_scheduled_return(frame, scheduled).is_err() {
        crate::arch::x86_64::halt();
    }
}

#[cfg(target_os = "none")]
fn report_timer_preemption_service(ticket: preemption::PreemptionTicket) {
    if preemption::record_serviced() {
        let statistics = preemption::statistics();
        let mut serial = unsafe { SerialPort::initialize(COM1) };
        let _ = core::fmt::Write::write_fmt(
            &mut serial,
            format_args!(
                "Boulder: timer preemption safe-point serviced pid={}:{} epoch={} tick={} irq={} published={} coalesced={}\n",
                ticket.authority.handle.pid,
                ticket.authority.handle.generation,
                ticket.authority.scheduler_epoch,
                ticket.requested_tick,
                statistics.irq_requests,
                statistics.published,
                statistics.coalesced,
            ),
        );
    }
}

#[cfg(any(target_os = "none", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TaskStateDescriptor {
    base: u64,
    limit: u64,
}

#[cfg(any(target_os = "none", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskStateDescriptorError {
    NotSystemSegment,
    NotTaskStateSegment,
    NotPresent,
    Truncated,
    InvalidBase,
}

#[cfg(target_os = "none")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MachineDispatchError {
    TaskState(TaskStateDescriptorError),
    CpuLocal(cpu_local::CpuLocalError),
    TaskStateWriteFailed,
}

/// Decodes an x86-64 16-byte available/active TSS descriptor. Keeping this
/// transformation pure makes the architecture boundary independently
/// testable without executing privileged instructions.
#[cfg(any(target_os = "none", test))]
fn decode_task_state_descriptor(
    low: u64,
    high: u64,
) -> Result<TaskStateDescriptor, TaskStateDescriptorError> {
    if low & (1 << 44) != 0 {
        return Err(TaskStateDescriptorError::NotSystemSegment);
    }
    if !matches!((low >> 40) & 0xf, 0x9 | 0xb) {
        return Err(TaskStateDescriptorError::NotTaskStateSegment);
    }
    if low & (1 << 47) == 0 {
        return Err(TaskStateDescriptorError::NotPresent);
    }

    let base = ((low >> 16) & 0xffff)
        | ((low >> 32) & 0xff) << 16
        | ((low >> 56) & 0xff) << 24
        | (high & 0xffff_ffff) << 32;
    let mut limit = (low & 0xffff) | ((low >> 48) & 0xf) << 16;
    if low & (1 << 55) != 0 {
        limit = (limit << 12) | 0xfff;
    }
    // A 64-bit TSS architecturally occupies 104 bytes. RSP0 lies near the
    // beginning, but accepting a shorter segment would bless malformed CPU
    // privilege state.
    if limit < 103 {
        return Err(TaskStateDescriptorError::Truncated);
    }
    if base < crate::process::context::KERNEL_ADDRESS_MINIMUM {
        return Err(TaskStateDescriptorError::InvalidBase);
    }
    Ok(TaskStateDescriptor { base, limit })
}

#[cfg(target_os = "none")]
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[cfg(target_os = "none")]
fn active_task_state_descriptor() -> Result<TaskStateDescriptor, TaskStateDescriptorError> {
    let mut table = DescriptorTablePointer { limit: 0, base: 0 };
    let selector: u16;
    // SAFETY: SGDT and STR only snapshot descriptor state already owned by
    // this CPU. The packed destination has the architectural ten-byte shape.
    unsafe {
        core::arch::asm!(
            "sgdt [{}]",
            in(reg) core::ptr::addr_of_mut!(table),
            options(nostack, preserves_flags),
        );
        core::arch::asm!(
            "str {selector:x}",
            selector = out(reg) selector,
            options(nomem, nostack, preserves_flags),
        );
    }
    if selector & 0x4 != 0 {
        return Err(TaskStateDescriptorError::NotTaskStateSegment);
    }
    let offset = usize::from(selector & !0x7);
    if offset
        .checked_add(15)
        .is_none_or(|last| last > usize::from(table.limit))
    {
        return Err(TaskStateDescriptorError::Truncated);
    }
    if table.base < crate::process::context::KERNEL_ADDRESS_MINIMUM {
        return Err(TaskStateDescriptorError::InvalidBase);
    }

    let descriptor = table.base as *const u8;
    // SAFETY: The loaded GDTR bounds check above proves both descriptor words
    // are inside the active GDT. The GDT is 8-byte aligned and immutable after
    // bootstrap publication.
    let low = unsafe { descriptor.add(offset).cast::<u64>().read_volatile() };
    let high = unsafe { descriptor.add(offset + 8).cast::<u64>().read_volatile() };
    decode_task_state_descriptor(low, high)
}

#[cfg(target_os = "none")]
fn task_state_rsp0(tss: TaskStateDescriptor) -> u64 {
    // SAFETY: Descriptor validation proves the complete architectural TSS is
    // present. Per-byte volatile reads preserve packed-field semantics without
    // imposing alignment Rust cannot derive from an arbitrary TSS descriptor.
    let mut bytes = [0_u8; 8];
    for (index, byte) in bytes.iter_mut().enumerate() {
        // SAFETY: RSP0 occupies bytes 4..12 of every validated 64-bit TSS.
        *byte = unsafe { (tss.base as *const u8).add(4 + index).read_volatile() };
    }
    u64::from_le_bytes(bytes)
}

#[cfg(target_os = "none")]
fn write_task_state_rsp0(tss: TaskStateDescriptor, rsp0: u64) {
    for (index, byte) in rsp0.to_le_bytes().into_iter().enumerate() {
        // SAFETY: RSP0 occupies bytes 4..12 of every validated 64-bit TSS.
        // Syscall entry has IF masked, so hardware cannot consume a partial
        // value before publication and readback complete.
        unsafe {
            (tss.base as *mut u8).add(4 + index).write_volatile(byte);
        }
    }
}

#[cfg(target_os = "none")]
fn validate_active_machine_entry() -> Result<(), MachineDispatchError> {
    let tss = active_task_state_descriptor().map_err(MachineDispatchError::TaskState)?;
    cpu_local::validate_machine_entry(current_hardware_thread_id(), tss.base, task_state_rsp0(tss))
        .map(|_| ())
        .map_err(MachineDispatchError::CpuLocal)
}

#[cfg(target_os = "none")]
fn activate_machine_dispatch(authority: AuthorizedUserReturn) -> Result<(), MachineDispatchError> {
    let tss = active_task_state_descriptor().map_err(MachineDispatchError::TaskState)?;
    let current_rsp0 = task_state_rsp0(tss);
    let stack = authority.dispatch.kernel_stack_pointer;
    let return_lease =
        cpu_local::prepare_return(current_hardware_thread_id(), tss.base, current_rsp0, stack)
            .map_err(MachineDispatchError::CpuLocal)?;
    // SAFETY: The lifecycle authority was revalidated immediately before this
    // call. CPU-local preparation proved this exact TSS and old RSP0 match the
    // entry generation. Interrupts remain masked across publication.
    write_task_state_rsp0(tss, stack);
    if task_state_rsp0(tss) != stack {
        return Err(MachineDispatchError::TaskStateWriteFailed);
    }
    cpu_local::commit_return(return_lease, tss.base, stack)
        .map_err(MachineDispatchError::CpuLocal)?;
    // SAFETY: The lifecycle and CPU-local generations are committed, the new
    // TSS RSP0 was read back, and the target hierarchy retains this entry
    // frame through its inherited higher-half kernel mapping.
    unsafe {
        core::arch::asm!(
            "mov cr3, {root}",
            root = in(reg) authority.dispatch.address_space_root,
            options(nostack, preserves_flags),
        );
    }
    Ok(())
}

/// Final generation-safe architecture activation called from the assembly
/// return gate. Any stale, malformed, or superseded authority halts rather
/// than returning through attacker-selected registers or address-space state.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn boulder_syscall_activate(frame: *const SyscallFrame) {
    let Some(authority) = (unsafe { frame.as_ref() }).copied() else {
        crate::arch::x86_64::halt();
    };
    let Ok(scheduled) = crate::process::lifecycle::authorize_user_return(authority) else {
        crate::arch::x86_64::halt();
    };
    if scheduled.authorized_return() != authority || activate_machine_dispatch(authority).is_err() {
        crate::arch::x86_64::halt();
    }
}

#[cfg(any(target_os = "none", test))]
const fn valid_user_control_address(address: u64) -> bool {
    address >= USER_ADDRESS_MINIMUM && address < USER_ADDRESS_LIMIT
}

#[cfg(target_os = "none")]
fn write_from_user(arguments: [u64; 6]) -> isize {
    if arguments[0] != 1 {
        return ERROR_BAD_FILE_DESCRIPTOR;
    }
    let Ok(length) = usize::try_from(arguments[2]) else {
        return ERROR_INVALID_ARGUMENT;
    };
    if length > MAXIMUM_WRITE_BYTES {
        return ERROR_INVALID_ARGUMENT;
    }

    let mut bytes = [0_u8; MAXIMUM_WRITE_BYTES];
    if copy_from_user(arguments[1], &mut bytes[..length]).is_err() {
        return ERROR_BAD_ADDRESS;
    }
    // SAFETY: The syscall gate serializes this bootstrap CPU and COM1 is the
    // kernel's established debug sink. User bytes are copied before I/O.
    let mut serial = unsafe { SerialPort::initialize(COM1) };
    serial.write_bytes(&bytes[..length]);
    WRITE_HITS.fetch_add(1, Ordering::AcqRel);
    length as isize
}

#[cfg(target_os = "none")]
fn copy_from_user(source: u64, target: &mut [u8]) -> Result<(), UserCopyError> {
    if target.is_empty() {
        return Ok(());
    }
    let end = source
        .checked_add(target.len() as u64)
        .ok_or(UserCopyError::InvalidRange)?;
    if source < USER_ADDRESS_MINIMUM || end > USER_ADDRESS_LIMIT {
        return Err(UserCopyError::InvalidRange);
    }

    // SAFETY: SYSCALL entered from the process whose hierarchy remains active
    // throughout this non-preemptible copy.
    let root = unsafe { active_page_table_root() };
    let mut copied = 0;
    while copied < target.len() {
        let user_address = source + copied as u64;
        let physical = translate_user_address(root, user_address, read_active_entry)?;
        let page_remaining = PAGE_SIZE - (user_address as usize & (PAGE_SIZE - 1));
        let length = core::cmp::min(page_remaining, target.len() - copied);
        let physical_end = physical
            .checked_add(length as u64)
            .ok_or(UserCopyError::UnmappedPhysicalMemory)?;
        if physical_end > EARLY_MAPPED_PHYSICAL_LIMIT {
            return Err(UserCopyError::UnmappedPhysicalMemory);
        }
        let source_pointer =
            direct_map_address(physical).ok_or(UserCopyError::UnmappedPhysicalMemory)? as *const u8;
        // SAFETY: The page walk verified a user-readable mapping, the direct
        // map covers this bounded physical span, and the local array cannot
        // overlap process memory.
        unsafe {
            core::ptr::copy_nonoverlapping(source_pointer, target.as_mut_ptr().add(copied), length);
        }
        copied += length;
    }
    Ok(())
}

#[cfg(target_os = "none")]
fn copy_to_user(target: u64, source: &[u8]) -> Result<(), UserCopyError> {
    if source.is_empty() {
        return Ok(());
    }
    let end = target
        .checked_add(source.len() as u64)
        .ok_or(UserCopyError::InvalidRange)?;
    if target < USER_ADDRESS_MINIMUM || end > USER_ADDRESS_LIMIT {
        return Err(UserCopyError::InvalidRange);
    }

    // SAFETY: The syscall gate retains the calling process hierarchy for the
    // duration of this bounded copy.
    let root = unsafe { active_page_table_root() };
    let mut copied = 0;
    while copied < source.len() {
        let user_address = target + copied as u64;
        let physical = translate_user_address_for_write(root, user_address, read_active_entry)?;
        let page_remaining = PAGE_SIZE - (user_address as usize & (PAGE_SIZE - 1));
        let length = core::cmp::min(page_remaining, source.len() - copied);
        let physical_end = physical
            .checked_add(length as u64)
            .ok_or(UserCopyError::UnmappedPhysicalMemory)?;
        if physical_end > EARLY_MAPPED_PHYSICAL_LIMIT {
            return Err(UserCopyError::UnmappedPhysicalMemory);
        }
        let target_pointer =
            direct_map_address(physical).ok_or(UserCopyError::UnmappedPhysicalMemory)? as *mut u8;
        // SAFETY: The page walk verified a user-writable mapping, the direct
        // map covers this span, and `source` is kernel-owned memory.
        unsafe {
            core::ptr::copy_nonoverlapping(source.as_ptr().add(copied), target_pointer, length);
        }
        copied += length;
    }
    Ok(())
}

#[cfg(target_os = "none")]
fn copy_value_to_user<T>(target: u64, value: &T) -> Result<(), UserCopyError> {
    // SAFETY: All callers use C wire structures whose padding is explicit and
    // initialized, so their complete object representation may be copied.
    let bytes = unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    };
    copy_to_user(target, bytes)
}

#[cfg(target_os = "none")]
fn kairos_query_to_user(arguments: [u64; 6]) -> isize {
    use ::kairos::wire::{RawCpuEntry, RawDomainEntry, RawTopologyHeader, RawTopologyReply};

    if arguments[1] != core::mem::size_of::<RawTopologyReply>() as u64 {
        return ERROR_INVALID_ARGUMENT;
    }
    let destination = arguments[0];
    let Ok(header) = crate::kairos::topology_header() else {
        return ERROR_NOT_IMPLEMENTED;
    };
    if copy_value_to_user(destination, &header).is_err() {
        return ERROR_BAD_ADDRESS;
    }

    let cpu_base = destination + core::mem::size_of::<RawTopologyHeader>() as u64;
    for index in 0..header.cpu_count as usize {
        let Ok(entry) = crate::kairos::cpu_entry(index) else {
            return ERROR_NOT_IMPLEMENTED;
        };
        let target = cpu_base + (index * core::mem::size_of::<RawCpuEntry>()) as u64;
        if copy_value_to_user(target, &entry).is_err() {
            return ERROR_BAD_ADDRESS;
        }
    }

    let domain_base = destination + core::mem::offset_of!(RawTopologyReply, domains) as u64;
    for index in 0..header.domain_count as usize {
        let Ok(entry) = crate::kairos::domain_entry(index) else {
            return ERROR_NOT_IMPLEMENTED;
        };
        let target = domain_base + (index * core::mem::size_of::<RawDomainEntry>()) as u64;
        if copy_value_to_user(target, &entry).is_err() {
            return ERROR_BAD_ADDRESS;
        }
    }
    0
}

#[cfg(target_os = "none")]
fn kairos_abi_to_user(arguments: [u64; 6]) -> isize {
    use ::kairos::wire::{AbiReply, AbiRequest};

    if arguments[1] != core::mem::size_of::<AbiRequest>() as u64
        || arguments[3] != core::mem::size_of::<AbiReply>() as u64
    {
        return ERROR_INVALID_ARGUMENT;
    }
    let mut bytes = [0_u8; core::mem::size_of::<AbiRequest>()];
    if copy_from_user(arguments[0], &mut bytes).is_err() {
        return ERROR_BAD_ADDRESS;
    }
    // SAFETY: The byte array contains exactly one fully initialized request;
    // every field accepts all integer bit patterns. Unaligned access is used
    // because the byte array itself has alignment one.
    let request = unsafe { bytes.as_ptr().cast::<AbiRequest>().read_unaligned() };
    let reply = crate::kairos::negotiate_request(request);
    if copy_value_to_user(arguments[2], &reply).is_err() {
        return ERROR_BAD_ADDRESS;
    }
    0
}

#[cfg(target_os = "none")]
fn read_active_entry(table: u64, index: usize) -> Option<u64> {
    if table & (PAGE_SIZE as u64 - 1) != 0 || index >= 512 {
        return None;
    }
    let offset = (index * core::mem::size_of::<u64>()) as u64;
    let physical = table.checked_add(offset)?;
    if physical.checked_add(8)? > EARLY_MAPPED_PHYSICAL_LIMIT {
        return None;
    }
    let pointer = direct_map_address(physical)? as *const u64;
    // SAFETY: The active root and all process page-table frames are retained,
    // page-aligned allocator-owned memory covered by the immutable direct map.
    Some(unsafe { pointer.read_volatile() })
}

#[cfg(any(target_os = "none", test))]
fn translate_user_address(
    root: u64,
    address: u64,
    read_entry: impl FnMut(u64, usize) -> Option<u64>,
) -> Result<u64, UserCopyError> {
    translate_user_address_with_access(root, address, false, read_entry)
}

#[cfg(any(target_os = "none", test))]
fn translate_user_address_for_write(
    root: u64,
    address: u64,
    read_entry: impl FnMut(u64, usize) -> Option<u64>,
) -> Result<u64, UserCopyError> {
    translate_user_address_with_access(root, address, true, read_entry)
}

#[cfg(any(target_os = "none", test))]
fn translate_user_address_with_access(
    root: u64,
    address: u64,
    write: bool,
    mut read_entry: impl FnMut(u64, usize) -> Option<u64>,
) -> Result<u64, UserCopyError> {
    if root == 0 || root & (PAGE_SIZE as u64 - 1) != 0 || !valid_user_control_address(address) {
        return Err(UserCopyError::InvalidRange);
    }
    let indices = [
        ((address >> 39) & 0x1ff) as usize,
        ((address >> 30) & 0x1ff) as usize,
        ((address >> 21) & 0x1ff) as usize,
        ((address >> 12) & 0x1ff) as usize,
    ];
    if indices[0] >= 256 {
        return Err(UserCopyError::InvalidRange);
    }

    let mut table = root;
    for index in &indices[..3] {
        let entry = read_entry(table, *index).ok_or(UserCopyError::MissingMapping)?;
        if entry & (ENTRY_PRESENT | ENTRY_USER) != ENTRY_PRESENT | ENTRY_USER {
            return Err(UserCopyError::PermissionDenied);
        }
        if write && entry & ENTRY_WRITABLE == 0 {
            return Err(UserCopyError::PermissionDenied);
        }
        if entry & ENTRY_HUGE != 0 {
            return Err(UserCopyError::HugePageUnsupported);
        }
        table = entry & PAGE_ADDRESS_MASK;
    }
    let leaf = read_entry(table, indices[3]).ok_or(UserCopyError::MissingMapping)?;
    if leaf & (ENTRY_PRESENT | ENTRY_USER) != ENTRY_PRESENT | ENTRY_USER {
        return Err(UserCopyError::PermissionDenied);
    }
    if write && leaf & ENTRY_WRITABLE == 0 {
        return Err(UserCopyError::PermissionDenied);
    }
    Ok((leaf & PAGE_ADDRESS_MASK) | (address & (PAGE_SIZE as u64 - 1)))
}

#[cfg(any(target_os = "none", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UserCopyError {
    InvalidRange,
    MissingMapping,
    PermissionDenied,
    HugePageUnsupported,
    #[cfg(target_os = "none")]
    UnmappedPhysicalMemory,
}

#[cfg(target_os = "none")]
fn nexus_control_from_user(arguments: [u64; 6]) -> isize {
    use crate::arch::{Active, Architecture};
    use aether::nexus_wire::{NexusCommand, NexusReply};

    if arguments[2] != core::mem::size_of::<NexusCommand>() as u64
        || arguments[3] != core::mem::size_of::<NexusReply>() as u64
    {
        return ERROR_INVALID_ARGUMENT;
    }

    let mut command = NexusCommand::ZERO;

    // SAFETY: NexusCommand contains only integer fields and initialized arrays.
    // The byte slice covers the complete 64-byte wire object.
    let command_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut command as *mut NexusCommand).cast::<u8>(),
            core::mem::size_of::<NexusCommand>(),
        )
    };

    if copy_from_user(arguments[0], command_bytes).is_err() {
        return ERROR_BAD_ADDRESS;
    }

    let wall_tick = Active::counter_sample();
    let reply = crate::nexus_runtime::control(&command, wall_tick);

    if copy_value_to_user(arguments[1], &reply).is_err() {
        return ERROR_BAD_ADDRESS;
    }

    0
}

#[cfg(target_os = "none")]
fn nexus_telemetry_to_user(arguments: [u64; 6]) -> isize {
    use crate::arch::{Active, Architecture};
    use aether::nexus_wire::NexusTelemetry;

    if arguments[1] != core::mem::size_of::<NexusTelemetry>() as u64 {
        return ERROR_INVALID_ARGUMENT;
    }

    let sequence = arguments[2];
    let telemetry = crate::nexus_runtime::telemetry(sequence, Active::counter_sample());

    if copy_value_to_user(arguments[0], &telemetry).is_err() {
        return ERROR_BAD_ADDRESS;
    }

    core::mem::size_of::<NexusTelemetry>() as isize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::context::{DispatchContext, SavedUserContext};
    use crate::process::lifecycle::ProcessHandle;

    #[test]
    fn scheduled_context_overwrites_the_complete_syscall_return_frame() {
        let mut frame = AuthorizedUserReturn::EMPTY;
        frame.dispatch.user = SavedUserContext::initial(0x2000, 0x8000);
        frame.dispatch.user.r15 = 1;
        frame.dispatch.user.rbx = 2;
        frame.dispatch.user.rax = grimoire::SYS_YIELD as u64;

        let mut next_user = SavedUserContext::initial(0x3000, 0x9000);
        next_user.r15 = 0x15;
        next_user.rbx = 0xb;
        let next = ScheduledProcess {
            handle: ProcessHandle {
                pid: 7,
                generation: 11,
            },
            context: DispatchContext {
                user: next_user,
                address_space_root: 0x4000,
                kernel_stack_pointer: 0xffff_8000_0000_8000,
            },
            scheduler_epoch: 19,
        };

        assert_eq!(
            install_scheduled_return(&mut frame, next),
            Ok(next.authorized_return())
        );
        assert_eq!(frame, next.authorized_return());
    }

    fn encoded_tss_descriptor(base: u64, limit: u32, present: bool, kind: u64) -> (u64, u64) {
        let mut low = u64::from(limit & 0xffff)
            | (base & 0xffff) << 16
            | ((base >> 16) & 0xff) << 32
            | (kind & 0xf) << 40
            | (u64::from(limit >> 16) & 0xf) << 48
            | ((base >> 24) & 0xff) << 56;
        if present {
            low |= 1 << 47;
        }
        (low, base >> 32)
    }

    #[test]
    fn task_state_descriptor_decode_rejects_non_tss_privilege_state() {
        let base = 0xffff_8000_1234_5000;
        let (low, high) = encoded_tss_descriptor(base, 103, true, 0xb);
        assert_eq!(
            decode_task_state_descriptor(low, high),
            Ok(TaskStateDescriptor { base, limit: 103 })
        );

        let (not_present, high) = encoded_tss_descriptor(base, 103, false, 0xb);
        assert_eq!(
            decode_task_state_descriptor(not_present, high),
            Err(TaskStateDescriptorError::NotPresent)
        );
        let (wrong_kind, high) = encoded_tss_descriptor(base, 103, true, 0x2);
        assert_eq!(
            decode_task_state_descriptor(wrong_kind, high),
            Err(TaskStateDescriptorError::NotTaskStateSegment)
        );
        let (truncated, high) = encoded_tss_descriptor(base, 11, true, 0x9);
        assert_eq!(
            decode_task_state_descriptor(truncated, high),
            Err(TaskStateDescriptorError::Truncated)
        );
    }

    fn mapped_entry(physical: u64) -> u64 {
        physical | ENTRY_PRESENT | ENTRY_USER
    }

    fn writable_entry(physical: u64) -> u64 {
        mapped_entry(physical) | ENTRY_WRITABLE
    }

    #[test]
    fn dispatch_exposes_only_implemented_non_pointer_work() {
        assert_eq!(dispatch(grimoire::SYS_YIELD, [0; 6]), 0);
        assert_eq!(dispatch(grimoire::SYS_EXIT, [0; 6]), ERROR_NOT_IMPLEMENTED);
        assert_eq!(dispatch(99, [0; 6]), ERROR_NOT_IMPLEMENTED);
        assert_eq!(
            dispatch(grimoire::SYS_WRITE, [2, 0, 0, 0, 0, 0]),
            ERROR_BAD_FILE_DESCRIPTOR
        );
    }

    #[test]
    fn yield_hint_storage_is_scalar_and_bounded_to_the_call() {
        LAST_YIELD_HINT.store(0x55aa, Ordering::Release);
        assert_eq!(last_yield_hint(), 0x55aa);
    }

    #[test]
    fn translates_a_user_page_through_all_four_levels() {
        let result = translate_user_address(0x1000, 0x1234, |table, index| match (table, index) {
            (0x1000, 0) => Some(mapped_entry(0x2000)),
            (0x2000, 0) => Some(mapped_entry(0x3000)),
            (0x3000, 0) => Some(mapped_entry(0x4000)),
            (0x4000, 1) => Some(mapped_entry(0x9000)),
            _ => None,
        });
        assert_eq!(result, Ok(0x9234));
    }

    #[test]
    fn rejects_supervisor_and_huge_page_paths() {
        let supervisor =
            translate_user_address(0x1000, 0x1000, |table, index| match (table, index) {
                (0x1000, 0) => Some(0x2000 | ENTRY_PRESENT),
                _ => None,
            });
        assert_eq!(supervisor, Err(UserCopyError::PermissionDenied));

        let huge = translate_user_address(0x1000, 0x1000, |table, index| match (table, index) {
            (0x1000, 0) => Some(mapped_entry(0x2000) | ENTRY_HUGE),
            _ => None,
        });
        assert_eq!(huge, Err(UserCopyError::HugePageUnsupported));
    }

    #[test]
    fn write_translation_requires_writable_hierarchy() {
        let read_only =
            translate_user_address_for_write(0x1000, 0x1000, |table, index| match (table, index) {
                (0x1000, 0) => Some(mapped_entry(0x2000)),
                (0x2000, 0) => Some(writable_entry(0x3000)),
                (0x3000, 0) => Some(writable_entry(0x4000)),
                (0x4000, 1) => Some(writable_entry(0x9000)),
                _ => None,
            });
        assert_eq!(read_only, Err(UserCopyError::PermissionDenied));

        let writable =
            translate_user_address_for_write(0x1000, 0x1234, |table, index| match (table, index) {
                (0x1000, 0) => Some(writable_entry(0x2000)),
                (0x2000, 0) => Some(writable_entry(0x3000)),
                (0x3000, 0) => Some(writable_entry(0x4000)),
                (0x4000, 1) => Some(writable_entry(0x9000)),
                _ => None,
            });
        assert_eq!(writable, Ok(0x9234));
    }
}
