//! Userland access to Boulder's Kairos machine-topology and ABI services.
//!
//! The fixed-size representations in this module intentionally avoid a heap.
//! They mirror the limits in `core/kairos` and are suitable for process-wide
//! storage initialized before worker threads are started.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

pub use ::kairos::abi::{ABI_KIND_DRIVER, ABI_KIND_NATIVE, ABI_MAGIC, ABI_VERSION};
pub use ::kairos::profile::{MAXIMUM_CPUS, MAXIMUM_IO_DEVICES};
pub use ::kairos::topology::MAXIMUM_DOMAINS;
use ::kairos::wire::{AbiReply, AbiRequest, RawTopologyReply};
pub use ::kairos::wire::{MAXIMUM_CPUS_PER_DOMAIN, features, trait_flags};

use crate::SyscallError;
use crate::syscall;
use crate::syscalls::{SYS_DISP_LEASE, SYS_DISP_QUERY};

// These are compatibility aliases for the currently assigned Kairos calls.
// Keeping the aliases here gives the kernel syscall table one source number
// while allowing the public API to use names meaningful to Kairos clients.
pub const SYS_KAIROS_QUERY: usize = SYS_DISP_QUERY;
pub const SYS_KAIROS_ABI: usize = SYS_DISP_LEASE;

/// Kernel-written, cache-line-sized summary page.
#[repr(C, align(64))]
pub struct KairosPage {
    pub cpu_count: AtomicU16,
    pub domain_count: AtomicU16,
    pub io_device_count: AtomicU16,
    pub numa_domains: AtomicU16,
    pub flags: AtomicU32,
    pub abi_features_lo: AtomicU32,
    pub abi_features_hi: AtomicU32,
    pub boot_epoch: AtomicU32,
    _reserved: [u8; 36],
}

const _: () = assert!(core::mem::size_of::<KairosPage>() == 64);

impl KairosPage {
    pub const fn zeroed() -> Self {
        Self {
            cpu_count: AtomicU16::new(0),
            domain_count: AtomicU16::new(0),
            io_device_count: AtomicU16::new(0),
            numa_domains: AtomicU16::new(0),
            flags: AtomicU32::new(0),
            abi_features_lo: AtomicU32::new(0),
            abi_features_hi: AtomicU32::new(0),
            boot_epoch: AtomicU32::new(0),
            _reserved: [0; 36],
        }
    }

    pub fn cpu_count(&self) -> u16 {
        self.cpu_count.load(Ordering::Acquire)
    }

    pub fn domain_count(&self) -> u16 {
        self.domain_count.load(Ordering::Acquire)
    }

    pub fn io_device_count(&self) -> u16 {
        self.io_device_count.load(Ordering::Acquire)
    }

    pub fn numa_domains(&self) -> u16 {
        self.numa_domains.load(Ordering::Acquire)
    }

    pub fn boot_epoch(&self) -> u32 {
        self.boot_epoch.load(Ordering::Acquire)
    }

    pub fn is_smp(&self) -> bool {
        self.has_trait(trait_flags::SMP)
    }

    pub fn is_numa(&self) -> bool {
        self.has_trait(trait_flags::NUMA)
    }

    pub fn is_heterogeneous(&self) -> bool {
        self.has_trait(trait_flags::HETEROGENEOUS)
    }

    pub fn has_offload(&self) -> bool {
        self.has_trait(trait_flags::OFFLOAD)
    }

    pub fn abi_features(&self) -> u64 {
        let low = u64::from(self.abi_features_lo.load(Ordering::Acquire));
        let high = u64::from(self.abi_features_hi.load(Ordering::Acquire));
        low | high << 32
    }

    pub fn is_ready(&self) -> bool {
        self.boot_epoch() != 0
    }

