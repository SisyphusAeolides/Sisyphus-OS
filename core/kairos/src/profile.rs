pub const MAXIMUM_CPUS: usize = 256;
pub const MAXIMUM_MEMORY_REGIONS: usize = 128;
pub const MAXIMUM_IO_DEVICES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CpuKind {
    Symmetric,
    Performance,
    Efficiency,
    Offload,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuProfile {
    pub hardware_id: u32,
    pub firmware_id: u32,
    pub package: u16,
    pub cluster: u16,
    pub core: u16,
    pub thread: u16,
    pub numa_domain: u16,
    pub kind: CpuKind,
    pub enabled: bool,
}

impl CpuProfile {
    const EMPTY: Self = Self {
        hardware_id: 0,
        firmware_id: 0,
        package: 0,
        cluster: 0,
        core: 0,
        thread: 0,
        numa_domain: 0,
        kind: CpuKind::Unknown,
        enabled: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MemoryKind {
    Ram,
    Reserved,
    Firmware,
    Persistent,
    Shared,
    Device,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryProfile {
    pub base: u64,
    pub length: u64,
    pub numa_domain: u16,
    pub kind: MemoryKind,
}

impl MemoryProfile {
    const EMPTY: Self = Self {
        base: 0,
        length: 0,
        numa_domain: 0,
        kind: MemoryKind::Reserved,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoProfile {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub class: u8,
    pub subclass: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub interrupt: u32,
}

impl IoProfile {
    const EMPTY: Self = Self {
        segment: 0,
        bus: 0,
        device: 0,
        function: 0,
        class: 0,
        subclass: 0,
        vendor_id: 0,
        device_id: 0,
        interrupt: u32::MAX,
    };
}

pub struct MachineProfile {
    cpus: [CpuProfile; MAXIMUM_CPUS],
    cpu_count: usize,
    memory: [MemoryProfile; MAXIMUM_MEMORY_REGIONS],
    memory_count: usize,
    io: [IoProfile; MAXIMUM_IO_DEVICES],
    io_count: usize,
}

impl MachineProfile {
    pub const fn new() -> Self {
        Self {
            cpus: [CpuProfile::EMPTY; MAXIMUM_CPUS],
            cpu_count: 0,
            memory: [MemoryProfile::EMPTY; MAXIMUM_MEMORY_REGIONS],
            memory_count: 0,
            io: [IoProfile::EMPTY; MAXIMUM_IO_DEVICES],
            io_count: 0,
        }
    }

    pub fn push_cpu(&mut self, cpu: CpuProfile) -> Result<(), ProfileError> {
        if self
            .cpus()
            .iter()
            .any(|existing| existing.hardware_id == cpu.hardware_id)
        {
            return Err(ProfileError::DuplicateCpu);
        }
        let slot = self
            .cpus
            .get_mut(self.cpu_count)
            .ok_or(ProfileError::CapacityExceeded)?;
        *slot = cpu;
        self.cpu_count += 1;
        Ok(())
    }

    pub fn push_memory(&mut self, memory: MemoryProfile) -> Result<(), ProfileError> {
        let end = memory
            .base
            .checked_add(memory.length)
            .ok_or(ProfileError::InvalidRange)?;
        if memory.length == 0 {
            return Err(ProfileError::InvalidRange);
        }
        if self.memory().iter().any(|existing| {
            let existing_end = existing.base + existing.length;
            memory.base < existing_end && existing.base < end
        }) {
            return Err(ProfileError::OverlappingMemory);
        }
        let slot = self
            .memory
            .get_mut(self.memory_count)
            .ok_or(ProfileError::CapacityExceeded)?;
        *slot = memory;
        self.memory_count += 1;
        Ok(())
    }

    pub fn push_io(&mut self, io: IoProfile) -> Result<(), ProfileError> {
        if io.device >= 32 || io.function >= 8 {
            return Err(ProfileError::InvalidDeviceAddress);
        }
        if self.io().iter().any(|existing| {
            (
                existing.segment,
                existing.bus,
                existing.device,
                existing.function,
            ) == (io.segment, io.bus, io.device, io.function)
        }) {
            return Err(ProfileError::DuplicateDevice);
        }
        let slot = self
            .io
            .get_mut(self.io_count)
            .ok_or(ProfileError::CapacityExceeded)?;
        *slot = io;
        self.io_count += 1;
        Ok(())
    }

    pub fn cpus(&self) -> &[CpuProfile] {
        &self.cpus[..self.cpu_count]
    }

    pub fn memory(&self) -> &[MemoryProfile] {
        &self.memory[..self.memory_count]
    }

    pub fn io(&self) -> &[IoProfile] {
        &self.io[..self.io_count]
    }

    pub fn traits(&self) -> MachineTraits {
        let enabled = self.cpus().iter().filter(|cpu| cpu.enabled);
        let mut cpu_count = 0_u16;
        let mut first_numa = None;
        let mut first_kind = None;
        let mut numa = false;
        let mut heterogeneous = false;
        let mut offload = false;
        for cpu in enabled {
            cpu_count = cpu_count.saturating_add(1);
            numa |= first_numa.is_some_and(|domain| domain != cpu.numa_domain);
            heterogeneous |= first_kind.is_some_and(|kind| kind != cpu.kind);
            first_numa.get_or_insert(cpu.numa_domain);
            first_kind.get_or_insert(cpu.kind);
            offload |= cpu.kind == CpuKind::Offload;
        }
        MachineTraits {
            cpu_count,
            symmetric_multiprocessing: cpu_count > 1,
            numa,
            heterogeneous,
            offload,
            persistent_memory: self
                .memory()
                .iter()
                .any(|region| region.kind == MemoryKind::Persistent),
            shared_memory: self
                .memory()
                .iter()
                .any(|region| region.kind == MemoryKind::Shared),
        }
    }
}

impl Default for MachineProfile {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MachineTraits {
    pub cpu_count: u16,
    pub symmetric_multiprocessing: bool,
    pub numa: bool,
    pub heterogeneous: bool,
    pub offload: bool,
    pub persistent_memory: bool,
    pub shared_memory: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileError {
    CapacityExceeded,
    DuplicateCpu,
    InvalidRange,
    OverlappingMemory,
    InvalidDeviceAddress,
    DuplicateDevice,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_profile_from_validated_real_descriptors() {
        let mut profile = MachineProfile::new();
        profile
            .push_cpu(CpuProfile {
                hardware_id: 4,
                firmware_id: 9,
                package: 0,
                cluster: 1,
                core: 2,
                thread: 0,
                numa_domain: 0,
                kind: CpuKind::Symmetric,
                enabled: true,
            })
            .unwrap();
        profile
            .push_memory(MemoryProfile {
                base: 0x1000,
                length: 0x4000,
                numa_domain: 0,
                kind: MemoryKind::Ram,
            })
            .unwrap();
        assert_eq!(profile.cpus().len(), 1);
        assert_eq!(profile.memory().len(), 1);
        assert_eq!(profile.traits().cpu_count, 1);
    }

    #[test]
    fn rejects_duplicate_cpus_and_overlapping_memory() {
        let mut profile = MachineProfile::new();
        let cpu = CpuProfile {
            hardware_id: 1,
            firmware_id: 1,
            package: 0,
            cluster: 0,
            core: 0,
            thread: 0,
            numa_domain: 0,
            kind: CpuKind::Symmetric,
            enabled: true,
        };
        profile.push_cpu(cpu).unwrap();
        assert_eq!(profile.push_cpu(cpu), Err(ProfileError::DuplicateCpu));
        profile
            .push_memory(MemoryProfile {
                base: 0x1000,
                length: 0x2000,
                numa_domain: 0,
                kind: MemoryKind::Ram,
            })
            .unwrap();
        assert_eq!(
            profile.push_memory(MemoryProfile {
                base: 0x2000,
                length: 0x1000,
                numa_domain: 0,
                kind: MemoryKind::Reserved,
            }),
            Err(ProfileError::OverlappingMemory)
        );
    }
}
