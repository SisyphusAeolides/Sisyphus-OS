use sisyphus_driver_abi::{Handle, STATUS_INVALID_ARGUMENT, STATUS_OK, STATUS_UNSUPPORTED, Status};

use super::pci::PciAddress;

const PAGE_SIZE: u64 = 4096;

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

pub struct IommuDomain<'a> {
    backend: &'a dyn DmaRemappingBackend,
    handle: Handle,
    device: PciAddress,
    active: bool,
}

impl<'a> IommuDomain<'a> {
    pub fn isolate_device(
        backend: &'a dyn DmaRemappingBackend,
        device: PciAddress,
    ) -> Result<Self, Status> {
        let handle = backend.isolate_device(device)?;
        if handle == 0 {
            return Err(STATUS_UNSUPPORTED);
        }
        Ok(Self {
            backend,
            handle,
            device,
            active: true,
        })
    }

    pub const fn device(&self) -> PciAddress {
        self.device
    }

    pub const fn handle(&self) -> Handle {
        self.handle
    }

    pub fn allow_dma(
        &self,
        device_address: u64,
        physical_address: u64,
        size: usize,
        access: DmaAccess,
    ) -> Status {
        if !self.active
            || size == 0
            || device_address % PAGE_SIZE != 0
            || physical_address % PAGE_SIZE != 0
            || size as u64 % PAGE_SIZE != 0
            || device_address.checked_add(size as u64).is_none()
            || physical_address.checked_add(size as u64).is_none()
            || !access.is_valid()
        {
            return STATUS_INVALID_ARGUMENT;
        }
        self.backend
            .map(self.handle, device_address, physical_address, size, access)
    }

    pub fn revoke_dma(&self, device_address: u64, size: usize) -> Status {
        if !self.active
            || size == 0
            || device_address % PAGE_SIZE != 0
            || size as u64 % PAGE_SIZE != 0
            || device_address.checked_add(size as u64).is_none()
        {
            return STATUS_INVALID_ARGUMENT;
        }
        self.backend.unmap(self.handle, device_address, size)
    }

    pub fn release(mut self) -> Status {
        let status = self.backend.release_domain(self.handle);
        if status == STATUS_OK {
            self.active = false;
        }
        status
    }
}

impl Drop for IommuDomain<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.backend.release_domain(self.handle);
            self.active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestBackend {
        calls: AtomicUsize,
    }

    impl DmaRemappingBackend for TestBackend {
        fn isolate_device(&self, _device: PciAddress) -> Result<Handle, Status> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(7)
        }

        fn map(
            &self,
            domain: Handle,
            _device_address: u64,
            _physical_address: u64,
            _length: usize,
            _access: DmaAccess,
        ) -> Status {
            assert_eq!(domain, 7);
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }

        fn unmap(&self, domain: Handle, _device_address: u64, _length: usize) -> Status {
            assert_eq!(domain, 7);
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }

        fn release_domain(&self, domain: Handle) -> Status {
            assert_eq!(domain, 7);
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }
    }

    #[test]
    fn validates_dma_mappings_and_releases_the_domain() {
        let backend = TestBackend {
            calls: AtomicUsize::new(0),
        };
        let device = PciAddress::new(0, 2, 0).unwrap();
        let domain = IommuDomain::isolate_device(&backend, device).unwrap();
        assert_eq!(
            domain.allow_dma(0x2000, 0x4000, 4096, DmaAccess::READ_WRITE),
            STATUS_OK
        );
        assert_eq!(
            domain.allow_dma(1, 0x4000, 4096, DmaAccess::READ_WRITE),
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(domain.revoke_dma(0x2000, 4096), STATUS_OK);
        assert_eq!(domain.release(), STATUS_OK);
        assert_eq!(backend.calls.load(Ordering::Relaxed), 4);
    }
}
