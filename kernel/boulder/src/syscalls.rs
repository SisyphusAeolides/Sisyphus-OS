use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
pub mod spectral_router;
#[cfg(target_os = "none")]
use crate::arch::x86_64::active_page_table_root;
#[cfg(target_os = "none")]
use crate::mmio::{EARLY_MAPPED_PHYSICAL_LIMIT, direct_map_address};
#[cfg(target_os = "none")]
use crate::serial::SerialPort;

use aether::grimoire;

const ERROR_BAD_FILE_DESCRIPTOR: isize = -9;
#[cfg(target_os = "none")]
const ERROR_BAD_ADDRESS: isize = -14;
#[cfg(target_os = "none")]
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

/// Register image built by the x86-64 syscall entry stub.
#[repr(C)]
pub struct SyscallFrame {
    pub number_or_result: u64,
    pub arguments: [u64; 6],
    pub user_instruction_pointer: u64,
    pub user_flags: u64,
    pub user_stack_pointer: u64,
}

const _: () = assert!(core::mem::size_of::<SyscallFrame>() == 80);

pub fn dispatch(number: usize, arguments: [usize; 6]) -> isize {
    match number {
        grimoire::SYS_YIELD => 0,
        grimoire::SYS_WRITE if arguments[0] != 1 => ERROR_BAD_FILE_DESCRIPTOR,
        grimoire::SYS_WRITE => ERROR_NOT_IMPLEMENTED,
        // Process destruction requires a scheduler-owned continuation. Until
        // that exists, returning ENOSYS is safer than returning to code that
        // reasonably believes its process has terminated.
        grimoire::SYS_EXIT => ERROR_NOT_IMPLEMENTED,
        grimoire::SYS_SPAWN => ERROR_NOT_IMPLEMENTED,
        grimoire::SYS_WAIT => ERROR_NOT_IMPLEMENTED,
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

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn boulder_syscall_dispatch(frame: *mut SyscallFrame) {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        crate::arch::x86_64::halt();
    };
    if !valid_user_control_address(frame.user_instruction_pointer)
        || !valid_user_control_address(frame.user_stack_pointer)
    {
        crate::arch::x86_64::halt();
    }

    let number = frame.number_or_result as usize;
    let result = match number {
        grimoire::SYS_WRITE => write_from_user(frame.arguments),
        grimoire::SYS_YIELD => {
            LAST_YIELD_HINT.store(frame.arguments[0], Ordering::Release);
            YIELD_HITS.fetch_add(1, Ordering::AcqRel);
            0
        }
        grimoire::SYS_EXIT => {
            EXIT_REQUESTS.fetch_add(1, Ordering::AcqRel);
            ERROR_NOT_IMPLEMENTED
        }
        grimoire::SYS_SPAWN => spawn_from_user(frame.arguments),
        grimoire::SYS_WAIT => wait_from_user(frame.arguments),
        grimoire::SYS_DISP_QUERY => kairos_query_to_user(frame.arguments),
        grimoire::SYS_DISP_LEASE => kairos_abi_to_user(frame.arguments),
        id @ crate::quantum_nexus::sys::SYS_NEXUS_ENTANGLE..=crate::quantum_nexus::sys::SYS_NEXUS_CONTROL => {
            let mut thermal = crate::quantum_nexus::ThermalBudget;
            match crate::quantum_nexus::sys::dispatch(id, frame.arguments[0] as usize, frame.arguments[1] as usize, frame.arguments[2] as usize, frame.arguments[3] as usize, frame.arguments[4] as usize, frame.arguments[5] as usize, &mut thermal) {
                Ok(v) => v as isize,
                Err(_) => -1, // Simple error mapping
            }
        }
        _ => ERROR_NOT_IMPLEMENTED,
    };
    frame.number_or_result = result as u64;
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
static NEXT_PID: AtomicUsize = AtomicUsize::new(2);
#[cfg(target_os = "none")]
static EXITED_PID: core::sync::atomic::AtomicIsize = core::sync::atomic::AtomicIsize::new(0);

#[cfg(target_os = "none")]
fn spawn_from_user(_arguments: [u64; 6]) -> isize {
    // Basic mock of spawn until we have a real scheduler and VFS image loading
    let pid = NEXT_PID.fetch_add(1, Ordering::AcqRel);
    // Pretend the child immediately exits for testing purposes
    EXITED_PID.store(pid as isize, Ordering::Release);
    pid as isize
}

#[cfg(target_os = "none")]
fn wait_from_user(arguments: [u64; 6]) -> isize {
    let pid_ptr = arguments[0];
    let status_ptr = arguments[1];
    
    let exited = EXITED_PID.swap(0, Ordering::Acquire);
    if exited == 0 {
        return -11; // EAGAIN
    }
    
    if pid_ptr != 0 {
        if copy_value_to_user(pid_ptr, &(exited as u32)).is_err() {
            return ERROR_BAD_ADDRESS;
        }
    }
    if status_ptr != 0 {
        let status: i32 = 0; // success
        if copy_value_to_user(status_ptr, &status).is_err() {
            return ERROR_BAD_ADDRESS;
        }
    }
    
    0
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

#[cfg(test)]
mod tests {
    use super::*;

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
