use ::kairos::abi::{
    ABI_KIND_DRIVER, ABI_MAGIC, ABI_VERSION, AbiDescriptor, NegotiationError, negotiate,
};
use ::kairos::object::{ObjectKind, ObjectTable, Rights};
use ::kairos::profile::{
    CpuKind, CpuProfile, IoProfile, MachineProfile, MachineTraits, MemoryKind, MemoryProfile,
    ProfileError,
};
use ::kairos::topology::{DomainGraph, TopologyError};
use ::kairos::wire::{
    AbiReply, AbiRequest, RawCpuEntry, RawDomainEntry, RawTopologyHeader, features, trait_flags,
};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryError {
    NotInitialized,
    IndexOutOfRange,
}

pub fn topology_header() -> Result<RawTopologyHeader, QueryError> {
    let state = STATE.lock();
    if !state.initialized {
        return Err(QueryError::NotInitialized);
    }
    let traits = state.profile.traits();
    Ok(RawTopologyHeader {
        cpu_count: state.profile.cpus().len() as u32,
        domain_count: state.graph.domains().len() as u32,
        trait_flags: encode_traits(traits),
        _pad: 0,
    })
}

pub fn cpu_entry(index: usize) -> Result<RawCpuEntry, QueryError> {
    let state = STATE.lock();
    if !state.initialized {
        return Err(QueryError::NotInitialized);
    }
    let cpu = state
        .profile
        .cpus()
        .get(index)
        .ok_or(QueryError::IndexOutOfRange)?;
    Ok(RawCpuEntry {
        logical_id: u16::try_from(index).map_err(|_| QueryError::IndexOutOfRange)?,
        _pad0: 0,
        hardware_id: cpu.hardware_id,
        package: cpu.package,
        core: cpu.core,
        thread: cpu.thread,
        numa_domain: cpu.numa_domain,
        kind: cpu.kind as u8,
        enabled: u8::from(cpu.enabled),
        _pad1: [0; 2],
    })
}

pub fn domain_entry(index: usize) -> Result<RawDomainEntry, QueryError> {
    let state = STATE.lock();
    if !state.initialized {
        return Err(QueryError::NotInitialized);
    }
    let domain = state
        .graph
        .domains()
        .get(index)
        .ok_or(QueryError::IndexOutOfRange)?;
    let mut entry = RawDomainEntry::ZERO;
    entry.id = domain.id;
    entry.kind = domain.kind as u8;
    if let Some(parent) = domain.parent {
        entry.parent_valid = 1;
        entry.parent_id = parent;
    }
    entry.member_count = domain.members().len() as u16;
    entry.members[..domain.members().len()].copy_from_slice(domain.members());
    Ok(entry)
}

pub fn negotiate_request(request: AbiRequest) -> AbiReply {
    if request.magic != ABI_MAGIC
        || request.version != ABI_VERSION
        || usize::from(request.structure_size) != core::mem::size_of::<AbiRequest>()
    {
        return invalid_abi_reply();
    }

    let requested_low = request.features_lo_req | request.features_lo_opt;
    let requested_high = request.features_hi_req | request.features_hi_opt;
    let requested = AbiDescriptor {
        magic: request.magic,
        version: request.version,
        structure_size: core::mem::size_of::<AbiDescriptor>() as u16,
        endian: request.endian,
        word_bits: request.word_bits,
        pointer_bits: request.pointer_bits,
        abi_kind: request.abi_kind,
        page_size: request.page_size,
        syscall_style: request.syscall_style,
        object_handle_bits: request.object_bits,
        features_low: requested_low,
        features_high: requested_high,
    };
    let native = AbiDescriptor::native(features::SYSCALL_BASIC, 0);
    let Ok(negotiated) = negotiate(native, requested) else {
        return invalid_abi_reply();
    };
    AbiReply {
        features_lo_granted: negotiated.descriptor.features_low,
        features_hi_granted: negotiated.descriptor.features_high,
        features_lo_unavailable: negotiated.unavailable_features_low,
        features_hi_unavailable: negotiated.unavailable_features_high,
        status: 0,
        _pad: 0,
    }
}

const fn invalid_abi_reply() -> AbiReply {
    let mut reply = AbiReply::ZERO;
    reply.status = 1;
    reply
}

