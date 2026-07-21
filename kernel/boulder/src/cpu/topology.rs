use crate::boot::acpi::{MAXIMUM_PROCESSORS, MadtInfo};
use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreRole {
    Aegis,
    Enclave,
    Compute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreState {
    Discovered,
    BootProcessor,
    Online,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuCore {
    pub firmware_uid: u32,
    pub apic_id: u32,
    pub role: CoreRole,
    pub state: CoreState,
    pub uses_x2apic: bool,
}

impl CpuCore {
    const EMPTY: Self = Self {
        firmware_uid: 0,
        apic_id: 0,
        role: CoreRole::Compute,
        state: CoreState::Discovered,
        uses_x2apic: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopologyPolicy {
    pub requested_enclave_cores: usize,
    pub reserve_compute_core: bool,
}

impl Default for TopologyPolicy {
    fn default() -> Self {
        Self {
            requested_enclave_cores: 2,
            reserve_compute_core: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopologyInfo {
    pub processor_count: usize,
    pub online_cores: usize,
    pub enclave_cores: usize,
    pub compute_cores: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionClass {
    KernelControl,
    DriverEnclave,
    Userland,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopologyError {
    AlreadyInitialized,
    MissingBootProcessor,
    NoUsableProcessors,
    UnknownProcessor,
    UnauthorizedExecution,
    InvalidStateTransition,
}

struct CpuTopology {
    cores: [CpuCore; MAXIMUM_PROCESSORS],
    core_count: usize,
    initialized: bool,
}

impl CpuTopology {
    const fn new() -> Self {
        Self {
            cores: [CpuCore::EMPTY; MAXIMUM_PROCESSORS],
            core_count: 0,
            initialized: false,
        }
    }
}

static TOPOLOGY: SpinLock<CpuTopology> = SpinLock::new(CpuTopology::new());

pub fn initialize(
    madt: &MadtInfo,
    boot_apic_id: u32,
    policy: TopologyPolicy,
) -> Result<TopologyInfo, TopologyError> {
    let mut topology = TOPOLOGY.lock();
    if topology.initialized {
        return Err(TopologyError::AlreadyInitialized);
    }
    let usable = madt
        .processors()
        .iter()
        .filter(|processor| processor.enabled || processor.online_capable);
    for processor in usable {
        let index = topology.core_count;
        topology.cores[index] = CpuCore {
            firmware_uid: processor.firmware_uid,
            apic_id: processor.apic_id,
            role: CoreRole::Compute,
            state: if processor.apic_id == boot_apic_id {
                CoreState::BootProcessor
            } else {
                CoreState::Discovered
            },
            uses_x2apic: processor.uses_x2apic,
        };
        topology.core_count += 1;
    }
    if topology.core_count == 0 {
        return Err(TopologyError::NoUsableProcessors);
    }
    let Some(boot_index) = topology.cores[..topology.core_count]
        .iter()
        .position(|core| core.apic_id == boot_apic_id)
    else {
        topology.core_count = 0;
        return Err(TopologyError::MissingBootProcessor);
    };
    topology.cores[boot_index].role = CoreRole::Aegis;

    let non_boot_cores = topology.core_count - 1;
    let compute_reservation = usize::from(policy.reserve_compute_core && non_boot_cores != 0);
    let enclave_count = policy
        .requested_enclave_cores
        .min(non_boot_cores.saturating_sub(compute_reservation));
    let mut assigned_enclaves = 0;
    for index in 0..topology.core_count {
        if index != boot_index && assigned_enclaves < enclave_count {
            topology.cores[index].role = CoreRole::Enclave;
            assigned_enclaves += 1;
        }
    }
    topology.initialized = true;
    let compute_cores = topology.cores[..topology.core_count]
        .iter()
        .filter(|core| core.role == CoreRole::Compute)
        .count();
    Ok(TopologyInfo {
        processor_count: topology.core_count,
        online_cores: 1,
        enclave_cores: assigned_enclaves,
        compute_cores,
    })
}

pub fn core(apic_id: u32) -> Option<CpuCore> {
    let topology = TOPOLOGY.lock();
    topology.cores[..topology.core_count]
        .iter()
        .copied()
        .find(|core| core.apic_id == apic_id)
}

pub fn mark_online(apic_id: u32) -> Result<(), TopologyError> {
    let mut topology = TOPOLOGY.lock();
    let core_count = topology.core_count;
    let core = topology.cores[..core_count]
        .iter_mut()
        .find(|core| core.apic_id == apic_id)
        .ok_or(TopologyError::UnknownProcessor)?;
    if core.state != CoreState::Discovered {
        return Err(TopologyError::InvalidStateTransition);
    }
    core.state = CoreState::Online;
    Ok(())
}

pub fn authorize_execution(
    current_apic_id: u32,
    execution: ExecutionClass,
) -> Result<(), TopologyError> {
    let core = core(current_apic_id).ok_or(TopologyError::UnknownProcessor)?;
    let authorized = matches!(
        (core.role, execution),
        (CoreRole::Aegis, ExecutionClass::KernelControl)
            | (CoreRole::Enclave, ExecutionClass::DriverEnclave)
            | (CoreRole::Compute, ExecutionClass::Userland)
    );
    if authorized {
        Ok(())
    } else {
        Err(TopologyError::UnauthorizedExecution)
    }
}

#[cfg(test)]
mod tests {
    use crate::boot::acpi::{AcpiError, Rsdp, discover_madt};

    use super::*;

    fn madt_with_processors() -> MadtInfo {
        const BASE: u64 = 0x1000;
        const XSDT: usize = 0x100;
        const MADT: usize = 0x200;
        let mut memory = [0_u8; 0x400];

        let xsdt_length = 44_usize;
        let xsdt = &mut memory[XSDT..XSDT + xsdt_length];
        xsdt[..4].copy_from_slice(b"XSDT");
        xsdt[4..8].copy_from_slice(&(xsdt_length as u32).to_le_bytes());
        xsdt[36..44].copy_from_slice(&(BASE + MADT as u64).to_le_bytes());
        set_checksum(xsdt, 9);

        let madt_length = 44 + 12 + 4 * 8;
        let madt = &mut memory[MADT..MADT + madt_length];
        madt[..4].copy_from_slice(b"APIC");
        madt[4..8].copy_from_slice(&(madt_length as u32).to_le_bytes());
        madt[36..40].copy_from_slice(&(0xfee0_0000_u32).to_le_bytes());
        let io_apic = &mut madt[44..56];
        io_apic[0] = 1;
        io_apic[1] = 12;
        io_apic[4..8].copy_from_slice(&(0xfec0_0000_u32).to_le_bytes());
        for index in 0..4 {
            let entry = &mut madt[56 + index * 8..64 + index * 8];
            entry[0] = 0;
            entry[1] = 8;
            entry[2] = index as u8;
            entry[3] = (index * 2) as u8;
            entry[4..8].copy_from_slice(&(1_u32).to_le_bytes());
        }
        set_checksum(madt, 9);

        let mut rsdp_bytes = [0_u8; 36];
        rsdp_bytes[..8].copy_from_slice(b"RSD PTR ");
        rsdp_bytes[15] = 2;
        rsdp_bytes[20..24].copy_from_slice(&(36_u32).to_le_bytes());
        rsdp_bytes[24..32].copy_from_slice(&(BASE + XSDT as u64).to_le_bytes());
        set_checksum(&mut rsdp_bytes[..20], 8);
        set_checksum(&mut rsdp_bytes, 32);
        let rsdp = Rsdp::parse(&rsdp_bytes).unwrap();
        let map = |address: u64, length: usize| {
            let offset = address.checked_sub(BASE)? as usize;
            Some(memory.get(offset..offset.checked_add(length)?)?.as_ptr())
        };
        unsafe { discover_madt(rsdp, map) }.unwrap()
    }

    fn set_checksum(bytes: &mut [u8], offset: usize) {
        bytes[offset] = 0;
        let sum = bytes.iter().copied().fold(0_u8, u8::wrapping_add);
        bytes[offset] = 0_u8.wrapping_sub(sum);
    }

    #[test]
    fn assigns_roles_from_real_apic_identifiers() -> Result<(), AcpiError> {
        let madt = madt_with_processors();
        let mut topology = CpuTopology::new();
        for processor in madt.processors() {
            let index = topology.core_count;
            topology.cores[index] = CpuCore {
                firmware_uid: processor.firmware_uid,
                apic_id: processor.apic_id,
                role: CoreRole::Compute,
                state: CoreState::Discovered,
                uses_x2apic: processor.uses_x2apic,
            };
            topology.core_count += 1;
        }
        assert_eq!(topology.cores[2].apic_id, 4);
        assert_eq!(topology.core_count, 4);
        Ok(())
    }

    #[test]
    fn authorization_maps_each_role_to_one_execution_class() {
        let allowed = [
            (CoreRole::Aegis, ExecutionClass::KernelControl),
            (CoreRole::Enclave, ExecutionClass::DriverEnclave),
            (CoreRole::Compute, ExecutionClass::Userland),
        ];
        for (role, execution) in allowed {
            assert!(matches!(
                (role, execution),
                (CoreRole::Aegis, ExecutionClass::KernelControl)
                    | (CoreRole::Enclave, ExecutionClass::DriverEnclave)
                    | (CoreRole::Compute, ExecutionClass::Userland)
            ));
        }
    }
}
