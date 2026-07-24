//! Exact requester-domain bindings for xHCI's fixed DMA arena.
//!
//! This module is intentionally separate from the identity-DMA allocator. A
//! translated controller receives one exact IOVA lease per ring region and
//! cannot fall back to first-fit relocation. The returned binding owns the
//! mapping receipts until the caller explicitly revokes them.

use sisyphus_driver_abi::Status;

use crate::hw::iommu::{DmaAccess, DmaMappingHandle, IommuDomain};

use super::xhci_dma::{XhciDmaPurpose, XhciDmaRegionPhase};
use super::xhci_ring::XhciRingStorage;

const CORE_PURPOSES: [XhciDmaPurpose; 4] = [
    XhciDmaPurpose::Dcbaa,
    XhciDmaPurpose::CommandRing,
    XhciDmaPurpose::EventRing,
    XhciDmaPurpose::EventRingSegmentTable,
];
const BINDING_ROOT_DOMAIN: u64 = 0x5848_4349_5654_444d;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciIommuBindingError {
    InvalidSecret,
    InvalidGeneration,
    MissingRegion(XhciDmaPurpose),
    RegionNotReady(XhciDmaPurpose),
    GenerationMismatch { expected: u32, observed: u32 },
    InvalidGeometry(XhciDmaPurpose),
    Mapping(Status),
}

/// Mapping receipts for the four regions programmed into the xHCI controller.
pub struct XhciIommuBinding {
    generation: u32,
    mappings: [Option<DmaMappingHandle>; CORE_PURPOSES.len()],
    root: u64,
}

impl core::fmt::Debug for XhciIommuBinding {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("XhciIommuBinding")
            .field("generation", &self.generation)
            .field("mapping_count", &self.mapping_count())
            .field("root", &self.root)
            .finish()
    }
}

impl core::fmt::Debug for XhciIommuReleaseDebt {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("XhciIommuReleaseDebt")
            .field("status", &self.status)
            .field("generation", &self.binding.generation)
            .field("mapping_count", &self.binding.mapping_count())
            .finish()
    }
}

impl core::fmt::Debug for XhciIommuBindFailure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("XhciIommuBindFailure")
            .field("cause", &self.cause)
            .field("has_release_debt", &self.debt.is_some())
            .finish()
    }
}

/// Retained mapping receipts when revocation cannot complete.
pub struct XhciIommuReleaseDebt {
    binding: XhciIommuBinding,
    status: Status,
}

impl XhciIommuReleaseDebt {
    pub const fn status(&self) -> Status {
        self.status
    }

    pub const fn generation(&self) -> u32 {
        self.binding.generation
    }

    pub fn retry(self, domain: &mut IommuDomain<'_>) -> Result<(), Self> {
        self.binding.revoke(domain)
    }
}

/// A failed bind retains any mapping receipts that could not be revoked.
pub struct XhciIommuBindFailure {
    cause: XhciIommuBindingError,
    debt: Option<XhciIommuReleaseDebt>,
}

impl XhciIommuBindFailure {
    pub const fn cause(&self) -> XhciIommuBindingError {
        self.cause
    }

    pub const fn has_release_debt(&self) -> bool {
        self.debt.is_some()
    }

    pub fn into_release_debt(self) -> Option<XhciIommuReleaseDebt> {
        self.debt
    }
}

impl XhciIommuBinding {
    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    pub fn mapping_count(&self) -> usize {
        self.mappings
            .iter()
            .filter(|mapping| mapping.is_some())
            .count()
    }

    /// Revokes every mapping before the domain itself is released.
    pub fn revoke(mut self, domain: &mut IommuDomain<'_>) -> Result<(), XhciIommuReleaseDebt> {
        for slot in &mut self.mappings {
            let Some(mapping) = *slot else {
                continue;
            };
            let status = domain.revoke_dma(mapping);
            if status != sisyphus_driver_abi::STATUS_OK {
                return Err(XhciIommuReleaseDebt {
                    binding: self,
                    status,
                });
            }
            *slot = None;
        }
        Ok(())
    }
}

