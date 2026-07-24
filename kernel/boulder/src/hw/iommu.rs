use sisyphus_driver_abi::{
    Handle, STATUS_BUSY, STATUS_INVALID_ARGUMENT, STATUS_IO_ERROR, STATUS_NOT_FOUND, STATUS_OK,
    STATUS_UNSUPPORTED, Status,
};

use super::iova::{IOVA_PAGE_SIZE, IovaError, IovaLease, IovaLedger, IovaRange};
use super::pci::PciAddress;

pub const MAXIMUM_DOMAIN_MAPPINGS: usize = 128;
pub const MAXIMUM_RESERVED_IOVA_RANGES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaAccess(u8);

impl DmaAccess {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);

    pub const fn bits(self) -> u8 {
        self.0
    }

    const fn is_valid(self) -> bool {
        self.0 != 0 && self.0 & !Self::READ_WRITE.0 == 0
    }
}

pub trait DmaRemappingBackend: Sync {
    fn isolate_device(&self, device: PciAddress) -> Result<Handle, Status>;
    fn map(
        &self,
        domain: Handle,
        device_address: u64,
        physical_address: u64,
        length: usize,
        access: DmaAccess,
    ) -> Status;
    fn unmap(&self, domain: Handle, device_address: u64, length: usize) -> Status;
    fn release_domain(&self, domain: Handle) -> Status;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaMappingHandle {
    slot: u16,
    generation: u32,
}

impl DmaMappingHandle {
    pub const fn from_raw(raw: u64) -> Option<Self> {
        let encoded_slot = raw as u32;
        let generation = (raw >> 32) as u32;
        if encoded_slot == 0 || generation == 0 || encoded_slot > u16::MAX as u32 + 1 {
            None
        } else {
            Some(Self {
                slot: (encoded_slot - 1) as u16,
                generation,
            })
        }
    }

    pub const fn raw(self) -> u64 {
        ((self.generation as u64) << 32) | (self.slot as u64 + 1)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaMappingInfo {
    pub device_address: u64,
    pub physical_address: u64,
    pub length: usize,
    pub access: DmaAccess,
}

#[derive(Clone, Copy)]
struct MappingRecord {
    generation: u32,
    lease: IovaLease,
    physical_address: u64,
    length: usize,
    access: DmaAccess,
    hardware_mapped: bool,
    active: bool,
}

impl MappingRecord {
    const EMPTY: Self = Self {
        generation: 0,
        lease: IovaLease::INVALID,
        physical_address: 0,
        length: 0,
        access: DmaAccess(0),
        hardware_mapped: false,
        active: false,
    };
}

pub struct IommuDomain<'a> {
    backend: &'a dyn DmaRemappingBackend,
    handle: Handle,
    device: PciAddress,
    active: bool,
    poisoned: bool,
    iovas: IovaLedger<MAXIMUM_DOMAIN_MAPPINGS, MAXIMUM_RESERVED_IOVA_RANGES>,
    mappings: [MappingRecord; MAXIMUM_DOMAIN_MAPPINGS],
    mapping_count: usize,
}

impl<'a> IommuDomain<'a> {
    pub fn isolate_device(
        backend: &'a dyn DmaRemappingBackend,
        device: PciAddress,
        aperture: IovaRange,
        reserved: &[IovaRange],
    ) -> Result<Self, Status> {
        let iovas = IovaLedger::new(aperture, reserved).map_err(iova_configuration_status)?;
        let handle = backend.isolate_device(device)?;
        if handle == 0 {
            return Err(STATUS_UNSUPPORTED);
        }
        Ok(Self {
            backend,
            handle,
            device,
            active: true,
            poisoned: false,
            iovas,
            mappings: [MappingRecord::EMPTY; MAXIMUM_DOMAIN_MAPPINGS],
            mapping_count: 0,
        })
    }

    pub const fn device(&self) -> PciAddress {
        self.device
    }

    pub const fn handle(&self) -> Handle {
        self.handle
    }

