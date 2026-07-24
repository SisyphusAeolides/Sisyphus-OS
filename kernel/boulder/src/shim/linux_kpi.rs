use core::ffi::{c_char, c_int, c_void};
use core::mem::{align_of, size_of};
use core::ptr;
#[cfg(test)]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{compiler_fence, AtomicPtr, Ordering};

use sisyphus_driver_abi::{KernelApi, ABI_VERSION, CAP_ALLOC, CAP_LOG, LOG_ERROR, STATUS_OK};

const ALLOCATION_MAGIC: u64 = 0x4b50_4941_4c4c_4f43;
const MAXIMUM_LOG_MESSAGE: usize = 4096;
const GFP_ZERO: u32 = 0x100;
const ZERO_SIZE_ADDRESS: usize = 16;
const MAXIMUM_QUARANTINED_ALLOCATIONS: usize = 16;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct AllocationHeader {
    magic: u64,
    allocation_size: usize,
    allocation_alignment: usize,
    flags: u64,
}

static KERNEL_API: AtomicPtr<KernelApi> = AtomicPtr::new(ptr::null_mut());
static QUARANTINE: crate::sync::SpinLock<Quarantine> =
    crate::sync::SpinLock::new(Quarantine::new());

#[derive(Clone, Copy)]
struct RawAllocation {
    address: usize,
    size: usize,
    alignment: usize,
}

#[derive(Clone, Copy)]
enum QuarantineSlot {
    Free,
    Reserved(RawAllocation),
    Reclaim(RawAllocation),
}

struct Quarantine {
    slots: [QuarantineSlot; MAXIMUM_QUARANTINED_ALLOCATIONS],
}

impl Quarantine {
    const fn new() -> Self {
        Self {
            slots: [QuarantineSlot::Free; MAXIMUM_QUARANTINED_ALLOCATIONS],
        }
    }

    fn reserve(&mut self) -> Option<usize> {
        let index = self
            .slots
            .iter()
            .position(|slot| matches!(slot, QuarantineSlot::Free))?;
        self.slots[index] = QuarantineSlot::Reserved(RawAllocation {
            address: 0,
            size: 0,
            alignment: 0,
        });
        Some(index)
    }

    fn attach(&mut self, index: usize, allocation: RawAllocation) {
        if matches!(self.slots[index], QuarantineSlot::Reserved(_)) {
            self.slots[index] = QuarantineSlot::Reserved(allocation);
        }
    }

    fn quarantine(&mut self, index: usize) {
        if let QuarantineSlot::Reserved(allocation) = self.slots[index] {
            self.slots[index] = QuarantineSlot::Reclaim(allocation);
        }
    }

    fn release_reservation(&mut self, index: usize) {
        if matches!(self.slots[index], QuarantineSlot::Reserved(_)) {
            self.slots[index] = QuarantineSlot::Free;
        }
    }