pub fn bind_core_regions<S: XhciRingStorage>(
    domain: &mut IommuDomain<'_>,
    storage: &S,
    expected_generation: u32,
    secret: u64,
) -> Result<XhciIommuBinding, XhciIommuBindFailure> {
    if expected_generation == 0 || secret == 0 {
        return Err(failure(if expected_generation == 0 {
            XhciIommuBindingError::InvalidGeneration
        } else {
            XhciIommuBindingError::InvalidSecret
        }));
    }
    let mut records = [None; CORE_PURPOSES.len()];
    for (index, purpose) in CORE_PURPOSES.iter().copied().enumerate() {
        let record = storage
            .region(purpose)
            .ok_or_else(|| failure(XhciIommuBindingError::MissingRegion(purpose)))?;
        if record.phase != XhciDmaRegionPhase::Ready {
            return Err(failure(XhciIommuBindingError::RegionNotReady(purpose)));
        }
        if record.generation != expected_generation {
            return Err(failure(XhciIommuBindingError::GenerationMismatch {
                expected: expected_generation,
                observed: record.generation,
            }));
        }
        let Some(length) = record
            .page_count
            .checked_mul(4096)
            .and_then(|pages| usize::try_from(pages).ok())
        else {
            return Err(failure(XhciIommuBindingError::InvalidGeometry(purpose)));
        };
        if record.device_address_start % 4096 != 0
            || record.physical_start % 4096 != 0
            || length == 0
        {
            return Err(failure(XhciIommuBindingError::InvalidGeometry(purpose)));
        }
        records[index] = Some(record);
    }

    let mut mappings = [None; CORE_PURPOSES.len()];
    for (index, record) in records.iter().flatten().enumerate() {
        let length = record.page_count as usize * 4096;
        match domain.map_dma_at(
            record.device_address_start,
            record.physical_start,
            length,
            DmaAccess::READ_WRITE,
        ) {
            Ok(mapping) => {
                mappings[index] = Some(mapping);
            }
            Err(status) => {
                let partial = XhciIommuBinding {
                    generation: expected_generation,
                    mappings,
                    root: BINDING_ROOT_DOMAIN,
                };
                let debt = partial.revoke(domain).err();
                return Err(XhciIommuBindFailure {
                    cause: XhciIommuBindingError::Mapping(status),
                    debt,
                });
            }
        }
    }
    let mut root = mix(secret ^ BINDING_ROOT_DOMAIN, u64::from(expected_generation));
    for record in records.into_iter().flatten() {
        root = mix(root, record.device_address_start);
        root = mix(root, record.physical_start);
        root = mix(root, record.page_count as u64);
    }
    Ok(XhciIommuBinding {
        generation: expected_generation,
        mappings,
        root: if root == 0 { BINDING_ROOT_DOMAIN } else { root },
    })
}

