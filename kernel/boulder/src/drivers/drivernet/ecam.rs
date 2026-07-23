use super::fingerprint::{FingerprintError, PciConfigReader, PciFunctionAddress};

pub const MAXIMUM_ECAM_WINDOWS: usize = 32;
pub const PCIE_CONFIGURATION_BYTES: u16 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EcamWindow {
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    pub physical_base: u64,
}

impl EcamWindow {
    pub const EMPTY: Self = Self {
        segment: 0,
        bus_start: 0,
        bus_end: 0,
        physical_base: 0,
    };

    pub const fn valid(self) -> bool {
        self.physical_base != 0 && self.bus_start <= self.bus_end
    }

    pub const fn contains(self, address: PciFunctionAddress) -> bool {
        self.valid()
            && self.segment == address.segment
            && address.bus >= self.bus_start
            && address.bus <= self.bus_end
            && address.slot < 32
            && address.function < 8
    }

    pub fn physical_address(
        self,
        address: PciFunctionAddress,
        offset: u16,
    ) -> Result<u64, EcamError> {
        if !self.contains(address) {
            return Err(EcamError::AddressOutsideWindow);
        }
        if offset & 3 != 0 || offset >= PCIE_CONFIGURATION_BYTES {
            return Err(EcamError::InvalidOffset);
        }

        let bus_delta = u64::from(address.bus - self.bus_start);
        let function_offset = (bus_delta << 20)
            | (u64::from(address.slot) << 15)
            | (u64::from(address.function) << 12)
            | u64::from(offset);

        self.physical_base
            .checked_add(function_offset)
            .ok_or(EcamError::AddressOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcamError {
    ZeroCapacity,
    Capacity,
    InvalidWindow,
    OverlappingWindow,
    AddressOutsideWindow,
    AddressOverflow,
    InvalidOffset,
    MappingFault,
}

pub trait EcamAccess: Sync {
    fn read_u32(&self, physical_address: u64) -> Result<u32, EcamError>;
}

pub struct EcamConfigurationReader<'a, const N: usize> {
    windows: [EcamWindow; N],
    length: usize,
    access: &'a dyn EcamAccess,
}

impl<'a, const N: usize> EcamConfigurationReader<'a, N> {
    pub fn new(access: &'a dyn EcamAccess) -> Result<Self, EcamError> {
        if N == 0 {
            return Err(EcamError::ZeroCapacity);
        }

        Ok(Self {
            windows: [EcamWindow::EMPTY; N],
            length: 0,
            access,
        })
    }

    pub fn insert(&mut self, window: EcamWindow) -> Result<(), EcamError> {
        if !window.valid() {
            return Err(EcamError::InvalidWindow);
        }

        if self.windows[..self.length]
            .iter()
            .copied()
            .any(|existing| windows_overlap(existing, window))
        {
            return Err(EcamError::OverlappingWindow);
        }

        let destination = self
            .windows
            .get_mut(self.length)
            .ok_or(EcamError::Capacity)?;
        *destination = window;
        self.length += 1;
        self.windows[..self.length]
            .sort_unstable_by_key(|candidate| (candidate.segment, candidate.bus_start));
        Ok(())
    }

    pub fn windows(&self) -> &[EcamWindow] {
        &self.windows[..self.length]
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    fn window_for(&self, address: PciFunctionAddress) -> Result<EcamWindow, EcamError> {
        self.windows[..self.length]
            .iter()
            .copied()
            .find(|window| window.contains(address))
            .ok_or(EcamError::AddressOutsideWindow)
    }
}

impl<const N: usize> PciConfigReader for EcamConfigurationReader<'_, N> {
    fn maximum_offset(&self) -> u16 {
        PCIE_CONFIGURATION_BYTES - 1
    }

    fn read_u32(&self, address: PciFunctionAddress, offset: u16) -> Result<u32, FingerprintError> {
        let window = self
            .window_for(address)
            .map_err(|_| FingerprintError::UnsupportedPciSegment)?;
        let physical = window
            .physical_address(address, offset)
            .map_err(|error| match error {
                EcamError::InvalidOffset => {
                    if offset & 3 != 0 {
                        FingerprintError::UnalignedConfigurationOffset
                    } else {
                        FingerprintError::UnsupportedConfigurationOffset
                    }
                }
                _ => FingerprintError::ConfigurationRead,
            })?;

        self.access
            .read_u32(physical)
            .map_err(|_| FingerprintError::ConfigurationRead)
    }
}

pub struct CascadingConfigurationReader<
    'a,
    Primary: PciConfigReader + ?Sized,
    Fallback: PciConfigReader + ?Sized,
> {
    primary: &'a Primary,
    fallback: &'a Fallback,
}

impl<'a, Primary: PciConfigReader + ?Sized, Fallback: PciConfigReader + ?Sized>
    CascadingConfigurationReader<'a, Primary, Fallback>
{
    pub const fn new(primary: &'a Primary, fallback: &'a Fallback) -> Self {
        Self { primary, fallback }
    }
}

impl<Primary: PciConfigReader + ?Sized, Fallback: PciConfigReader + ?Sized> PciConfigReader
    for CascadingConfigurationReader<'_, Primary, Fallback>
{
    fn maximum_offset(&self) -> u16 {
        self.primary
            .maximum_offset()
            .max(self.fallback.maximum_offset())
    }

    fn read_u32(&self, address: PciFunctionAddress, offset: u16) -> Result<u32, FingerprintError> {
        match self.primary.read_u32(address, offset) {
            Ok(value) => Ok(value),
            Err(FingerprintError::UnsupportedPciSegment)
            | Err(FingerprintError::UnsupportedConfigurationOffset) => {
                self.fallback.read_u32(address, offset)
            }
            Err(error) => Err(error),
        }
    }
}

const fn windows_overlap(left: EcamWindow, right: EcamWindow) -> bool {
    left.segment == right.segment
        && left.bus_start <= right.bus_end
        && right.bus_start <= left.bus_end
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Access;

    impl EcamAccess for Access {
        fn read_u32(&self, physical_address: u64) -> Result<u32, EcamError> {
            Ok(physical_address as u32)
        }
    }

    #[test]
    fn computes_segment_relative_ecam_addresses() {
        let window = EcamWindow {
            segment: 2,
            bus_start: 0x40,
            bus_end: 0x4f,
            physical_base: 0x8000_0000,
        };
        let address = PciFunctionAddress {
            segment: 2,
            bus: 0x42,
            slot: 3,
            function: 5,
        };

        assert_eq!(
            window.physical_address(address, 0x100).unwrap(),
            0x8021_d100
        );
    }

    #[test]
    fn rejects_overlapping_windows_in_one_segment() {
        let mut reader = EcamConfigurationReader::<4>::new(&Access).unwrap();
        reader
            .insert(EcamWindow {
                segment: 0,
                bus_start: 0,
                bus_end: 63,
                physical_base: 0x8000_0000,
            })
            .unwrap();

        assert_eq!(
            reader.insert(EcamWindow {
                segment: 0,
                bus_start: 32,
                bus_end: 95,
                physical_base: 0x9000_0000,
            }),
            Err(EcamError::OverlappingWindow)
        );
    }
}