    fn has_trait(&self, flag: u32) -> bool {
        self.flags.load(Ordering::Acquire) & flag != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CpuKind {
    Symmetric = 0,
    Performance = 1,
    Efficiency = 2,
    Offload = 3,
    Unknown = 4,
}

impl CpuKind {
    pub const fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Symmetric,
            1 => Self::Performance,
            2 => Self::Efficiency,
            3 => Self::Offload,
            _ => Self::Unknown,
        }
    }

    pub const fn is_compute_capable(self) -> bool {
        matches!(self, Self::Symmetric | Self::Performance)
    }

    pub const fn is_offload_only(self) -> bool {
        matches!(self, Self::Offload)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuDescriptor {
    pub logical_id: u16,
    pub hardware_id: u32,
    pub package: u16,
    pub core: u16,
    pub thread: u16,
    pub numa_domain: u16,
    pub kind: CpuKind,
    pub enabled: bool,
}

impl CpuDescriptor {
    const EMPTY: Self = Self {
        logical_id: u16::MAX,
        hardware_id: 0,
        package: 0,
        core: 0,
        thread: 0,
        numa_domain: 0,
        kind: CpuKind::Unknown,
        enabled: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DomainKind {
    Machine = 0,
    Numa = 1,
}

#[derive(Clone, Copy, Debug)]
pub struct DomainDescriptor {
    pub id: u16,
    pub kind: DomainKind,
    pub parent: Option<u16>,
    members: [u16; MAXIMUM_CPUS_PER_DOMAIN],
    member_count: usize,
}

impl DomainDescriptor {
    const EMPTY: Self = Self {
        id: u16::MAX,
        kind: DomainKind::Machine,
        parent: None,
        members: [u16::MAX; MAXIMUM_CPUS_PER_DOMAIN],
        member_count: 0,
    };

    pub fn members(&self) -> &[u16] {
        &self.members[..self.member_count]
    }
}

pub struct TopologySnapshot {
    cpus: [CpuDescriptor; MAXIMUM_CPUS],
    cpu_count: usize,
    domains: [DomainDescriptor; MAXIMUM_DOMAINS],
    domain_count: usize,
    trait_flags: u32,
}

struct RawQuerySlot {
    claimed: AtomicBool,
    reply: UnsafeCell<RawTopologyReply>,
}

impl RawQuerySlot {
    const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            reply: UnsafeCell::new(RawTopologyReply::zeroed()),
        }
    }

    fn acquire(&self) -> Option<RawQueryGuard<'_>> {
        self.claimed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| RawQueryGuard { slot: self })
    }
}

// SAFETY: `claimed` gives the single mutable user exclusive access to `reply`.
unsafe impl Sync for RawQuerySlot {}

struct RawQueryGuard<'a> {
    slot: &'a RawQuerySlot,
}

impl RawQueryGuard<'_> {
    fn as_mut_ptr(&mut self) -> *mut RawTopologyReply {
        self.slot.reply.get()
    }

    fn reply(&self) -> &RawTopologyReply {
        // SAFETY: This guard holds the slot's exclusive claim and exposes no
        // mutable reference while the returned shared reference is live.
        unsafe { &*self.slot.reply.get() }
    }
}

impl Drop for RawQueryGuard<'_> {
    fn drop(&mut self) {
        self.slot.claimed.store(false, Ordering::Release);
    }
}

static RAW_QUERY_SLOT: RawQuerySlot = RawQuerySlot::new();

impl TopologySnapshot {
    const fn empty() -> Self {
        Self {
            cpus: [CpuDescriptor::EMPTY; MAXIMUM_CPUS],
            cpu_count: 0,
            domains: [DomainDescriptor::EMPTY; MAXIMUM_DOMAINS],
            domain_count: 0,
            trait_flags: 0,
        }
    }

