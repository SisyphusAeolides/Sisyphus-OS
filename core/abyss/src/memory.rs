use crate::paging::{PAGE_SIZE, PhysicalAddress};

pub const MAX_MEMORY_REGIONS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRegionKind {
    Usable,
    Reserved,
    AcpiReclaimable,
    AcpiNonVolatile,
    Defective,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    pub start: PhysicalAddress,
    pub end: PhysicalAddress,
    pub kind: MemoryRegionKind,
}

impl MemoryRegion {
    const EMPTY: Self = Self {
        start: PhysicalAddress::new(0),
        end: PhysicalAddress::new(0),
        kind: MemoryRegionKind::Reserved,
    };

    pub const fn new(start: PhysicalAddress, end: PhysicalAddress, kind: MemoryRegionKind) -> Self {
        Self { start, end, kind }
    }

    pub const fn length(self) -> u64 {
        self.end.as_u64().saturating_sub(self.start.as_u64())
    }
}

pub struct MemoryMap {
    regions: [MemoryRegion; MAX_MEMORY_REGIONS],
    length: usize,
}

impl MemoryMap {
    pub const fn new() -> Self {
        Self {
            regions: [MemoryRegion::EMPTY; MAX_MEMORY_REGIONS],
            length: 0,
        }
    }

    pub fn push(&mut self, region: MemoryRegion) -> Result<(), MemoryMapError> {
        if region.start.as_u64() >= region.end.as_u64() {
            return Err(MemoryMapError::InvalidRegion);
        }
        let slot = self
            .regions
            .get_mut(self.length)
            .ok_or(MemoryMapError::CapacityExceeded)?;
        *slot = region;
        self.length += 1;
        Ok(())
    }

    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions[..self.length]
    }

    pub fn usable_range(
        &self,
        minimum_address: u64,
        maximum_address: u64,
        minimum_size: u64,
    ) -> Option<MemoryRegion> {
        self.regions()
            .iter()
            .filter(|region| region.kind == MemoryRegionKind::Usable)
            .filter_map(|region| {
                let start = align_up(region.start.as_u64().max(minimum_address), PAGE_SIZE as u64)?;
                let end = align_down(region.end.as_u64().min(maximum_address), PAGE_SIZE as u64);
                (end.saturating_sub(start) >= minimum_size).then(|| {
                    MemoryRegion::new(
                        PhysicalAddress::new(start),
                        PhysicalAddress::new(end),
                        MemoryRegionKind::Usable,
                    )
                })
            })
            .max_by_key(|region| region.length())
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
}

impl Default for MemoryMap {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryMapError {
    InvalidRegion,
    CapacityExceeded,
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded & !(alignment - 1))
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_largest_clipped_usable_range() {
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(0),
            PhysicalAddress::new(0x9f000),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(0x10_0000),
            PhysicalAddress::new(0x80_0000),
            MemoryRegionKind::Usable,
        ))
        .unwrap();

        let selected = map.usable_range(0x12_3456, 0x40_0000, 0x10_0000).unwrap();
        assert_eq!(selected.start.as_u64(), 0x12_4000);
        assert_eq!(selected.end.as_u64(), 0x40_0000);
    }

    #[test]
    fn rejects_empty_regions() {
        let mut map = MemoryMap::new();
        assert_eq!(
            map.push(MemoryRegion::new(
                PhysicalAddress::new(0x1000),
                PhysicalAddress::new(0x1000),
                MemoryRegionKind::Usable,
            )),
            Err(MemoryMapError::InvalidRegion)
        );
    }
}