    fn outstanding(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !matches!(slot, QuarantineSlot::Free))
            .count()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallError {
    IncompatibleApi,
    MissingAllocationService,
    MissingLogService,
    AlreadyInstalled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UninstallError {
    OperationsOrReclaimsOutstanding(usize),
}

/// Installs the API table used by native compatibility entry points.
///
/// # Safety
///
/// `api` and every service reachable through it must remain valid until the
/// compatibility layer is uninstalled, after all modules have stopped.
pub unsafe fn install(api: &'static KernelApi) -> Result<(), InstallError> {
    if api.abi_version != ABI_VERSION || api.struct_size < size_of::<KernelApi>() as u32 {
        return Err(InstallError::IncompatibleApi);
    }
    if api.capabilities & CAP_ALLOC == 0 || api.alloc.is_none() || api.dealloc.is_none() {
        return Err(InstallError::MissingAllocationService);
    }
    if api.capabilities & CAP_LOG == 0 || api.log.is_none() {
        return Err(InstallError::MissingLogService);
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
pub unsafe fn uninstall() -> Result<(), UninstallError> {
    if let Some(api) = installed_api() {
        retry_quarantined_with(api);
    }
    let outstanding = QUARANTINE.lock().outstanding();
    if outstanding != 0 {
        return Err(UninstallError::OperationsOrReclaimsOutstanding(outstanding));
    }
    KERNEL_API.store(ptr::null_mut(), Ordering::Release);
    Ok(())
}

/// Reports whether the complete implemented KPI subset has a live backend.
pub(crate) fn is_ready() -> bool {
    installed_api().is_some_and(api_supports_subset)
}

#[unsafe(no_mangle)]
pub extern "C" fn kmalloc(size: usize, flags: u32) -> *mut c_void {
    let Some(api) = installed_api() else {
        return ptr::null_mut();
    };
    let Some(allocate) = api.alloc else {
        return ptr::null_mut();
    };
    retry_quarantined_with(api);
    if size == 0 {
        return zero_size_pointer();
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
        let payload = header.add(1).cast::<c_void>();
        if flags & GFP_ZERO != 0 {
            payload.write_bytes(0, size);
        }
        payload
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
    if zero_or_null(pointer) {
        return;
    }
    let Some(api) = installed_api() else {
        return;
    };
    retry_quarantined_with(api);
    let Some(deallocate) = api.dealloc else {
        return;
    };
    let header = unsafe { pointer.cast::<AllocationHeader>().sub(1) };
    // SAFETY: The function contract requires a live kmalloc allocation.
    let metadata = unsafe { header.read() };
    if metadata.magic != ALLOCATION_MAGIC {
        return;
    }
    // The header remains intact unless the backend accepts the release. This
    // makes a transient failure retryable through the otherwise-void Linux API.
    let _ = unsafe {
        deallocate(
            api.kernel_context,
            header.cast(),
            metadata.allocation_size,
            metadata.allocation_alignment,
        )
    };
}

/// Reports the usable size of a live allocation from this compatibility heap.
///
/// # Safety
///
/// `pointer` must be null, the Linux zero-size sentinel, or a live pointer
/// returned by this compatibility layer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ksize(pointer: *const c_void) -> usize {
    allocation_metadata(pointer.cast_mut())
        .map(|(_, metadata)| metadata.allocation_size - size_of::<AllocationHeader>())
        .unwrap_or(0)
}

/// Reallocates an object while preserving the old object on every failure.
///
/// A replacement whose rollback also encounters a transient deallocation
/// failure is retained in the bounded quarantine and retried by later heap
/// operations or uninstall.
///
/// # Safety
///
/// `pointer` must be null, the Linux zero-size sentinel, or a live pointer
/// returned by this compatibility layer and exclusively owned by the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn krealloc(
    pointer: *const c_void,
    new_size: usize,
    flags: u32,
) -> *mut c_void {
    let pointer = pointer.cast_mut();
    if new_size == 0 {
        unsafe { kfree(pointer) };
        return zero_size_pointer();
    }
    if zero_or_null(pointer) {
        return kmalloc(new_size, flags);
    }
    let Some((old_header, old_metadata)) = allocation_metadata(pointer) else {
        return ptr::null_mut();
    };
    let old_size = old_metadata.allocation_size - size_of::<AllocationHeader>();
    if old_size >= new_size {
        return pointer;
    }

    let Some(quarantine_slot) = QUARANTINE.lock().reserve() else {
        return ptr::null_mut();
    };
    let replacement = kmalloc(new_size, flags);
    if replacement.is_null() || replacement == zero_size_pointer() {
        QUARANTINE.lock().release_reservation(quarantine_slot);
        return ptr::null_mut();
    }
    let replacement_header = unsafe { replacement.cast::<AllocationHeader>().sub(1) };
    let replacement_metadata = unsafe { replacement_header.read() };
    let replacement_raw = RawAllocation {
        address: replacement_header.addr(),
        size: replacement_metadata.allocation_size,
        alignment: replacement_metadata.allocation_alignment,
    };
    QUARANTINE.lock().attach(quarantine_slot, replacement_raw);

    unsafe { ptr::copy_nonoverlapping(pointer.cast::<u8>(), replacement.cast::<u8>(), old_size) };
    let Some(api) = installed_api() else {
        QUARANTINE.lock().quarantine(quarantine_slot);
        return ptr::null_mut();
    };
    if release_raw(api, raw_allocation(old_header, old_metadata)) {
        QUARANTINE.lock().release_reservation(quarantine_slot);
        replacement
    } else if release_raw(api, replacement_raw) {
        QUARANTINE.lock().release_reservation(quarantine_slot);
        ptr::null_mut()
    } else {
        QUARANTINE.lock().quarantine(quarantine_slot);
        ptr::null_mut()
    }
}

/// Duplicates exactly `length` bytes into the compatibility heap.
///
/// # Safety
///
/// `source` must identify `length` readable bytes when `length` is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kmemdup(source: *const c_void, length: usize, flags: u32) -> *mut c_void {
    if length != 0 && source.is_null() {
        return ptr::null_mut();
    }
    let destination = kmalloc(length, flags);
    if length != 0 && !destination.is_null() {
        unsafe { ptr::copy_nonoverlapping(source.cast::<u8>(), destination.cast::<u8>(), length) };
    }
    destination
}

/// Duplicates exactly `length` bytes and appends a null terminator.
///
/// # Safety
///
/// `source` must identify `length` readable bytes when it is non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kmemdup_nul(
    source: *const c_char,
    length: usize,
    flags: u32,
) -> *mut c_char {
    if source.is_null() {
        return ptr::null_mut();
    }
    let Some(allocation_size) = length.checked_add(1) else {
        return ptr::null_mut();
    };
    let destination = kmalloc(allocation_size, flags).cast::<c_char>();
    if destination.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(source.cast::<u8>(), destination.cast::<u8>(), length);
        destination.add(length).write(0);
    }
    destination
}

