use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "none")]
use crate::arch::x86_64::active_page_table_root;
#[cfg(target_os = "none")]
use crate::mmio::{EARLY_MAPPED_PHYSICAL_LIMIT, direct_map_address};
#[cfg(target_os = "none")]
use crate::serial::SerialPort;

pub const SYSCALL_WRITE: usize = 1;
pub const SYSCALL_EXIT: usize = 2;
pub const SYSCALL_YIELD: usize = 3;

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
const ENTRY_HUGE: u64 = 1 << 7;
#[cfg(target_os = "none")]
const MAXIMUM_WRITE_BYTES: usize = 256;
#[cfg(target_os = "none")]
const COM1: u16 = 0x3f8;

static YIELD_HITS: AtomicUsize = AtomicUsize::new(0);
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
        SYSCALL_YIELD => 0,
        SYSCALL_WRITE if arguments[0] != 1 => ERROR_BAD_FILE_DESCRIPTOR,
        SYSCALL_WRITE => ERROR_NOT_IMPLEMENTED,
        // Process destruction requires a scheduler-owned continuation. Until
        // that exists, returning ENOSYS is safer than returning to code that
        // reasonably believes its process has terminated.
        SYSCALL_EXIT => ERROR_NOT_IMPLEMENTED,
        _ => ERROR_NOT_IMPLEMENTED,
    }
}

pub fn yield_hits() -> usize {
    YIELD_HITS.load(Ordering::Acquire)
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
        SYSCALL_WRITE => write_from_user(frame.arguments),
        SYSCALL_YIELD => {
            YIELD_HITS.fetch_add(1, Ordering::AcqRel);
            0
        }
        SYSCALL_EXIT => {
            EXIT_REQUESTS.fetch_add(1, Ordering::AcqRel);
            ERROR_NOT_IMPLEMENTED
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
        if entry & ENTRY_HUGE != 0 {
            return Err(UserCopyError::HugePageUnsupported);
        }
        table = entry & PAGE_ADDRESS_MASK;
    }
    let leaf = read_entry(table, indices[3]).ok_or(UserCopyError::MissingMapping)?;
    if leaf & (ENTRY_PRESENT | ENTRY_USER) != ENTRY_PRESENT | ENTRY_USER {
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

    #[test]
    fn dispatch_exposes_only_implemented_non_pointer_work() {
        assert_eq!(dispatch(SYSCALL_YIELD, [0; 6]), 0);
        assert_eq!(dispatch(SYSCALL_EXIT, [0; 6]), ERROR_NOT_IMPLEMENTED);
        assert_eq!(dispatch(99, [0; 6]), ERROR_NOT_IMPLEMENTED);
        assert_eq!(
            dispatch(SYSCALL_WRITE, [2, 0, 0, 0, 0, 0]),
            ERROR_BAD_FILE_DESCRIPTOR
        );
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
}
