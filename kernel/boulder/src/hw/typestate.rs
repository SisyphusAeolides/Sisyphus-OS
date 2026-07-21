use core::marker::PhantomData;
use core::ptr::NonNull;

pub struct Uninitialized;
pub struct PoweredOn;
pub struct Configured;

pub trait PhysicalDeviceBackend: Sync {
    type Error;

    fn power_on(&self, register_base: NonNull<u8>) -> Result<(), Self::Error>;
    fn configure_dma(
        &self,
        register_base: NonNull<u8>,
        ring_device_address: u64,
        ring_length: usize,
    ) -> Result<(), Self::Error>;
    fn transmit(
        &self,
        register_base: NonNull<u8>,
        packet_device_address: u64,
        packet_length: usize,
    ) -> Result<(), Self::Error>;
}

pub struct PhysicalDevice<'a, State, Backend: PhysicalDeviceBackend + ?Sized> {
    register_base: NonNull<u8>,
    backend: &'a Backend,
    _state: PhantomData<State>,
}

impl<'a, Backend: PhysicalDeviceBackend + ?Sized> PhysicalDevice<'a, Uninitialized, Backend> {
    /// Creates a device in its initial compile-time state.
    ///
    /// # Safety
    ///
    /// `register_base` must be a live mapping for the device represented by
    /// `backend` and must remain valid for the full device lifetime.
    pub unsafe fn new(register_base: NonNull<u8>, backend: &'a Backend) -> Self {
        Self {
            register_base,
            backend,
            _state: PhantomData,
        }
    }

    pub fn power_on(self) -> Result<PhysicalDevice<'a, PoweredOn, Backend>, Backend::Error> {
        self.backend.power_on(self.register_base)?;
        Ok(PhysicalDevice {
            register_base: self.register_base,
            backend: self.backend,
            _state: PhantomData,
        })
    }
}

impl<'a, Backend: PhysicalDeviceBackend + ?Sized> PhysicalDevice<'a, PoweredOn, Backend> {
    pub fn configure_dma(
        self,
        ring_device_address: u64,
        ring_length: usize,
    ) -> Result<PhysicalDevice<'a, Configured, Backend>, Backend::Error> {
        self.backend
            .configure_dma(self.register_base, ring_device_address, ring_length)?;
        Ok(PhysicalDevice {
            register_base: self.register_base,
            backend: self.backend,
            _state: PhantomData,
        })
    }
}

impl<Backend: PhysicalDeviceBackend + ?Sized> PhysicalDevice<'_, Configured, Backend> {
    pub fn transmit(
        &self,
        packet_device_address: u64,
        packet_length: usize,
    ) -> Result<(), Backend::Error> {
        self.backend
            .transmit(self.register_base, packet_device_address, packet_length)
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestBackend {
        transitions: AtomicUsize,
    }

    impl PhysicalDeviceBackend for TestBackend {
        type Error = Infallible;

        fn power_on(&self, _register_base: NonNull<u8>) -> Result<(), Self::Error> {
            self.transitions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn configure_dma(
            &self,
            _register_base: NonNull<u8>,
            _ring_device_address: u64,
            _ring_length: usize,
        ) -> Result<(), Self::Error> {
            self.transitions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn transmit(
            &self,
            _register_base: NonNull<u8>,
            _packet_device_address: u64,
            _packet_length: usize,
        ) -> Result<(), Self::Error> {
            self.transitions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn enforces_the_initialization_sequence() {
        let backend = TestBackend {
            transitions: AtomicUsize::new(0),
        };
        let mut registers = [0_u8; 64];
        let pointer = NonNull::new(registers.as_mut_ptr()).unwrap();
        let device = unsafe { PhysicalDevice::new(pointer, &backend) };
        let device = device.power_on().unwrap();
        let device = device.configure_dma(0x2000, 4096).unwrap();
        device.transmit(0x3000, 64).unwrap();

        assert_eq!(backend.transitions.load(Ordering::Relaxed), 3);
    }
}