/// Explicitly clears the complete allocation before attempting to free it.
/// A transient deallocation failure leaves a valid, zeroed object that can be
/// passed to `kfree` again.
///
/// # Safety
///
/// `pointer` must be null, the Linux zero-size sentinel, or a live pointer
/// returned by this compatibility layer and exclusively owned by the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kfree_sensitive(pointer: *const c_void) {
    let pointer = pointer.cast_mut();
    if let Some((_, metadata)) = allocation_metadata(pointer) {
        let length = metadata.allocation_size - size_of::<AllocationHeader>();
        unsafe { pointer.write_bytes(0, length) };
        compiler_fence(Ordering::SeqCst);
    }
    unsafe { kfree(pointer) };
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

fn zero_size_pointer() -> *mut c_void {
    ptr::without_provenance_mut(ZERO_SIZE_ADDRESS)
}

fn zero_or_null(pointer: *mut c_void) -> bool {
    pointer.addr() <= ZERO_SIZE_ADDRESS
}

fn allocation_metadata(pointer: *mut c_void) -> Option<(*mut AllocationHeader, AllocationHeader)> {
    if zero_or_null(pointer) {
        return None;
    }
    let header = unsafe { pointer.cast::<AllocationHeader>().sub(1) };
    let metadata = unsafe { header.read() };
    (metadata.magic == ALLOCATION_MAGIC
        && metadata.allocation_alignment == align_of::<AllocationHeader>()
        && metadata.allocation_size >= size_of::<AllocationHeader>())
    .then_some((header, metadata))
}

fn raw_allocation(header: *mut AllocationHeader, metadata: AllocationHeader) -> RawAllocation {
    RawAllocation {
        address: header.addr(),
        size: metadata.allocation_size,
        alignment: metadata.allocation_alignment,
    }
}

fn release_raw(api: &KernelApi, allocation: RawAllocation) -> bool {
    let Some(deallocate) = api.dealloc else {
        return false;
    };
    let pointer = ptr::without_provenance_mut::<c_void>(allocation.address);
    unsafe {
        deallocate(
            api.kernel_context,
            pointer,
            allocation.size,
            allocation.alignment,
        ) == STATUS_OK
    }
}

fn retry_quarantined_with(api: &KernelApi) {
    for index in 0..MAXIMUM_QUARANTINED_ALLOCATIONS {
        let allocation = {
            let mut quarantine = QUARANTINE.lock();
            match quarantine.slots[index] {
                QuarantineSlot::Reclaim(allocation) => {
                    quarantine.slots[index] = QuarantineSlot::Reserved(allocation);
                    Some(allocation)
                }
                QuarantineSlot::Free | QuarantineSlot::Reserved(_) => None,
            }
        };
        let Some(allocation) = allocation else {
            continue;
        };
        let released = release_raw(api, allocation);
        let mut quarantine = QUARANTINE.lock();
        quarantine.slots[index] = if released {
            QuarantineSlot::Free
        } else {
            QuarantineSlot::Reclaim(allocation)
        };
    }
}

