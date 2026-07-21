use core::ffi::c_void;
use core::mem::{align_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use sisyphus_driver_abi::{KernelApi, STATUS_OK};

pub type NtStatus = i32;
pub const STATUS_SUCCESS: NtStatus = 0;

const ALLOCATION_MAGIC: u64 = 0x4e54_504f_4f4c_414c;

#[repr(C, align(16))]
struct PoolHeader {
    magic: u64,
    allocation_size: usize,
    alignment: usize,
    pool_type: u32,
    tag: u32,
}

static KERNEL_API: AtomicPtr<KernelApi> = AtomicPtr::new(ptr::null_mut());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallError {
    MissingAllocationService,
    AlreadyInstalled,
}

/// Installs the service table used by the exported Win64 entry points.
///
/// # Safety
///
/// The API and all referenced services must remain valid until every Windows
/// personality module has stopped and `uninstall` has completed.
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

/// Removes the service table after all dependent calls have stopped.
///
/// # Safety
///
/// No exported Win64 entry point may execute concurrently or afterward.
pub unsafe fn uninstall() {
    KERNEL_API.store(ptr::null_mut(), Ordering::Release);
}

#[unsafe(export_name = "ExAllocatePoolWithTag")]
pub extern "win64" fn ex_allocate_pool_with_tag(
    pool_type: u32,
    number_of_bytes: usize,
    tag: u32,
) -> *mut c_void {
    let Some(api) = installed_api() else {
        return ptr::null_mut();
    };
    let Some(allocate) = api.alloc else {
        return ptr::null_mut();
    };
    if number_of_bytes == 0 {
        return ptr::null_mut();
    }
    let Some(allocation_size) = size_of::<PoolHeader>().checked_add(number_of_bytes) else {
        return ptr::null_mut();
    };
    let mut allocation = ptr::null_mut();
    let status = unsafe {
        allocate(
            api.kernel_context,
            allocation_size,
            align_of::<PoolHeader>(),
            u64::from(pool_type),
            &mut allocation,
        )
    };
    if status != STATUS_OK || allocation.is_null() {
        return ptr::null_mut();
    }
    let header = allocation.cast::<PoolHeader>();
    unsafe {
        header.write(PoolHeader {
            magic: ALLOCATION_MAGIC,
            allocation_size,
            alignment: align_of::<PoolHeader>(),
            pool_type,
            tag,
        });
        header.add(1).cast()
    }
}

/// Releases a live allocation returned by `ExAllocatePoolWithTag`.
///
/// # Safety
///
/// `pointer` must be null or a live pool allocation, and `tag` must match the
/// allocation tag supplied by the caller.
#[unsafe(export_name = "ExFreePoolWithTag")]
pub unsafe extern "win64" fn ex_free_pool_with_tag(pointer: *mut c_void, tag: u32) {
    if pointer.is_null() {
        return;
    }
    let Some(api) = installed_api() else {
        return;
    };
    let Some(deallocate) = api.dealloc else {
        return;
    };
    let header = unsafe { pointer.cast::<PoolHeader>().sub(1) };
    let metadata = unsafe { header.read() };
    if metadata.magic != ALLOCATION_MAGIC || metadata.tag != tag {
        return;
    }
    unsafe { header.write_bytes(0, 1) };
    let _ = unsafe {
        deallocate(
            api.kernel_context,
            header.cast(),
            metadata.allocation_size,
            metadata.alignment,
        )
    };
}

pub const fn nt_success(status: NtStatus) -> bool {
    status >= STATUS_SUCCESS
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
    fn pool_metadata_keeps_win64_allocations_aligned() {
        assert_eq!(align_of::<PoolHeader>(), 16);
        assert_eq!(size_of::<PoolHeader>() % 16, 0);
        assert!(nt_success(STATUS_SUCCESS));
        assert!(!nt_success(-1));
    }
}
