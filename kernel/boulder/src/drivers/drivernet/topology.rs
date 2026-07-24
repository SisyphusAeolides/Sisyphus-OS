use super::fingerprint::{
    FirmwareFramebufferEvidence, PciFunctionAddress, TopologyEvidence, TopologyEvidenceProvider,
};
use super::inventory::PciFunctionRecord;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopologyRecord {
    pub address: PciFunctionAddress,
    pub evidence: TopologyEvidence,
}

impl TopologyRecord {
    pub const EMPTY: Self = Self {
        address: PciFunctionAddress {
            segment: 0,
            bus: 0,
            slot: 0,
            function: 0,
        },
        evidence: TopologyEvidence::EMPTY,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopologyTableError {
    ZeroCapacity,
    Capacity,
    DuplicateAddress,
    InvalidFirmwareFramebuffer,
}

pub struct BootTopologyTable<const N: usize> {
    records: [TopologyRecord; N],
    length: usize,
    firmware: FirmwareFramebufferEvidence,
}

impl<const N: usize> BootTopologyTable<N> {
    pub fn new() -> Result<Self, TopologyTableError> {
        if N == 0 {
            return Err(TopologyTableError::ZeroCapacity);
        }

        Ok(Self {
            records: [TopologyRecord::EMPTY; N],
            length: 0,
            firmware: FirmwareFramebufferEvidence::NONE,
        })
    }

    pub fn insert(&mut self, record: TopologyRecord) -> Result<(), TopologyTableError> {
        if self.records[..self.length]
            .iter()
            .any(|existing| existing.address == record.address)
        {
            return Err(TopologyTableError::DuplicateAddress);
        }

        let slot = self
            .records
            .get_mut(self.length)
            .ok_or(TopologyTableError::Capacity)?;
        *slot = record;
        self.length += 1;
        Ok(())
    }

    pub fn set_firmware_framebuffer(
        &mut self,
        firmware: FirmwareFramebufferEvidence,
    ) -> Result<(), TopologyTableError> {
        if !firmware.usable() {
            return Err(TopologyTableError::InvalidFirmwareFramebuffer);
        }
        self.firmware = firmware;
        Ok(())
    }

    pub fn records(&self) -> &[TopologyRecord] {
        &self.records[..self.length]
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
}

impl<const N: usize> TopologyEvidenceProvider for BootTopologyTable<N> {
    fn evidence_for(&self, function: &PciFunctionRecord) -> TopologyEvidence {
        let mut evidence = self.records[..self.length]
            .iter()
            .find(|record| record.address == function.address)
            .map(|record| record.evidence)
            .unwrap_or(TopologyEvidence::EMPTY);

        if evidence.firmware_framebuffer == FirmwareFramebufferEvidence::NONE
            && self
                .firmware
                .owner
                .is_some_and(|owner| owner == function.address)
        {
            evidence.firmware_framebuffer = self.firmware;
        }

        evidence
    }

    fn firmware_framebuffer(&self) -> FirmwareFramebufferEvidence {
        self.firmware
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::drivernet::fingerprint::{FirmwareFramebufferKind, TOPOLOGY_BOOT_DISPLAY};

    #[test]
    fn table_rejects_duplicate_addresses() {
        let mut table = BootTopologyTable::<4>::new().unwrap();
        let record = TopologyRecord {
            address: PciFunctionAddress {
                segment: 0,
                bus: 0,
                slot: 2,
                function: 0,
            },
            evidence: TopologyEvidence {
                topology_flags: TOPOLOGY_BOOT_DISPLAY,
                ..TopologyEvidence::EMPTY
            },
        };

        table.insert(record).unwrap();
        assert_eq!(
            table.insert(record),
            Err(TopologyTableError::DuplicateAddress)
        );
    }

    #[test]
    fn firmware_framebuffer_must_be_retained() {
        let mut table = BootTopologyTable::<4>::new().unwrap();
        let firmware = FirmwareFramebufferEvidence {
            kind: FirmwareFramebufferKind::UefiGop,
            physical_address: 0xe000_0000,
            width: 1024,
            height: 768,
            pitch: 4096,
            format: 1,
            byte_length: 3_145_728,
            owner: None,
            retained: false,
        };

        assert_eq!(
            table.set_firmware_framebuffer(firmware),
            Err(TopologyTableError::InvalidFirmwareFramebuffer)
        );
    }
}