fn api_supports_subset(api: &KernelApi) -> bool {
    api.abi_version == ABI_VERSION
        && api.struct_size >= size_of::<KernelApi>() as u32
        && api.capabilities & (CAP_ALLOC | CAP_LOG) == (CAP_ALLOC | CAP_LOG)
        && api.alloc.is_some()
        && api.dealloc.is_some()
        && api.log.is_some()
}

#[cfg(test)]
pub(crate) static TEST_INSTALL_LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());
#[cfg(test)]
static TEST_DEALLOCATE_FAILURES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
unsafe extern "C" fn test_allocate(
    _kernel_context: *mut c_void,
    size: usize,
    alignment: usize,
    _flags: u64,
    out_pointer: *mut *mut c_void,
) -> sisyphus_driver_abi::Status {
    if out_pointer.is_null() {
        return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
    }
    let Ok(layout) = core::alloc::Layout::from_size_align(size, alignment) else {
        return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
    };
    let allocation = unsafe { alloc::alloc::alloc(layout) }.cast::<c_void>();
    if allocation.is_null() {
        return sisyphus_driver_abi::STATUS_NO_MEMORY;
    }
    unsafe { allocation.cast::<u8>().write_bytes(0xa5, size) };
    unsafe { out_pointer.write(allocation) };
    STATUS_OK
}

#[cfg(test)]
unsafe extern "C" fn test_deallocate(
    _kernel_context: *mut c_void,
    pointer: *mut c_void,
    size: usize,
    alignment: usize,
) -> sisyphus_driver_abi::Status {
    if TEST_DEALLOCATE_FAILURES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        return sisyphus_driver_abi::STATUS_BUSY;
    }
    let Ok(layout) = core::alloc::Layout::from_size_align(size, alignment) else {
        return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
    };
    if pointer.is_null() {
        return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
    }
    unsafe { alloc::alloc::dealloc(pointer.cast::<u8>(), layout) };
    STATUS_OK
}

#[cfg(test)]
unsafe extern "C" fn test_log(
    _kernel_context: *mut c_void,
    _level: u32,
    message: *const u8,
    message_len: usize,
) -> sisyphus_driver_abi::Status {
    if message_len != 0 && message.is_null() {
        sisyphus_driver_abi::STATUS_INVALID_ARGUMENT
    } else {
        STATUS_OK
    }
}