    pub const fn active_mapping_count(&self) -> usize {
        self.mapping_count
    }

    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn mapping_info(&self, mapping: DmaMappingHandle) -> Option<DmaMappingInfo> {
        let record = self.mappings.get(usize::from(mapping.slot))?;
        if !record.active || record.generation != mapping.generation {
            return None;
        }
        let range = self.iovas.range(record.lease).ok()?;
        (range.length() == record.length as u64).then_some(DmaMappingInfo {
            device_address: range.start(),
            physical_address: record.physical_address,
            length: record.length,
            access: record.access,
        })
    }

    pub fn map_dma(
        &mut self,
        physical_address: u64,
        size: usize,
        access: DmaAccess,
    ) -> Result<DmaMappingHandle, Status> {
        self.map_dma_aligned(physical_address, size, 1, access)
    }

    pub fn map_dma_aligned(
        &mut self,
        physical_address: u64,
        size: usize,
        alignment_pages: u64,
        access: DmaAccess,
    ) -> Result<DmaMappingHandle, Status> {
        let size_u64 = u64::try_from(size).map_err(|_| STATUS_INVALID_ARGUMENT)?;
        if !self.active {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        if self.poisoned {
            return Err(STATUS_IO_ERROR);
        }
        if size == 0
            || physical_address % IOVA_PAGE_SIZE != 0
            || size_u64 % IOVA_PAGE_SIZE != 0
            || physical_address.checked_add(size_u64).is_none()
            || !access.is_valid()
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        if self
            .mappings
            .iter()
            .copied()
            .filter(|record| record.active)
            .any(|record| {
                ranges_overlap(
                    record.physical_address,
                    record.length,
                    physical_address,
                    size,
                )
            })
        {
            return Err(STATUS_BUSY);
        }
        let slot = self
            .mappings
            .iter()
            .position(|record| !record.active && record.generation != u32::MAX)
            .ok_or(STATUS_BUSY)?;
        let page_count = size_u64 / IOVA_PAGE_SIZE;
        let mut candidate_iovas = self.iovas;
        let lease = candidate_iovas
            .reserve_aligned(page_count, alignment_pages)
            .map_err(iova_runtime_status)?;
        let range = match candidate_iovas.range(lease) {
            Ok(range) if range.length() == size_u64 => range,
            _ => {
                self.poisoned = true;
                return Err(STATUS_IO_ERROR);
            }
        };
        let status = self
            .backend
            .map(self.handle, range.start(), physical_address, size, access);
        if status != STATUS_OK {
            return Err(status);
        }
        self.iovas = candidate_iovas;
        let generation = self.mappings[slot].generation + 1;
        self.mappings[slot] = MappingRecord {
            generation,
            lease,
            physical_address,
            length: size,
            access,
            hardware_mapped: true,
            active: true,
        };
        self.mapping_count += 1;
        Ok(DmaMappingHandle {
            slot: slot as u16,
            generation,
        })
    }

    /// Maps a caller-selected IOVA without relocating it.  The operation is
    /// deliberately separate from first-fit allocation so an xHCI ring can
    /// use IOVA==physical while still being covered by an active VT-d
    /// requester context.
    pub fn map_dma_at(
        &mut self,
        device_address: u64,
        physical_address: u64,
        size: usize,
        access: DmaAccess,
    ) -> Result<DmaMappingHandle, Status> {
        let size_u64 = u64::try_from(size).map_err(|_| STATUS_INVALID_ARGUMENT)?;
        if !self.active || self.poisoned {
            return Err(if self.poisoned {
                STATUS_IO_ERROR
            } else {
                STATUS_INVALID_ARGUMENT
            });
        }
        if size == 0
            || device_address % IOVA_PAGE_SIZE != 0
            || physical_address % IOVA_PAGE_SIZE != 0
            || size_u64 % IOVA_PAGE_SIZE != 0
            || device_address.checked_add(size_u64).is_none()
            || physical_address.checked_add(size_u64).is_none()
            || !access.is_valid()
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        if self
            .mappings
            .iter()
            .copied()
            .filter(|record| record.active)
            .any(|record| {
                ranges_overlap(
                    record.physical_address,
                    record.length,
                    physical_address,
                    size,
                ) || self
                    .iovas
                    .range(record.lease)
                    .map(|range| ranges_overlap(range.start(), record.length, device_address, size))
                    .unwrap_or(true)
            })
        {
            return Err(STATUS_BUSY);
        }
        let slot = self
            .mappings
            .iter()
            .position(|record| !record.active && record.generation != u32::MAX)
            .ok_or(STATUS_BUSY)?;
        let range = IovaRange::new(device_address, size_u64).map_err(iova_runtime_status)?;
        let mut candidate_iovas = self.iovas;
        let lease = candidate_iovas
            .reserve_exact(range)
            .map_err(iova_runtime_status)?;
        let status = self
            .backend
            .map(self.handle, device_address, physical_address, size, access);
        if status != STATUS_OK {
            return Err(status);
        }
        self.iovas = candidate_iovas;
        let generation = self.mappings[slot].generation + 1;
        self.mappings[slot] = MappingRecord {
            generation,
            lease,
            physical_address,
            length: size,
            access,
            hardware_mapped: true,
            active: true,
        };
        self.mapping_count += 1;
        Ok(DmaMappingHandle {
            slot: slot as u16,
            generation,
        })
    }

    pub fn revoke_dma(&mut self, mapping: DmaMappingHandle) -> Status {
        if !self.active {
            return STATUS_INVALID_ARGUMENT;
        }
        if self.poisoned {
            return STATUS_IO_ERROR;
        }
        let slot = usize::from(mapping.slot);
        let Some(record) = self.mappings.get(slot).copied() else {
            return STATUS_NOT_FOUND;
        };
        if !record.active || record.generation != mapping.generation {
            return STATUS_NOT_FOUND;
        }
        let Ok(range) = self.iovas.range(record.lease) else {
            self.poisoned = true;
            return STATUS_IO_ERROR;
        };
        if range.length() != record.length as u64 || !record.hardware_mapped {
            self.poisoned = true;
            return STATUS_IO_ERROR;
        }
        let status = self
            .backend
            .unmap(self.handle, range.start(), record.length);
        if status != STATUS_OK {
            return status;
        }
        self.mappings[slot].hardware_mapped = false;
        if self.iovas.release(record.lease).is_err() {
            self.poisoned = true;
            return STATUS_IO_ERROR;
        }
        self.mappings[slot] = MappingRecord {
            generation: record.generation,
            ..MappingRecord::EMPTY
        };
        self.mapping_count -= 1;
        STATUS_OK
    }

    /// Releases an empty domain. On failure the error owns the still-live
    /// domain, allowing the caller to repair mappings or retry without
    /// reconstructing authority from a raw handle.
    pub fn release(mut self) -> Result<(), DomainReleaseError<'a>> {
        if self.poisoned {
            return Err(DomainReleaseError {
                status: STATUS_IO_ERROR,
                domain: self,
            });
        }
        if self.mapping_count != 0 {
            return Err(DomainReleaseError {
                status: STATUS_BUSY,
                domain: self,
            });
        }
        if self.iovas.active_lease_count() != 0 {
            self.poisoned = true;
            return Err(DomainReleaseError {
                status: STATUS_IO_ERROR,
                domain: self,
            });
        }
        let status = self.backend.release_domain(self.handle);
        if status == STATUS_OK {
            self.active = false;
            Ok(())
        } else {
            Err(DomainReleaseError {
                status,
                domain: self,
            })
        }
    }
}

pub struct DomainReleaseError<'a> {
    status: Status,
    domain: IommuDomain<'a>,
}

impl<'a> DomainReleaseError<'a> {
    pub const fn status(&self) -> Status {
        self.status
    }