    /// Queries the kernel once and validates the complete fixed-size reply.
    ///
    /// This value is intentionally large. Call it during early process setup
    /// and retain the returned snapshot instead of repeatedly constructing it.
    pub fn query() -> Result<Self, KairosError> {
        let mut slot = RAW_QUERY_SLOT.acquire().ok_or(KairosError::Busy)?;
        let arguments = [
            slot.as_mut_ptr() as usize,
            core::mem::size_of::<RawTopologyReply>(),
            0,
            0,
            0,
            0,
        ];
        // SAFETY: The process-wide slot is initialized, exclusively claimed,
        // writable for the advertised length, and alive for the entire call.
        unsafe { syscall(SYS_KAIROS_QUERY, arguments) }.map_err(KairosError::Syscall)?;
        Self::from_raw(slot.reply())
    }

    fn from_raw(reply: &RawTopologyReply) -> Result<Self, KairosError> {
        let cpu_count =
            usize::try_from(reply.header.cpu_count).map_err(|_| KairosError::InvalidTopology)?;
        let domain_count =
            usize::try_from(reply.header.domain_count).map_err(|_| KairosError::InvalidTopology)?;
        if cpu_count == 0
            || domain_count == 0
            || cpu_count > MAXIMUM_CPUS
            || domain_count > MAXIMUM_DOMAINS
        {
            return Err(KairosError::InvalidTopology);
        }

        let mut snapshot = Self::empty();
        for (index, raw) in reply.cpus[..cpu_count].iter().enumerate() {
            if snapshot.cpus[..index]
                .iter()
                .any(|cpu| cpu.logical_id == raw.logical_id)
            {
                return Err(KairosError::InvalidTopology);
            }
            snapshot.cpus[index] = CpuDescriptor {
                logical_id: raw.logical_id,
                hardware_id: raw.hardware_id,
                package: raw.package,
                core: raw.core,
                thread: raw.thread,
                numa_domain: raw.numa_domain,
                kind: CpuKind::from_raw(raw.kind),
                enabled: raw.enabled != 0,
            };
        }
        snapshot.cpu_count = cpu_count;

        for (index, raw) in reply.domains[..domain_count].iter().enumerate() {
            let member_count = usize::from(raw.member_count);
            if member_count > MAXIMUM_CPUS_PER_DOMAIN
                || raw.kind > DomainKind::Numa as u8
                || snapshot.domains[..index]
                    .iter()
                    .any(|domain| domain.id == raw.id)
                || raw.members[..member_count].iter().enumerate().any(
                    |(member_index, logical_id)| raw.members[..member_index].contains(logical_id),
                )
                || raw.members[..member_count].iter().any(|logical_id| {
                    !snapshot.cpus[..cpu_count]
                        .iter()
                        .any(|cpu| cpu.logical_id == *logical_id)
                })
            {
                return Err(KairosError::InvalidTopology);
            }

            let mut domain = DomainDescriptor::EMPTY;
            domain.id = raw.id;
            domain.kind = if raw.kind == DomainKind::Numa as u8 {
                DomainKind::Numa
            } else {
                DomainKind::Machine
            };
            domain.parent = (raw.parent_valid != 0).then_some(raw.parent_id);
            domain.members[..member_count].copy_from_slice(&raw.members[..member_count]);
            domain.member_count = member_count;
            snapshot.domains[index] = domain;
        }

        if snapshot.domains[..domain_count].iter().any(|domain| {
            domain.parent.is_some_and(|parent| {
                !snapshot.domains[..domain_count]
                    .iter()
                    .any(|candidate| candidate.id == parent)
            })
        }) {
            return Err(KairosError::InvalidTopology);
        }

        snapshot.domain_count = domain_count;
        snapshot.trait_flags = reply.header.trait_flags;
        Ok(snapshot)
    }

    pub fn cpus(&self) -> &[CpuDescriptor] {
        &self.cpus[..self.cpu_count]
    }

    pub fn domains(&self) -> &[DomainDescriptor] {
        &self.domains[..self.domain_count]
    }

    pub fn is_smp(&self) -> bool {
        self.has_trait(trait_flags::SMP)
    }

    pub fn is_numa(&self) -> bool {
        self.has_trait(trait_flags::NUMA)
    }