#[cfg(test)]
pub(crate) static TEST_KERNEL_API: KernelApi = KernelApi {
    abi_version: ABI_VERSION,
    struct_size: size_of::<KernelApi>() as u32,
    capabilities: CAP_ALLOC | CAP_LOG,
    kernel_context: ptr::null_mut(),
    log: Some(test_log),
    alloc: Some(test_allocate),
    dealloc: Some(test_deallocate),
    monotonic_ns: None,
    sleep_ns: None,
    mmio_map: None,
    mmio_unmap: None,
    dma_alloc: None,
    dma_free: None,
    irq_register: None,
    irq_set_enabled: None,
    irq_unregister: None,
    device_publish: None,
    device_remove: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    struct InstalledApi;

    impl Drop for InstalledApi {
        fn drop(&mut self) {
            let _ = unsafe { uninstall() };
        }
    }

    #[test]
    fn allocation_header_preserves_required_alignment() {
        assert_eq!(align_of::<AllocationHeader>(), 16);
        assert_eq!(size_of::<AllocationHeader>() % 16, 0);
    }

    #[test]
    fn install_and_uninstall_control_subset_readiness() {
        let _lock = TEST_INSTALL_LOCK.lock();
        let _ = unsafe { uninstall() };
        assert!(!is_ready());
        assert!(kmalloc(16, 0).is_null());
        assert_eq!(unsafe { printk(c"not ready".as_ptr()) }, -1);

        assert_eq!(unsafe { install(&TEST_KERNEL_API) }, Ok(()));
        let _installed = InstalledApi;
        assert!(is_ready());
        assert_eq!(
            unsafe { install(&TEST_KERNEL_API) },
            Err(InstallError::AlreadyInstalled)
        );

        let allocation = kmalloc(16, 0x25);
        assert!(!allocation.is_null());
        unsafe {
            allocation.cast::<u8>().write(0xa5);
            assert_eq!(allocation.cast::<u8>().read(), 0xa5);
            kfree(allocation);
        }
        assert_eq!(unsafe { printk(c"ready".as_ptr()) }, 5);

        let _ = unsafe { uninstall() };
        assert!(!is_ready());
        assert!(kmalloc(16, 0).is_null());
        assert_eq!(unsafe { printk(c"not ready".as_ptr()) }, -1);
    }

    #[test]
    fn incomplete_or_incompatible_apis_fail_closed() {
        let mut api = TEST_KERNEL_API;
        api.abi_version = ABI_VERSION.wrapping_add(1);
        assert!(!api_supports_subset(&api));

        api = TEST_KERNEL_API;
        api.struct_size = (size_of::<KernelApi>() - 1) as u32;
        assert!(!api_supports_subset(&api));

        api = TEST_KERNEL_API;
        api.capabilities &= !CAP_LOG;
        assert!(!api_supports_subset(&api));

        api = TEST_KERNEL_API;
        api.log = None;
        assert!(!api_supports_subset(&api));

        api = TEST_KERNEL_API;
        api.dealloc = None;
        assert!(!api_supports_subset(&api));
    }

    #[test]
    fn linux_heap_contract_zeroes_duplicates_and_reallocates_transactionally() {
        let _lock = TEST_INSTALL_LOCK.lock();
        let _ = unsafe { uninstall() };
        TEST_DEALLOCATE_FAILURES.store(0, Ordering::Release);
        assert_eq!(unsafe { install(&TEST_KERNEL_API) }, Ok(()));
        let _installed = InstalledApi;

        let empty = kmalloc(0, 0);
        assert_eq!(empty.addr(), ZERO_SIZE_ADDRESS);
        assert_eq!(unsafe { ksize(empty) }, 0);
        unsafe { kfree(empty) };

        let zeroed = kmalloc(24, GFP_ZERO);
        assert!(!zeroed.is_null());
        assert_eq!(unsafe { ksize(zeroed) }, 24);
        assert!(
            unsafe { core::slice::from_raw_parts(zeroed.cast::<u8>(), 24) }
                .iter()
                .all(|byte| *byte == 0)
        );

        let source = [1_u8, 2, 3, 4, 5];
        let duplicate = unsafe { kmemdup(source.as_ptr().cast(), source.len(), 0) };
        assert_eq!(
            unsafe { core::slice::from_raw_parts(duplicate.cast::<u8>(), source.len()) },
            source
        );
        let string = unsafe { kmemdup_nul(source.as_ptr().cast(), source.len(), 0) };
        assert_eq!(
            unsafe { core::slice::from_raw_parts(string.cast::<u8>(), source.len() + 1) },
            &[1, 2, 3, 4, 5, 0]
        );

        let grown = unsafe { krealloc(duplicate, 32, 0) };
        assert!(!grown.is_null());
        assert_eq!(unsafe { ksize(grown) }, 32);
        assert_eq!(
            unsafe { core::slice::from_raw_parts(grown.cast::<u8>(), source.len()) },
            source
        );

        unsafe {
            kfree(zeroed);
            kfree(grown);
            kfree(string.cast());
        }
    }

    #[test]
    fn sensitive_free_and_double_release_failure_remain_recoverable() {
        let _lock = TEST_INSTALL_LOCK.lock();
        let _ = unsafe { uninstall() };
        TEST_DEALLOCATE_FAILURES.store(0, Ordering::Release);
        assert_eq!(unsafe { install(&TEST_KERNEL_API) }, Ok(()));

        let sensitive = kmalloc(32, 0);
        unsafe { sensitive.write_bytes(0x5a, 32) };
        TEST_DEALLOCATE_FAILURES.store(1, Ordering::Release);
        unsafe { kfree_sensitive(sensitive) };
        assert!(
            unsafe { core::slice::from_raw_parts(sensitive.cast::<u8>(), 32) }
                .iter()
                .all(|byte| *byte == 0)
        );
        unsafe { kfree(sensitive) };

        let original = kmalloc(8, 0);
        unsafe { original.write_bytes(0x3c, 8) };
        TEST_DEALLOCATE_FAILURES.store(3, Ordering::Release);
        assert!(unsafe { krealloc(original, 64, 0) }.is_null());
        assert_eq!(
            unsafe { uninstall() },
            Err(UninstallError::OperationsOrReclaimsOutstanding(1))
        );
        assert!(is_ready());
        TEST_DEALLOCATE_FAILURES.store(0, Ordering::Release);
        unsafe { kfree(original) };
        assert_eq!(unsafe { uninstall() }, Ok(()));
    }
}