    pub fn into_domain(self) -> IommuDomain<'a> {
        self.domain
    }
}

impl Drop for IommuDomain<'_> {
    fn drop(&mut self) {
        if self.active {
            let mut mappings_drained = true;
            for index in (0..self.mappings.len()).rev() {
                let record = self.mappings[index];
                if !record.active {
                    continue;
                }
                let Ok(range) = self.iovas.range(record.lease) else {
                    self.poisoned = true;
                    mappings_drained = false;
                    continue;
                };
                if range.length() != record.length as u64 {
                    self.poisoned = true;
                    mappings_drained = false;
                    continue;
                }
                if record.hardware_mapped
                    && self
                        .backend
                        .unmap(self.handle, range.start(), record.length)
                        != STATUS_OK
                {
                    mappings_drained = false;
                    continue;
                }
                self.mappings[index].hardware_mapped = false;
                if self.iovas.release(record.lease).is_err() {
                    self.poisoned = true;
                    mappings_drained = false;
                    continue;
                }
                self.mappings[index] = MappingRecord {
                    generation: record.generation,
                    ..MappingRecord::EMPTY
                };
                self.mapping_count -= 1;
            }
            if self.mapping_count != 0 || self.iovas.active_lease_count() != 0 {
                mappings_drained = false;
            }
            if mappings_drained && !self.poisoned {
                let _ = self.backend.release_domain(self.handle);
            }
            self.active = false;
        }
    }
}