    pub fn is_heterogeneous(&self) -> bool {
        self.has_trait(trait_flags::HETEROGENEOUS)
    }

    pub fn has_offload(&self) -> bool {
        self.has_trait(trait_flags::OFFLOAD)
    }

    pub fn numa_domain_of(&self, logical_cpu_id: u16) -> Option<u16> {
        self.cpus()
            .iter()
            .find(|cpu| cpu.logical_id == logical_cpu_id)
            .map(|cpu| cpu.numa_domain)
    }

    pub fn numa_local_cpus(&self, domain_id: u16) -> NucleusCpuIter<'_> {
        NucleusCpuIter {
            snapshot: self,
            domain_id,
            index: 0,
        }
    }

    pub fn compute_affinity(&self, class: WorkloadClass) -> CpuAffinityHint {
        let preferred_kind = match class {
            WorkloadClass::Compute => CpuKind::Performance,
            WorkloadClass::Io => CpuKind::Efficiency,
            WorkloadClass::Offload => CpuKind::Offload,
            WorkloadClass::Any => return CpuAffinityHint::any(),
        };

        for domain in self.domains() {
            if class != WorkloadClass::Offload && domain.kind != DomainKind::Numa {
                continue;
            }
            if self.domain_has_enabled_kind(domain, preferred_kind) {
                return CpuAffinityHint {
                    domain_id: domain.id,
                    preferred_kind,
                    smp_eligible: class != WorkloadClass::Offload
                        && (class == WorkloadClass::Io || self.is_smp()),
                };
            }
        }
        CpuAffinityHint::any()
    }

    /// Partitions work proportionally to the number of enabled CPUs in each
    /// NUMA domain. Any rounding remainder is assigned in domain order.
    pub fn partition_work(&self, total: usize) -> WorkPartition {
        let mut partition = WorkPartition::empty();
        let mut domain_ids = [0_u16; MAXIMUM_DOMAINS];
        let mut weights = [0_usize; MAXIMUM_DOMAINS];
        let mut domain_count = 0;
        let mut enabled_total = 0;
        for domain in self
            .domains()
            .iter()
            .filter(|domain| domain.kind == DomainKind::Numa)
        {
            let weight = self.enabled_member_count(domain);
            if weight == 0 {
                continue;
            }
            domain_ids[domain_count] = domain.id;
            weights[domain_count] = weight;
            domain_count += 1;
            enabled_total += weight;
        }

        if enabled_total == 0 {
            partition.slices[0] = Some(WorkSlice {
                domain_id: 0,
                start: 0,
                end: total,
            });
            partition.count = 1;
            return partition;
        }

        let mut chunks = [0_usize; MAXIMUM_DOMAINS];
        let quotient = total / enabled_total;
        let residual = total % enabled_total;
        let mut allocated = 0;
        for index in 0..domain_count {
            chunks[index] = quotient * weights[index] + residual * weights[index] / enabled_total;
            allocated += chunks[index];
        }
        for chunk in chunks[..total - allocated].iter_mut() {
            *chunk += 1;
        }

        let mut cursor = 0;
        for index in 0..domain_count {
            let chunk = chunks[index];
            let end = cursor + chunk;
            partition.slices[partition.count] = Some(WorkSlice {
                domain_id: domain_ids[index],
                start: cursor,
                end,
            });
            partition.count += 1;
            cursor = end;
        }
        partition
    }

    pub fn domain_iter(&self) -> DomainIterator<'_> {
        DomainIterator {
            snapshot: self,
            index: 0,
        }
    }

    fn has_trait(&self, flag: u32) -> bool {
        self.trait_flags & flag != 0
    }

    fn enabled_member_count(&self, domain: &DomainDescriptor) -> usize {
        domain
            .members()
            .iter()
            .filter(|logical_id| {
                self.cpus()
                    .iter()
                    .any(|cpu| cpu.logical_id == **logical_id && cpu.enabled)
            })
            .count()
    }

    fn domain_has_enabled_kind(&self, domain: &DomainDescriptor, kind: CpuKind) -> bool {
        domain.members().iter().any(|logical_id| {
            self.cpus()
                .iter()
                .any(|cpu| cpu.logical_id == *logical_id && cpu.enabled && cpu.kind == kind)
        })
    }
}

