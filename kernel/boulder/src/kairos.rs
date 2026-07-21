use ::kairos::abi::{ABI_KIND_DRIVER, AbiDescriptor, NegotiationError, negotiate};
use ::kairos::object::{ObjectKind, ObjectTable, Rights};
use ::kairos::profile::{
    CpuKind, CpuProfile, IoProfile, MachineProfile, MachineTraits, MemoryKind, MemoryProfile,
    ProfileError,
};
use ::kairos::topology::{DomainGraph, TopologyError};
use abyss::memory::{MemoryMap, MemoryRegionKind};

use crate::boot::acpi::MadtInfo;
use crate::capability::{Capability, MachineProfileControl};
use crate::hw::pci::PciInventory;
use crate::sync::SpinLock;

struct State {
    profile: MachineProfile,
    graph: DomainGraph,
    initialized: bool,
}

impl State {
    const fn new() -> Self {
        Self {
            profile: MachineProfile::new(),
            graph: DomainGraph::new(),
            initialized: false,
        }
    }
}

static STATE: SpinLock<State> = SpinLock::new(State::new());
static OBJECTS: ObjectTable<64> = ObjectTable::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Summary {
    pub processors: usize,
    pub memory_regions: usize,
    pub io_devices: usize,
    pub domains: usize,
    pub traits: MachineTraits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializeError {
    AlreadyInitialized,
    Profile(ProfileError),
    Topology(TopologyError),
    Abi(NegotiationError),
    ObjectSelfTest,
}

pub fn initialize(
    madt: &MadtInfo,
    memory_map: &MemoryMap,
    pci: &PciInventory,
    _authority: &Capability<'_, MachineProfileControl>,
) -> Result<Summary, InitializeError> {
    let mut state = STATE.lock();
    if state.initialized {
        return Err(InitializeError::AlreadyInitialized);
    }

    for (logical_id, processor) in madt.processors().iter().enumerate() {
        state
            .profile
            .push_cpu(CpuProfile {
                hardware_id: processor.apic_id,
                firmware_id: processor.firmware_uid,
                package: 0,
                cluster: 0,
                core: logical_id as u16,
                thread: 0,
                numa_domain: 0,
                kind: CpuKind::Symmetric,
                enabled: processor.enabled,
            })
            .map_err(InitializeError::Profile)?;
    }
    for region in memory_map.regions() {
        state
            .profile
            .push_memory(MemoryProfile {
                base: region.start.as_u64(),
                length: region.length(),
                numa_domain: 0,
                kind: match region.kind {
                    MemoryRegionKind::Usable => MemoryKind::Ram,
                    MemoryRegionKind::AcpiReclaimable | MemoryRegionKind::AcpiNonVolatile => {
                        MemoryKind::Firmware
                    }
                    MemoryRegionKind::Reserved | MemoryRegionKind::Defective => {
                        MemoryKind::Reserved
                    }
                },
            })
            .map_err(InitializeError::Profile)?;
    }
    for device in pci.devices() {
        state
            .profile
            .push_io(IoProfile {
                segment: 0,
                bus: device.address.bus,
                device: device.address.slot,
                function: device.address.function,
                class: device.class_code,
                subclass: device.subclass,
                vendor_id: device.vendor_id,
                device_id: device.device_id,
                interrupt: if device.interrupt_line == 0xff {
                    u32::MAX
                } else {
                    u32::from(device.interrupt_line)
                },
            })
            .map_err(InitializeError::Profile)?;
    }

    let State { profile, graph, .. } = &mut *state;
    graph.rebuild(profile).map_err(InitializeError::Topology)?;

    let native = AbiDescriptor::native(0b111, 0);
    let mut driver_request = native;
    driver_request.abi_kind = ABI_KIND_DRIVER;
    driver_request.features_low = 0b011;
    negotiate(native, driver_request).map_err(InitializeError::Abi)?;

    let object = OBJECTS
        .allocate(ObjectKind::Device, 1, Rights::READ.union(Rights::CONTROL))
        .map_err(|_| InitializeError::ObjectSelfTest)?;
    let view = OBJECTS
        .duplicate(&object, Rights::READ)
        .map_err(|_| InitializeError::ObjectSelfTest)?;
    if OBJECTS.resolve(&view, Rights::READ).is_err()
        || OBJECTS.resolve(&view, Rights::CONTROL).is_ok()
        || OBJECTS.close(view) != Ok(false)
        || OBJECTS.close(object) != Ok(true)
    {
        return Err(InitializeError::ObjectSelfTest);
    }

    state.initialized = true;
    let traits = state.profile.traits();
    Ok(Summary {
        processors: state.profile.cpus().len(),
        memory_regions: state.profile.memory().len(),
        io_devices: state.profile.io().len(),
        domains: state.graph.domains().len(),
        traits,
    })
}