const fn failure(cause: XhciIommuBindingError) -> XhciIommuBindFailure {
    XhciIommuBindFailure { cause, debt: None }
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use sisyphus_driver_abi::{Handle, STATUS_BUSY, STATUS_OK};

    use super::*;
    use crate::hw::iommu::DmaRemappingBackend;
    use crate::hw::iova::IovaRange;
    use crate::hw::pci::PciAddress;

    struct Backend {
        fail_unmap: AtomicBool,
        maps: AtomicUsize,
        unmaps: AtomicUsize,
    }

    impl Backend {
        const fn new() -> Self {
            Self {
                fail_unmap: AtomicBool::new(false),
                maps: AtomicUsize::new(0),
                unmaps: AtomicUsize::new(0),
            }
        }
    }

    impl DmaRemappingBackend for Backend {
        fn isolate_device(&self, _device: PciAddress) -> Result<Handle, Status> {
            Ok(1)
        }

        fn map(
            &self,
            _domain: Handle,
            _device_address: u64,
            _physical_address: u64,
            _length: usize,
            _access: DmaAccess,
        ) -> Status {
            self.maps.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }

        fn unmap(&self, _domain: Handle, _device_address: u64, _length: usize) -> Status {
            self.unmaps.fetch_add(1, Ordering::Relaxed);
            if self.fail_unmap.load(Ordering::Relaxed) {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }

        fn release_domain(&self, _domain: Handle) -> Status {
            STATUS_OK
        }
    }

    struct Storage {
        records: [super::super::xhci_dma::XhciDmaRegionRecord; CORE_PURPOSES.len()],
    }

    impl Storage {
        fn ready(generation: u32) -> Self {
            let device = PciAddress::new(0, 4, 0).unwrap();
            let mut records = [record(XhciDmaPurpose::Dcbaa, device, generation, 0x1000); 4];
            for (index, purpose) in CORE_PURPOSES.iter().copied().enumerate() {
                records[index] = record(purpose, device, generation, 0x1000 * (index as u64 + 1));
            }
            Self { records }
        }
    }

    impl XhciRingStorage for Storage {
        type Error = ();

        fn region(
            &self,
            purpose: XhciDmaPurpose,
        ) -> Option<super::super::xhci_dma::XhciDmaRegionRecord> {
            self.records
                .iter()
                .copied()
                .find(|record| record.purpose == purpose)
        }

        fn write(
            &self,
            _purpose: XhciDmaPurpose,
            _offset: usize,
            _bytes: &[u8],
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn read(
            &self,
            _purpose: XhciDmaPurpose,
            _offset: usize,
            _output: &mut [u8],
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn record(
        purpose: XhciDmaPurpose,
        device: PciAddress,
        generation: u32,
        address: u64,
    ) -> super::super::xhci_dma::XhciDmaRegionRecord {
        super::super::xhci_dma::XhciDmaRegionRecord {
            phase: XhciDmaRegionPhase::Ready,
            generation,
            purpose,
            device,
            physical_start: address,
            physical_end: address + 4096,
            device_address_start: address,
            device_address_end: address + 4096,
            cpu_start: address as usize,
            cpu_end: address as usize + 4096,
            page_count: 1,
            region_root: address,
        }
    }

    fn domain<'a>(backend: &'a Backend) -> IommuDomain<'a> {
        IommuDomain::isolate_device(
            backend,
            PciAddress::new(0, 4, 0).unwrap(),
            IovaRange::new(0x1000, 0x8000).unwrap(),
            &[],
        )
        .unwrap()
    }

    #[test]
    fn exact_binding_maps_each_core_region_and_releases_every_receipt() {
        let backend = Backend::new();
        let mut domain = domain(&backend);
        let binding = bind_core_regions(&mut domain, &Storage::ready(7), 7, 0x5eed).unwrap();
        assert_eq!(binding.mapping_count(), 4);
        assert_ne!(binding.root(), 0);
        binding.revoke(&mut domain).unwrap();
        assert_eq!(backend.maps.load(Ordering::Relaxed), 4);
        assert_eq!(backend.unmaps.load(Ordering::Relaxed), 4);
        assert!(domain.release().is_ok());
    }

    #[test]
    fn failed_release_retains_all_unrevoked_receipts_for_retry() {
        let backend = Backend::new();
        let mut domain = domain(&backend);
        let binding = bind_core_regions(&mut domain, &Storage::ready(9), 9, 0x5eed).unwrap();
        backend.fail_unmap.store(true, Ordering::Relaxed);
        let debt = binding.revoke(&mut domain).unwrap_err();
        assert_eq!(debt.generation(), 9);
        backend.fail_unmap.store(false, Ordering::Relaxed);
        debt.retry(&mut domain).unwrap();
        assert_eq!(domain.active_mapping_count(), 0);
        assert!(domain.release().is_ok());
    }
}