pub struct NucleusCpuIter<'a> {
    snapshot: &'a TopologySnapshot,
    domain_id: u16,
    index: usize,
}

impl<'a> Iterator for NucleusCpuIter<'a> {
    type Item = &'a CpuDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.snapshot.cpu_count {
            let cpu = &self.snapshot.cpus[self.index];
            self.index += 1;
            if cpu.enabled && cpu.numa_domain == self.domain_id {
                return Some(cpu);
            }
        }
        None
    }
}

pub struct DomainIterator<'a> {
    snapshot: &'a TopologySnapshot,
    index: usize,
}

impl<'a> Iterator for DomainIterator<'a> {
    type Item = &'a DomainDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        let domain = self.snapshot.domains().get(self.index)?;
        self.index += 1;
        Some(domain)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkloadClass {
    Compute,
    Io,
    Offload,
    Any,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuAffinityHint {
    pub domain_id: u16,
    pub preferred_kind: CpuKind,
    pub smp_eligible: bool,
}

impl CpuAffinityHint {
    pub const fn any() -> Self {
        Self {
            domain_id: 0,
            preferred_kind: CpuKind::Symmetric,
            smp_eligible: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkSlice {
    pub domain_id: u16,
    pub start: usize,
    pub end: usize,
}

impl WorkSlice {
    pub const fn len(&self) -> usize {
        self.end - self.start
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct WorkPartition {
    pub slices: [Option<WorkSlice>; MAXIMUM_DOMAINS],
    pub count: usize,
}

impl WorkPartition {
    const fn empty() -> Self {
        Self {
            slices: [None; MAXIMUM_DOMAINS],
            count: 0,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &WorkSlice> {
        self.slices[..self.count].iter().filter_map(Option::as_ref)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedFeatures {
    pub granted_lo: u64,
    pub granted_hi: u64,
    pub unavailable_lo: u64,
    pub unavailable_hi: u64,
}

impl NegotiatedFeatures {
    pub const fn has(&self, feature: u64) -> bool {
        self.granted_lo & feature != 0
    }

    pub const fn missing(&self, feature: u64) -> bool {
        self.unavailable_lo & feature != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbiError {
    Syscall(SyscallError),
    InvalidReply,
    RequiredFeatureMissing { missing_lo: u64, missing_hi: u64 },
}

pub struct AbiNegotiator {
    required_lo: u64,
    required_hi: u64,
    optional_lo: u64,
    optional_hi: u64,
    abi_kind: u8,
}

impl AbiNegotiator {
    pub const fn native() -> Self {
        Self {
            required_lo: features::SYSCALL_BASIC,
            required_hi: 0,
            optional_lo: 0,
            optional_hi: 0,
            abi_kind: ABI_KIND_NATIVE,
        }
    }

    pub const fn driver() -> Self {
        Self {
            required_lo: features::SYSCALL_BASIC | features::SYSCALL_DRIVER,
            required_hi: 0,
            optional_lo: 0,
            optional_hi: 0,
            abi_kind: ABI_KIND_DRIVER,
        }
    }

    pub const fn require(mut self, feature: u64) -> Self {
        self.required_lo |= feature;
        self
    }

    pub const fn want(mut self, feature: u64) -> Self {
        self.optional_lo |= feature;
        self
    }

    pub fn negotiate(self) -> Result<NegotiatedFeatures, AbiError> {
        let request = AbiRequest {
            magic: ABI_MAGIC,
            version: ABI_VERSION,
            structure_size: core::mem::size_of::<AbiRequest>() as u16,
            endian: if cfg!(target_endian = "little") { 1 } else { 2 },
            word_bits: usize::BITS as u8,
            pointer_bits: usize::BITS as u8,
            abi_kind: self.abi_kind,
            page_size: 4096,
            syscall_style: 1,
            object_bits: 64,
            _pad: 0,
            features_lo_req: self.required_lo,
            features_hi_req: self.required_hi,
            features_lo_opt: self.optional_lo,
            features_hi_opt: self.optional_hi,
        };
        let mut reply = AbiReply::ZERO;
        let arguments = [
            core::ptr::addr_of!(request) as usize,
            core::mem::size_of::<AbiRequest>(),
            core::ptr::addr_of_mut!(reply) as usize,
            core::mem::size_of::<AbiReply>(),
            0,
            0,
        ];
        // SAFETY: Both ABI structures are initialized and live for the call;
        // the reply is writable for the advertised size.
        unsafe { syscall(SYS_KAIROS_ABI, arguments) }.map_err(AbiError::Syscall)?;

        if reply.status != 0
            || reply.features_lo_granted & !(self.required_lo | self.optional_lo) != 0
            || reply.features_hi_granted & !(self.required_hi | self.optional_hi) != 0
        {
            return Err(AbiError::InvalidReply);
        }

        let missing_lo = self.required_lo & !reply.features_lo_granted;
        let missing_hi = self.required_hi & !reply.features_hi_granted;
        if missing_lo != 0 || missing_hi != 0 {
            return Err(AbiError::RequiredFeatureMissing {
                missing_lo,
                missing_hi,
            });
        }

        Ok(NegotiatedFeatures {
            granted_lo: reply.features_lo_granted,
            granted_hi: reply.features_hi_granted,
            unavailable_lo: reply.features_lo_unavailable,
            unavailable_hi: reply.features_hi_unavailable,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KairosError {
    Syscall(SyscallError),
    NotReady,
    Busy,
    InvalidTopology,
}

pub struct KairosInit {
    pub topology: TopologySnapshot,
    pub features: NegotiatedFeatures,
}

impl KairosInit {
    pub fn run(required_features: u64, optional_features: u64) -> Result<Self, KairosBootError> {
        Self::run_with(
            AbiNegotiator::native(),
            required_features,
            optional_features,
        )
    }

    pub fn run_driver(
        required_features: u64,
        optional_features: u64,
    ) -> Result<Self, KairosBootError> {
        Self::run_with(
            AbiNegotiator::driver(),
            required_features,
            optional_features,
        )
    }

    fn run_with(
        negotiator: AbiNegotiator,
        required_features: u64,
        optional_features: u64,
    ) -> Result<Self, KairosBootError> {
        let features = negotiator
            .require(required_features)
            .want(optional_features)
            .negotiate()
            .map_err(KairosBootError::Abi)?;
        let topology = TopologySnapshot::query().map_err(KairosBootError::Topology)?;
        Ok(Self { topology, features })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KairosBootError {
    Abi(AbiError),
    Topology(KairosError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_snapshot(performance_cpus: usize, efficiency_cpus: usize) -> TopologySnapshot {
        let mut snapshot = TopologySnapshot::empty();
        let cpu_count = performance_cpus + efficiency_cpus;
        for index in 0..cpu_count {
            snapshot.cpus[index] = CpuDescriptor {
                logical_id: index as u16,
                hardware_id: index as u32,
                package: 0,
                core: index as u16,
                thread: 0,
                numa_domain: if index < performance_cpus { 0 } else { 1 },
                kind: if index < performance_cpus {
                    CpuKind::Performance
                } else {
                    CpuKind::Efficiency
                },
                enabled: true,
            };
        }
        snapshot.cpu_count = cpu_count;

        let mut machine = DomainDescriptor::EMPTY;
        machine.id = 0;
        for index in 0..cpu_count {
            machine.members[index] = index as u16;
        }
        machine.member_count = cpu_count;
        snapshot.domains[0] = machine;

        let mut performance = DomainDescriptor::EMPTY;
        performance.id = 1;
        performance.kind = DomainKind::Numa;
        performance.parent = Some(0);
        for index in 0..performance_cpus {
            performance.members[index] = index as u16;
        }
        performance.member_count = performance_cpus;
        snapshot.domains[1] = performance;

        let mut efficiency = DomainDescriptor::EMPTY;
        efficiency.id = 2;
        efficiency.kind = DomainKind::Numa;
        efficiency.parent = Some(0);
        for index in 0..efficiency_cpus {
            efficiency.members[index] = (performance_cpus + index) as u16;
        }
        efficiency.member_count = efficiency_cpus;
        snapshot.domains[2] = efficiency;
        snapshot.domain_count = 3;
        snapshot.trait_flags = trait_flags::SMP | trait_flags::NUMA | trait_flags::HETEROGENEOUS;
        snapshot
    }

    #[test]
    fn page_decodes_flags_and_features() {
        let page = KairosPage::zeroed();
        page.flags.store(
            trait_flags::SMP | trait_flags::NUMA | trait_flags::HETEROGENEOUS,
            Ordering::Relaxed,
        );
        page.abi_features_lo.store(0xdead, Ordering::Relaxed);
        page.abi_features_hi.store(0xbeef, Ordering::Relaxed);
        assert!(page.is_smp());
        assert!(page.is_numa());
        assert!(page.is_heterogeneous());
        assert!(!page.has_offload());
        assert_eq!(page.abi_features(), 0x0000_beef_0000_dead);
        assert!(!page.is_ready());
    }

    #[test]
    fn affinity_uses_enabled_cpu_kinds() {
        let snapshot = synthetic_snapshot(2, 2);
        assert_eq!(
            snapshot.compute_affinity(WorkloadClass::Compute),
            CpuAffinityHint {
                domain_id: 1,
                preferred_kind: CpuKind::Performance,
                smp_eligible: true,
            }
        );
        assert_eq!(
            snapshot.compute_affinity(WorkloadClass::Io).preferred_kind,
            CpuKind::Efficiency
        );
    }

    #[test]
    fn partition_is_proportional_and_exact() {
        let snapshot = synthetic_snapshot(3, 1);
        let partition = snapshot.partition_work(101);
        let mut slices = partition.iter();
        let first = slices.next().unwrap();
        let second = slices.next().unwrap();
        assert_eq!(partition.count, 2);
        assert_eq!(first.len(), 76);
        assert_eq!(second.len(), 25);
        assert_eq!(first.len() + second.len(), 101);
        assert_eq!(second.start, first.end);
    }

    #[test]
    fn raw_topology_rejects_unknown_members() {
        let mut raw = RawTopologyReply::zeroed();
        raw.header.cpu_count = 1;
        raw.cpus[0].logical_id = 7;
        raw.header.domain_count = 1;
        raw.domains[0].member_count = 1;
        raw.domains[0].members[0] = 8;
        assert!(matches!(
            TopologySnapshot::from_raw(&raw),
            Err(KairosError::InvalidTopology)
        ));
    }

    #[test]
    fn feature_queries_and_cpu_capabilities_work() {
        let negotiated = NegotiatedFeatures {
            granted_lo: features::SYSCALL_BASIC | features::ASYNC_IO,
            granted_hi: 0,
            unavailable_lo: features::HOLOGRAM_FS,
            unavailable_hi: 0,
        };
        assert!(negotiated.has(features::ASYNC_IO));
        assert!(negotiated.missing(features::HOLOGRAM_FS));
        assert!(CpuKind::Performance.is_compute_capable());
        assert!(!CpuKind::Efficiency.is_compute_capable());
        assert!(CpuKind::Offload.is_offload_only());
        assert_eq!(CpuKind::from_raw(99), CpuKind::Unknown);
    }
}