fn ranges_overlap(left_start: u64, left_size: usize, right_start: u64, right_size: usize) -> bool {
    let Some(left_end) = left_start.checked_add(left_size as u64) else {
        return true;
    };
    let Some(right_end) = right_start.checked_add(right_size as u64) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

fn iova_configuration_status(_error: IovaError) -> Status {
    STATUS_INVALID_ARGUMENT
}

fn iova_runtime_status(error: IovaError) -> Status {
    match error {
        IovaError::InvalidRange | IovaError::InvalidRequest => STATUS_INVALID_ARGUMENT,
        IovaError::LeaseCapacity | IovaError::AddressSpaceExhausted => STATUS_BUSY,
        IovaError::ExactRangeUnavailable => STATUS_BUSY,
        IovaError::StaleLease => STATUS_NOT_FOUND,
        IovaError::InvalidCapacity
        | IovaError::ReservedCapacity
        | IovaError::ReservedOutsideAperture
        | IovaError::OverlappingReservedRanges
        | IovaError::BatchOutputMismatch => STATUS_IO_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use super::*;

    struct TestBackend {
        calls: AtomicUsize,
        fail_map: AtomicBool,
        fail_unmap: AtomicBool,
        fail_release: AtomicBool,
        last_map_address: AtomicU64,
        last_unmap_address: AtomicU64,
    }

    impl TestBackend {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail_map: AtomicBool::new(false),
                fail_unmap: AtomicBool::new(false),
                fail_release: AtomicBool::new(false),
                last_map_address: AtomicU64::new(0),
                last_unmap_address: AtomicU64::new(0),
            }
        }
    }

    impl DmaRemappingBackend for TestBackend {
        fn isolate_device(&self, _device: PciAddress) -> Result<Handle, Status> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(7)
        }

        fn map(
            &self,
            domain: Handle,
            device_address: u64,
            _physical_address: u64,
            _length: usize,
            _access: DmaAccess,
        ) -> Status {
            assert_eq!(domain, 7);
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.last_map_address
                .store(device_address, Ordering::Relaxed);
            if self.fail_map.load(Ordering::Relaxed) {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }

        fn unmap(&self, domain: Handle, device_address: u64, _length: usize) -> Status {
            assert_eq!(domain, 7);
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.last_unmap_address
                .store(device_address, Ordering::Relaxed);
            if self.fail_unmap.load(Ordering::Relaxed) {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }

        fn release_domain(&self, domain: Handle) -> Status {
            assert_eq!(domain, 7);
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_release.load(Ordering::Relaxed) {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }
    }

    fn range(start: u64, pages: u64) -> IovaRange {
        IovaRange::new(start, pages * IOVA_PAGE_SIZE).unwrap()
    }

    fn isolate<'a>(backend: &'a TestBackend, pages: u64) -> IommuDomain<'a> {
        IommuDomain::isolate_device(
            backend,
            PciAddress::new(0, 2, 0).unwrap(),
            range(0x1000, pages),
            &[],
        )
        .unwrap()
    }

    #[test]
    fn assigns_reserved_aware_aligned_iovas_without_raw_address_authority() {
        let backend = TestBackend::new();
        let device = PciAddress::new(0, 2, 0).unwrap();
        let mut domain =
            IommuDomain::isolate_device(&backend, device, range(0x1000, 16), &[range(0x1000, 1)])
                .unwrap();

        let first = domain
            .map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ_WRITE)
            .unwrap();
        let aligned = domain
            .map_dma_aligned(0x8000, (2 * IOVA_PAGE_SIZE) as usize, 4, DmaAccess::READ)
            .unwrap();
        assert_eq!(
            domain.mapping_info(first),
            Some(DmaMappingInfo {
                device_address: 0x2000,
                physical_address: 0x4000,
                length: IOVA_PAGE_SIZE as usize,
                access: DmaAccess::READ_WRITE,
            })
        );
        assert_eq!(domain.mapping_info(aligned).unwrap().device_address, 0x4000);
        assert_eq!(backend.last_map_address.load(Ordering::Relaxed), 0x4000);

        assert_eq!(
            domain.map_dma(1, IOVA_PAGE_SIZE as usize, DmaAccess::READ),
            Err(STATUS_INVALID_ARGUMENT)
        );
        assert_eq!(
            domain.map_dma_aligned(0xc000, IOVA_PAGE_SIZE as usize, 3, DmaAccess::READ,),
            Err(STATUS_INVALID_ARGUMENT)
        );
        assert_eq!(
            domain.map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ),
            Err(STATUS_BUSY)
        );

        let busy = domain.release().unwrap_err();
        assert_eq!(busy.status(), STATUS_BUSY);
        let mut domain = busy.into_domain();
        assert_eq!(domain.revoke_dma(first), STATUS_OK);
        assert_eq!(domain.revoke_dma(aligned), STATUS_OK);
        assert!(domain.release().is_ok());
        assert_eq!(backend.calls.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn failed_map_rolls_back_the_entire_iova_reservation() {
        let backend = TestBackend::new();
        let mut domain = isolate(&backend, 2);
        backend.fail_map.store(true, Ordering::Relaxed);

        assert_eq!(
            domain.map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ),
            Err(STATUS_BUSY)
        );
        assert_eq!(domain.active_mapping_count(), 0);
        assert_eq!(domain.iovas.active_lease_count(), 0);

        backend.fail_map.store(false, Ordering::Relaxed);
        let mapping = domain
            .map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();
        assert_eq!(mapping.raw() >> 32, 1);
        assert_eq!(
            domain.mapping_info(mapping).unwrap().device_address,
            domain.iovas.aperture().start()
        );
        assert_eq!(domain.revoke_dma(mapping), STATUS_OK);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn exact_mapping_proves_identity_iova_without_relocation() {
        let backend = TestBackend::new();
        let mut domain = isolate(&backend, 8);
        let mapping = domain
            .map_dma_at(
                0x5000,
                0x5000,
                IOVA_PAGE_SIZE as usize,
                DmaAccess::READ_WRITE,
            )
            .unwrap();
        assert_eq!(
            domain.mapping_info(mapping).unwrap(),
            DmaMappingInfo {
                device_address: 0x5000,
                physical_address: 0x5000,
                length: IOVA_PAGE_SIZE as usize,
                access: DmaAccess::READ_WRITE,
            }
        );
        assert_eq!(
            domain.map_dma_at(0x5000, 0x9000, IOVA_PAGE_SIZE as usize, DmaAccess::READ,),
            Err(STATUS_BUSY)
        );
        assert_eq!(domain.revoke_dma(mapping), STATUS_OK);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn failed_unmap_retains_hardware_and_iova_authority() {
        let backend = TestBackend::new();
        let mut domain = isolate(&backend, 3);
        let first = domain
            .map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ_WRITE)
            .unwrap();

        backend.fail_unmap.store(true, Ordering::Relaxed);
        assert_eq!(domain.revoke_dma(first), STATUS_BUSY);
        assert_eq!(domain.active_mapping_count(), 1);
        assert_eq!(domain.iovas.active_lease_count(), 1);
        assert!(domain.mapping_info(first).is_some());

        let second = domain
            .map_dma(0x8000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();
        assert_eq!(domain.mapping_info(second).unwrap().device_address, 0x2000);

        backend.fail_unmap.store(false, Ordering::Relaxed);
        assert_eq!(domain.revoke_dma(first), STATUS_OK);
        let replacement = domain
            .map_dma(0xc000, IOVA_PAGE_SIZE as usize, DmaAccess::WRITE)
            .unwrap();
        assert_eq!(
            domain.mapping_info(replacement).unwrap().device_address,
            0x1000
        );
        assert_ne!(first, replacement);
        assert_eq!(domain.revoke_dma(first), STATUS_NOT_FOUND);
        assert_eq!(domain.revoke_dma(second), STATUS_OK);
        assert_eq!(domain.revoke_dma(replacement), STATUS_OK);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn aperture_and_mapping_capacity_fail_closed() {
        let backend = TestBackend::new();
        let mut domain = isolate(&backend, 2);
        let first = domain
            .map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();
        let second = domain
            .map_dma(0x8000, IOVA_PAGE_SIZE as usize, DmaAccess::WRITE)
            .unwrap();

        assert_eq!(
            domain.map_dma(0xc000, IOVA_PAGE_SIZE as usize, DmaAccess::READ),
            Err(STATUS_BUSY)
        );
        assert_eq!(domain.active_mapping_count(), 2);
        assert_eq!(domain.revoke_dma(first), STATUS_OK);
        assert_eq!(domain.revoke_dma(second), STATUS_OK);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn stale_and_forged_mapping_leases_cannot_revoke_replacements() {
        let backend = TestBackend::new();
        let mut domain = isolate(&backend, 1);
        let first = domain
            .map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();
        assert_eq!(domain.revoke_dma(first), STATUS_OK);

        let replacement = domain
            .map_dma(0x8000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();
        assert_ne!(first, replacement);
        assert_eq!(domain.revoke_dma(first), STATUS_NOT_FOUND);
        let forged = DmaMappingHandle::from_raw(replacement.raw() + (1_u64 << 32)).unwrap();
        assert_eq!(domain.revoke_dma(forged), STATUS_NOT_FOUND);
        assert!(domain.mapping_info(first).is_none());
        assert_eq!(domain.revoke_dma(replacement), STATUS_OK);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn failed_backend_release_returns_the_complete_domain_for_retry() {
        let backend = TestBackend::new();
        let domain = isolate(&backend, 1);
        backend.fail_release.store(true, Ordering::Relaxed);

        let failure = domain.release().unwrap_err();
        assert_eq!(failure.status(), STATUS_BUSY);
        let domain = failure.into_domain();
        backend.fail_release.store(false, Ordering::Relaxed);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn exhausted_mapping_generation_retires_its_slot() {
        let backend = TestBackend::new();
        let mut domain = isolate(&backend, 1);
        domain.mappings[0].generation = u32::MAX - 1;

        let final_handle = domain
            .map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();
        assert_eq!(final_handle.slot, 0);
        assert_eq!(final_handle.generation, u32::MAX);
        assert_eq!(domain.revoke_dma(final_handle), STATUS_OK);

        let replacement = domain
            .map_dma(0x8000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();
        assert_eq!(replacement.slot, 1);
        assert_eq!(domain.revoke_dma(final_handle), STATUS_NOT_FOUND);
        assert_eq!(domain.revoke_dma(replacement), STATUS_OK);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn accounting_mismatch_poison_quarantines_the_domain() {
        let backend = TestBackend::new();
        let mut domain = isolate(&backend, 1);
        let mapping = domain
            .map_dma(0x4000, IOVA_PAGE_SIZE as usize, DmaAccess::READ)
            .unwrap();

        let lease = domain.mappings[usize::from(mapping.slot)].lease;
        domain.iovas.release(lease).unwrap();
        assert_eq!(domain.revoke_dma(mapping), STATUS_IO_ERROR);
        assert!(domain.is_poisoned());
        assert_eq!(
            domain.map_dma(0x8000, IOVA_PAGE_SIZE as usize, DmaAccess::READ),
            Err(STATUS_IO_ERROR)
        );

        let failure = domain.release().unwrap_err();
        assert_eq!(failure.status(), STATUS_IO_ERROR);
        drop(failure.into_domain());
        assert_eq!(backend.calls.load(Ordering::Relaxed), 2);
    }
}
