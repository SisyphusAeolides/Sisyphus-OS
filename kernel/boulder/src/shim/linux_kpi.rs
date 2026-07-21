use core::ffi::{c_char, c_int, c_void};
use core::mem::{align_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use sisyphus_driver_abi::{KernelApi, LOG_ERROR, STATUS_OK};

const ALLOCATION_MAGIC: u64 = 0x4b50_4941_4c4c_4f43;
const MAXIMUM_LOG_MESSAGE: usize = 4096;

#[repr(C, align(16))]
struct AllocationHeader {
    magic: u64,
    allocation_size: usize,
    allocation_alignment: usize,
    flags: u64,
}

static KERNEL_API: AtomicPtr<KernelApi> = AtomicPtr::new(ptr::null_mut());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallError {
    MissingAllocationService,
    AlreadyInstalled,
}

/// Installs the API table used by native compatibility entry points.
///
/// # Safety
///
/// `api` and every service reachable through it must remain valid until the
/// compatibility layer is uninstalled, after all modules have stopped.
pub unsafe fn install(api: &'static KernelApi) -> Result<(), InstallError> {
    if api.alloc.is_none() || api.dealloc.is_none() {
        return Err(InstallError::MissingAllocationService);
    }
    KERNEL_API
        .compare_exchange(
            ptr::null_mut(),
            api as *const KernelApi as *mut KernelApi,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| InstallError::AlreadyInstalled)
}

/// Removes the installed API table after every dependent module has stopped.
///
/// # Safety
///
/// No compatibility entry point may be executing or called after this point.
pub unsafe fn uninstall() {
    KERNEL_API.store(ptr::null_mut(), Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "C" fn kmalloc(size: usize, flags: u32) -> *mut c_void {
    let Some(api) = installed_api() else {
        return ptr::null_mut();
    };
    let Some(allocate) = api.alloc else {
        return ptr::null_mut();
    };
    if size == 0 {
        return ptr::null_mut();
    }
    let Some(allocation_size) = size_of::<AllocationHeader>().checked_add(size) else {
        return ptr::null_mut();
    };
    let mut allocation = ptr::null_mut();
    let status = unsafe {
        allocate(
            api.kernel_context,
            allocation_size,
            align_of::<AllocationHeader>(),
            u64::from(flags),
            &mut allocation,
        )
    };
    if status != STATUS_OK || allocation.is_null() {
        return ptr::null_mut();
    }
    let header = allocation.cast::<AllocationHeader>();
    unsafe {
        header.write(AllocationHeader {
            magic: ALLOCATION_MAGIC,
            allocation_size,
            allocation_alignment: align_of::<AllocationHeader>(),
            flags: u64::from(flags),
        });
        header.add(1).cast::<c_void>()
    }
}

/// Releases memory returned by `kmalloc`.
///
/// # Safety
///
/// `pointer` must be null or a live pointer returned by this compatibility
/// layer, and no caller may use it after this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kfree(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    let Some(api) = installed_api() else {
        return;
    };
    let Some(deallocate) = api.dealloc else {
        return;
    };
    let header = unsafe { pointer.cast::<AllocationHeader>().sub(1) };
    // SAFETY: The function contract requires a live kmalloc allocation.
    let metadata = unsafe { header.read() };
    if metadata.magic != ALLOCATION_MAGIC {
        return;
    }
    unsafe { header.write_bytes(0, 1) };
    let _ = unsafe {
        deallocate(
            api.kernel_context,
            header.cast(),
            metadata.allocation_size,
            metadata.allocation_alignment,
        )
    };
}

/// Emits a literal, null-terminated format string through the kernel logger.
/// Format argument interpolation is intentionally not claimed by this subset.
///
/// # Safety
///
/// `format` must point to a readable null-terminated byte string no longer
/// than `MAXIMUM_LOG_MESSAGE` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn printk(format: *const c_char) -> c_int {
    let Some(api) = installed_api() else {
        return -1;
    };
    let Some(log) = api.log else {
        return -1;
    };
    if format.is_null() {
        return -1;
    }
    let mut length = 0;
    while length < MAXIMUM_LOG_MESSAGE {
        if unsafe { format.cast::<u8>().add(length).read() } == 0 {
            let status = unsafe { log(api.kernel_context, LOG_ERROR, format.cast::<u8>(), length) };
            return if status == STATUS_OK {
                length as c_int
            } else {
                -1
            };
        }
        length += 1;
    }
    -1
}

fn installed_api() -> Option<&'static KernelApi> {
    let pointer = KERNEL_API.load(Ordering::Acquire);
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { &*pointer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_header_preserves_required_alignment() {
        assert_eq!(align_of::<AllocationHeader>(), 16);
        assert_eq!(size_of::<AllocationHeader>() % 16, 0);
    }
}