const fn encode_traits(traits: MachineTraits) -> u32 {
    let mut flags = 0;
    if traits.symmetric_multiprocessing {
        flags |= trait_flags::SMP;
    }
    if traits.numa {
        flags |= trait_flags::NUMA;
    }
    if traits.heterogeneous {
        flags |= trait_flags::HETEROGENEOUS;
    }
    if traits.offload {
        flags |= trait_flags::OFFLOAD;
    }
    if traits.persistent_memory {
        flags |= trait_flags::PERSISTENT_MEM;
    }
    if traits.shared_memory {
        flags |= trait_flags::SHARED_MEM;
    }
    flags
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

// ─── CRITICAL-MOMENT SCHEDULER ──────────────────────────────────────────────

use crate::ouroboros::{
    ExecutorHook, PhaseHint, ScheduleError, TaskId,
};

pub const FLAG_KAIROS: u32 = 1 << 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KairosWindow {
    pub opens_at: u64,
    pub closes_at: u64,
}

impl KairosWindow {
    pub const EMPTY: Self = Self {
        opens_at: 0,
        closes_at: 0,
    };

    pub const fn contains(self, tick: u64) -> bool {
        tick >= self.opens_at && tick <= self.closes_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MomentPriority {
    Background = 0,
    Normal = 1,
    Urgent = 2,
    Critical = 3,
    Singularity = 4,
}

impl MomentPriority {
    pub const fn mass(self) -> u16 {
        match self {
            Self::Background => 0x2000,
            Self::Normal => 0x5000,
            Self::Urgent => 0x9000,
            Self::Critical => 0xd000,
            Self::Singularity => 0xffff,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CriticalMoment {
    pub task: TaskId,
    pub pair_id: u64,
    pub window: KairosWindow,
    pub priority: MomentPriority,
    pub phase_bin: u16,
    pub coherence: u16,
    pub entanglement_q15: i16,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KairosBoost {
    pub task: TaskId,
    pub priority_mass: u16,
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KairosError {
    FlagMissing,
    InvalidWindow,
    Capacity,
    Scheduler(ScheduleError),
}

#[derive(Clone, Copy)]
struct MomentSlot {
    active: bool,
    moment: CriticalMoment,
}

impl MomentSlot {
    const EMPTY: Self = Self {
        active: false,
        moment: CriticalMoment {
            task: TaskId::INVALID,
            pair_id: 0,
            window: KairosWindow::EMPTY,
            priority: MomentPriority::Background,
            phase_bin: 0,
            coherence: 0,
            entanglement_q15: 0,
            flags: 0,
        },
    };
}

pub struct KairosScheduler<const N: usize> {
    slots: [MomentSlot; N],
}

impl<const N: usize> KairosScheduler<N> {
    pub const fn new() -> Self {
        Self {
            slots: [MomentSlot::EMPTY; N],
        }
    }

    pub fn offer<H: ExecutorHook>(
        &mut self,
        moment: CriticalMoment,
        now_tick: u64,
        executor: &mut H,
    ) -> Result<KairosBoost, KairosError> {
        if moment.flags & FLAG_KAIROS == 0 {
            return Err(KairosError::FlagMissing);
        }

        if moment.window.closes_at < moment.window.opens_at
            || moment.window.closes_at < now_tick
        {
            return Err(KairosError::InvalidWindow);
        }

        let index = self
            .slots
            .iter()
            .position(|slot| slot.active && slot.moment.task == moment.task)
            .or_else(|| self.slots.iter().position(|slot| !slot.active))
            .ok_or(KairosError::Capacity)?;

        let slot = &mut self.slots[index];

        slot.active = true;
        slot.moment = moment;

        let correlation = i32::from(moment.entanglement_q15)
            .unsigned_abs()
            .min(i16::MAX as u32);

        let entanglement_boost = ((correlation as u64 * 0x3000) / i16::MAX as u64) as u16;
        let priority_mass = moment
            .priority
            .mass()
            .saturating_add(entanglement_boost);

        executor
            .offer(
                moment.task,
                PhaseHint {
                    phase_bin: moment.phase_bin,
                    coherence: moment.coherence,
                    priority_mass,
                    flags: moment.flags as u16,
                },
                now_tick,
            )
            .map_err(KairosError::Scheduler)?;

        Ok(KairosBoost {
            task: moment.task,
            priority_mass,
            expires_at: moment.window.closes_at,
        })
    }

    pub fn retire_expired<H: ExecutorHook>(
        &mut self,
        now_tick: u64,
        executor: &mut H,
    ) {
        for slot in &mut self.slots {
            if slot.active && slot.moment.window.closes_at < now_tick {
                executor.complete(slot.moment.task);
                *slot = MomentSlot::EMPTY;
            }
        }
    }
}

impl<const N: usize> Default for KairosScheduler<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(required: u64, optional: u64) -> AbiRequest {
        AbiRequest {
            magic: ABI_MAGIC,
            version: ABI_VERSION,
            structure_size: core::mem::size_of::<AbiRequest>() as u16,
            endian: if cfg!(target_endian = "little") { 1 } else { 2 },
            word_bits: usize::BITS as u8,
            pointer_bits: usize::BITS as u8,
            abi_kind: ::kairos::abi::ABI_KIND_NATIVE,
            page_size: 4096,
            syscall_style: 1,
            object_bits: 64,
            _pad: 0,
            features_lo_req: required,
            features_hi_req: 0,
            features_lo_opt: optional,
            features_hi_opt: 0,
        }
    }

    #[test]
    fn abi_reply_reports_granted_and_unavailable_features() {
        let reply = negotiate_request(request(
            features::SYSCALL_BASIC,
            features::ASYNC_IO | features::HOLOGRAM_FS,
        ));
        assert_eq!(reply.status, 0);
        assert_eq!(reply.features_lo_granted, features::SYSCALL_BASIC);
        assert_eq!(
            reply.features_lo_unavailable,
            features::ASYNC_IO | features::HOLOGRAM_FS
        );
    }

    #[test]
    fn abi_reply_rejects_an_invalid_wire_version() {
        let mut invalid = request(features::SYSCALL_BASIC, 0);
        invalid.version = ABI_VERSION + 1;
        assert_ne!(negotiate_request(invalid).status, 0);
    }
}
