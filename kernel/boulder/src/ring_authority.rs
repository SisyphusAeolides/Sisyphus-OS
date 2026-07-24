//! Bounded authority for x86-64 privilege-domain dispatch.
//!
//! CPL 1 and CPL 2 remain supervisor modes under long-mode page-table U/S
//! permissions. They are therefore treated as labels, not memory boundaries:
//! every native hardware cell and personality translator owns a distinct CR3.
//! The transition frontier is intended to sit directly between a scheduling
//! decision and the architecture return trampoline. It refuses stale
//! identities, shared address spaces, invalid return mechanisms, and unscoped
//! hardware requests before machine state is committed.

use crate::process::context::valid_page_table_root;

pub const MAXIMUM_IO_PORT_RANGES: usize = 8;
pub const MAXIMUM_MMIO_RANGES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PrivilegeRing {
    Kernel = 0,
    HardwareCell = 1,
    Personality = 2,
    User = 3,
}

impl PrivilegeRing {
    pub const ALL: [Self; 4] = [
        Self::Kernel,
        Self::HardwareCell,
        Self::Personality,
        Self::User,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainRole {
    NativeHardwareCell,
    PersonalityTranslator,
    UserProcess,
}

impl DomainRole {
    pub const fn ring(self) -> PrivilegeRing {
        match self {
            Self::NativeHardwareCell => PrivilegeRing::HardwareCell,
            Self::PersonalityTranslator => PrivilegeRing::Personality,
            Self::UserProcess => PrivilegeRing::User,
        }
    }
}

/// Return instruction used by the final Ring 0 dispatch stub.
///
/// `IRETQ` can target every less-privileged ring when supplied a valid frame.
/// `SYSRETQ` is admitted only for Ring 3; x86-64 does not provide a SYSRET
/// encoding that safely targets CPL 1 or CPL 2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionGate {
    Iretq,
    Sysretq,
}

pub const fn valid_dispatch_gate(
    source: PrivilegeRing,
    target: PrivilegeRing,
    gate: TransitionGate,
) -> bool {
    if !matches!(source, PrivilegeRing::Kernel) {
        return false;
    }
    match gate {
        TransitionGate::Iretq => !matches!(target, PrivilegeRing::Kernel),
        TransitionGate::Sysretq => matches!(target, PrivilegeRing::User),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DomainHandle {
    slot: u16,
    generation: u32,
}

impl DomainHandle {
    pub const fn slot(self) -> u16 {
        self.slot
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoPortRange {
    first: u16,
    last: u16,
}

impl IoPortRange {
    pub const fn new(first: u16, last: u16) -> Result<Self, AuthorityError> {
        if first > last {
            Err(AuthorityError::InvalidIoPortRange)
        } else {
            Ok(Self { first, last })
        }
    }

    pub const fn first(self) -> u16 {
        self.first
    }

    pub const fn last(self) -> u16 {
        self.last
    }

    const fn contains(self, first: u16, byte_width: u8) -> bool {
        if !matches!(byte_width, 1 | 2 | 4) {
            return false;
        }
        match first.checked_add(byte_width as u16 - 1) {
            Some(last) => first >= self.first && last <= self.last,
            None => false,
        }
    }

    const fn overlaps(self, other: Self) -> bool {
        self.first <= other.last && other.first <= self.last
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioAccess(u8);

impl MmioAccess {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);

    pub const fn bits(self) -> u8 {
        self.0
    }

    const fn valid(self) -> bool {
        self.0 != 0 && self.0 & !Self::READ_WRITE.0 == 0
    }

    const fn contains(self, requested: Self) -> bool {
        requested.valid() && self.0 & requested.0 == requested.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioRange {
    base: u64,
    length: u64,
    access: MmioAccess,
}

impl MmioRange {
    pub const fn new(base: u64, length: u64, access: MmioAccess) -> Result<Self, AuthorityError> {
        if base & 0xfff != 0
            || length == 0
            || length & 0xfff != 0
            || base.checked_add(length).is_none()
            || !access.valid()
        {
            Err(AuthorityError::InvalidMmioRange)
        } else {
            Ok(Self {
                base,
                length,
                access,
            })
        }
    }

    pub const fn base(self) -> u64 {
        self.base
    }

    pub const fn length(self) -> u64 {
        self.length
    }

    pub const fn access(self) -> MmioAccess {
        self.access
    }

    const fn end(self) -> u64 {
        self.base + self.length
    }

    const fn contains(self, address: u64, length: u64, access: MmioAccess) -> bool {
        if length == 0 || !self.access.contains(access) {
            return false;
        }
        match address.checked_add(length) {
            Some(end) => address >= self.base && end <= self.end(),
            None => false,
        }
    }

    const fn overlaps(self, other: Self) -> bool {
        self.base < other.end() && other.base < self.end()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaGrant {
    pub iommu_domain: u64,
    pub generation: u32,
    pub requester_id: u16,
}

impl DmaGrant {
    pub const fn validate(self) -> bool {
        self.iommu_domain != 0 && self.generation != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HardwareAuthority {
    io_ports: [Option<IoPortRange>; MAXIMUM_IO_PORT_RANGES],
    mmio: [Option<MmioRange>; MAXIMUM_MMIO_RANGES],
    dma: Option<DmaGrant>,
}

impl HardwareAuthority {
    pub const NONE: Self = Self {
        io_ports: [None; MAXIMUM_IO_PORT_RANGES],
        mmio: [None; MAXIMUM_MMIO_RANGES],
        dma: None,
    };

    pub fn grant_io(&mut self, range: IoPortRange) -> Result<(), AuthorityError> {
        if self
            .io_ports
            .iter()
            .flatten()
            .any(|entry| entry.overlaps(range))
        {
            return Err(AuthorityError::OverlappingIoPortRange);
        }
        let slot = self
            .io_ports
            .iter_mut()
            .find(|entry| entry.is_none())
            .ok_or(AuthorityError::IoPortCapacity)?;
        *slot = Some(range);
        Ok(())
    }

    pub fn grant_mmio(&mut self, range: MmioRange) -> Result<(), AuthorityError> {
        if self
            .mmio
            .iter()
            .flatten()
            .any(|entry| entry.overlaps(range))
        {
            return Err(AuthorityError::OverlappingMmioRange);
        }
        let slot = self
            .mmio
            .iter_mut()
            .find(|entry| entry.is_none())
            .ok_or(AuthorityError::MmioCapacity)?;
        *slot = Some(range);
        Ok(())
    }

    pub fn grant_dma(&mut self, grant: DmaGrant) -> Result<(), AuthorityError> {
        if !grant.validate() {
            return Err(AuthorityError::InvalidDmaGrant);
        }
        if self.dma.is_some() {
            return Err(AuthorityError::DmaAlreadyGranted);
        }
        self.dma = Some(grant);
        Ok(())
    }

    pub fn permits_io(&self, port: u16, byte_width: u8) -> bool {
        self.io_ports
            .iter()
            .flatten()
            .any(|range| range.contains(port, byte_width))
    }

    pub fn permits_mmio(&self, address: u64, length: u64, access: MmioAccess) -> bool {
        self.mmio
            .iter()
            .flatten()
            .any(|range| range.contains(address, length, access))
    }

    pub const fn permits_dma(&self, iommu_domain: u64, generation: u32, requester_id: u16) -> bool {
        match self.dma {
            Some(grant) => {
                grant.iommu_domain == iommu_domain
                    && grant.generation == generation
                    && grant.requester_id == requester_id
            }
            None => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.io_ports.iter().all(Option::is_none)
            && self.mmio.iter().all(Option::is_none)
            && self.dma.is_none()
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        self.io_ports.iter().flatten().any(|left| {
            other
                .io_ports
                .iter()
                .flatten()
                .any(|right| left.overlaps(*right))
        }) || self.mmio.iter().flatten().any(|left| {
            other
                .mmio
                .iter()
                .flatten()
                .any(|right| left.overlaps(*right))
        }) || matches!(
            (self.dma, other.dma),
            (Some(left), Some(right))
                if left.iommu_domain == right.iommu_domain
                    || left.requester_id == right.requester_id
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DomainDescriptor {
    pub role: DomainRole,
    pub address_space_root: u64,
    pub authority: HardwareAuthority,
}

impl DomainDescriptor {
    fn validate(self) -> Result<(), AuthorityError> {
        if !valid_page_table_root(self.address_space_root) {
            return Err(AuthorityError::InvalidAddressSpaceRoot);
        }
        match self.role {
            DomainRole::NativeHardwareCell if self.authority.is_empty() => {
                Err(AuthorityError::HardwareCellHasNoAuthority)
            }
            DomainRole::NativeHardwareCell => Ok(()),
            DomainRole::PersonalityTranslator | DomainRole::UserProcess
                if !self.authority.is_empty() =>
            {
                Err(AuthorityError::HardwareAuthorityForbidden)
            }
            DomainRole::PersonalityTranslator | DomainRole::UserProcess => Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
struct DomainSlot {
    occupied: bool,
    generation: u32,
    execution_references: u32,
    descriptor: DomainDescriptor,
}

impl DomainSlot {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 0,
        execution_references: 0,
        descriptor: DomainDescriptor {
            role: DomainRole::UserProcess,
            address_space_root: 0,
            authority: HardwareAuthority::NONE,
        },
    };
}

/// Global fixed-capacity domain registry.
///
/// Every live non-kernel domain owns a unique root, including Ring 1 and Ring
/// 2. That uniqueness is the actual supervisor-compartment memory boundary;
/// CPL numbering alone is never accepted as isolation evidence.
pub struct DomainRegistry<const CAPACITY: usize> {
    kernel_address_space_root: u64,
    slots: [DomainSlot; CAPACITY],
    live: usize,
}

impl<const CAPACITY: usize> DomainRegistry<CAPACITY> {
    pub fn new(kernel_address_space_root: u64) -> Result<Self, AuthorityError> {
        if CAPACITY == 0 || CAPACITY > u16::MAX as usize + 1 {
            return Err(AuthorityError::DomainCapacity);
        }
        if !valid_page_table_root(kernel_address_space_root) {
            return Err(AuthorityError::InvalidAddressSpaceRoot);
        }
        Ok(Self {
            kernel_address_space_root,
            slots: [DomainSlot::EMPTY; CAPACITY],
            live: 0,
        })
    }

    pub const fn kernel_address_space_root(&self) -> u64 {
        self.kernel_address_space_root
    }

    pub const fn live_domains(&self) -> usize {
        self.live
    }

    pub fn register(
        &mut self,
        descriptor: DomainDescriptor,
    ) -> Result<DomainHandle, AuthorityError> {
        descriptor.validate()?;
        if descriptor.address_space_root == self.kernel_address_space_root
            || self.slots.iter().any(|slot| {
                slot.occupied && slot.descriptor.address_space_root == descriptor.address_space_root
            })
        {
            return Err(AuthorityError::SharedAddressSpace);
        }
        if descriptor.role == DomainRole::NativeHardwareCell
            && self.slots.iter().any(|slot| {
                slot.occupied
                    && slot.descriptor.role == DomainRole::NativeHardwareCell
                    && slot
                        .descriptor
                        .authority
                        .conflicts_with(&descriptor.authority)
            })
        {
            return Err(AuthorityError::HardwareAuthorityConflict);
        }
        let index = self
            .slots
            .iter()
            .position(|slot| !slot.occupied && slot.generation != u32::MAX)
            .ok_or(AuthorityError::DomainCapacity)?;
        let generation = self.slots[index]
            .generation
            .checked_add(1)
            .ok_or(AuthorityError::GenerationExhausted)?;
        self.slots[index] = DomainSlot {
            occupied: true,
            generation,
            execution_references: 0,
            descriptor,
        };
        self.live += 1;
        Ok(DomainHandle {
            slot: index as u16,
            generation,
        })
    }

    pub fn descriptor(&self, handle: DomainHandle) -> Result<DomainDescriptor, AuthorityError> {
        let slot = self.slot(handle)?;
        Ok(slot.descriptor)
    }

    pub fn revoke(&mut self, handle: DomainHandle) -> Result<(), AuthorityError> {
        let index = usize::from(handle.slot);
        let slot = self
            .slots
            .get_mut(index)
            .ok_or(AuthorityError::StaleDomain)?;
        if !slot.occupied || slot.generation != handle.generation {
            return Err(AuthorityError::StaleDomain);
        }
        if slot.execution_references != 0 {
            return Err(AuthorityError::DomainBusy);
        }
        slot.occupied = false;
        slot.descriptor = DomainSlot::EMPTY.descriptor;
        self.live -= 1;
        Ok(())
    }

    fn slot(&self, handle: DomainHandle) -> Result<&DomainSlot, AuthorityError> {
        let slot = self
            .slots
            .get(usize::from(handle.slot))
            .ok_or(AuthorityError::StaleDomain)?;
        if !slot.occupied || slot.generation != handle.generation {
            Err(AuthorityError::StaleDomain)
        } else {
            Ok(slot)
        }
    }

    fn retain_execution(&mut self, handle: DomainHandle) -> Result<(), AuthorityError> {
        let slot = self
            .slots
            .get_mut(usize::from(handle.slot))
            .ok_or(AuthorityError::StaleDomain)?;
        if !slot.occupied || slot.generation != handle.generation {
            return Err(AuthorityError::StaleDomain);
        }
        if slot.execution_references != 0 {
            return Err(AuthorityError::DomainBusy);
        }
        slot.execution_references = slot
            .execution_references
            .checked_add(1)
            .ok_or(AuthorityError::GenerationExhausted)?;
        Ok(())
    }

    fn release_execution(&mut self, handle: DomainHandle) -> Result<(), AuthorityError> {
        let slot = self
            .slots
            .get_mut(usize::from(handle.slot))
            .ok_or(AuthorityError::StaleDomain)?;
        if !slot.occupied || slot.generation != handle.generation {
            return Err(AuthorityError::StaleDomain);
        }
        slot.execution_references = slot
            .execution_references
            .checked_sub(1)
            .ok_or(AuthorityError::CorruptExecutionReference)?;
        Ok(())
    }

    pub fn authorize_io(
        &self,
        frontier: &TransitionFrontier,
        invocation: &KernelInvocation,
        port: u16,
        byte_width: u8,
    ) -> Result<(), AuthorityError> {
        let descriptor = self.authorized_caller(frontier, invocation)?;
        if descriptor.role != DomainRole::NativeHardwareCell
            || !descriptor.authority.permits_io(port, byte_width)
        {
            return Err(AuthorityError::HardwareRequestDenied);
        }
        Ok(())
    }

    pub fn authorize_mmio(
        &self,
        frontier: &TransitionFrontier,
        invocation: &KernelInvocation,
        address: u64,
        length: u64,
        access: MmioAccess,
    ) -> Result<(), AuthorityError> {
        let descriptor = self.authorized_caller(frontier, invocation)?;
        if descriptor.role != DomainRole::NativeHardwareCell
            || !descriptor.authority.permits_mmio(address, length, access)
        {
            return Err(AuthorityError::HardwareRequestDenied);
        }
        Ok(())
    }

    pub fn authorize_dma(
        &self,
        frontier: &TransitionFrontier,
        invocation: &KernelInvocation,
        iommu_domain: u64,
        generation: u32,
        requester_id: u16,
    ) -> Result<(), AuthorityError> {
        let descriptor = self.authorized_caller(frontier, invocation)?;
        if descriptor.role != DomainRole::NativeHardwareCell
            || !descriptor
                .authority
                .permits_dma(iommu_domain, generation, requester_id)
        {
            return Err(AuthorityError::HardwareRequestDenied);
        }
        Ok(())
    }

    fn authorized_caller(
        &self,
        frontier: &TransitionFrontier,
        invocation: &KernelInvocation,
    ) -> Result<DomainDescriptor, AuthorityError> {
        frontier.validate_invocation(invocation)?;
        self.descriptor(invocation.caller)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveExecution {
    Kernel,
    Domain(DomainHandle),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingTransition {
    sequence: u64,
    target: DomainHandle,
    target_ring: PrivilegeRing,
    address_space_root: u64,
    gate: TransitionGate,
}

/// Single-use authority returned to the scheduler before CR3 is changed.
#[derive(Debug, Eq, PartialEq)]
pub struct TransitionLease {
    cpu_id: u32,
    cpu_generation: u64,
    pending: PendingTransition,
}

impl TransitionLease {
    pub const fn target(&self) -> DomainHandle {
        self.pending.target
    }

    pub const fn address_space_root(&self) -> u64 {
        self.pending.address_space_root
    }

    pub const fn target_ring(&self) -> PrivilegeRing {
        self.pending.target_ring
    }

    pub const fn gate(&self) -> TransitionGate {
        self.pending.gate
    }
}

/// Machine-state certificate consumed by the final non-returning transition
/// stub. The stub must load this exact root and immediately execute the
/// certified IRETQ/SYSRETQ path without returning to Rust in the target CR3.
#[derive(Debug, Eq, PartialEq)]
pub struct CommittedTransition {
    target: DomainHandle,
    target_ring: PrivilegeRing,
    address_space_root: u64,
    gate: TransitionGate,
    sequence: u64,
}

impl CommittedTransition {
    pub const fn target(&self) -> DomainHandle {
        self.target
    }

    pub const fn target_ring(&self) -> PrivilegeRing {
        self.target_ring
    }

    pub const fn address_space_root(&self) -> u64 {
        self.address_space_root
    }

    pub const fn gate(&self) -> TransitionGate {
        self.gate
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Generation-bound authority for one trapped request from a non-kernel ring.
#[derive(Debug, Eq, PartialEq)]
pub struct KernelInvocation {
    cpu_id: u32,
    cpu_generation: u64,
    caller: DomainHandle,
    entry_sequence: u64,
}

impl KernelInvocation {
    pub const fn caller(&self) -> DomainHandle {
        self.caller
    }
}

/// Per-CPU transition state. A separate instance is required for every online
/// hardware thread and must be coupled to that CPU's GDT/TSS/CR3 path.
pub struct TransitionFrontier {
    cpu_id: u32,
    cpu_generation: u64,
    kernel_address_space_root: u64,
    sequence: u64,
    active: ActiveExecution,
    pending: Option<PendingTransition>,
    invocation: Option<(DomainHandle, u64)>,
}

impl TransitionFrontier {
    pub fn new(
        cpu_id: u32,
        cpu_generation: u64,
        kernel_address_space_root: u64,
    ) -> Result<Self, AuthorityError> {
        if cpu_generation == 0 {
            return Err(AuthorityError::InvalidCpuGeneration);
        }
        if !valid_page_table_root(kernel_address_space_root) {
            return Err(AuthorityError::InvalidAddressSpaceRoot);
        }
        Ok(Self {
            cpu_id,
            cpu_generation,
            kernel_address_space_root,
            sequence: 0,
            active: ActiveExecution::Kernel,
            pending: None,
            invocation: None,
        })
    }

    pub fn prepare<const CAPACITY: usize>(
        &mut self,
        registry: &mut DomainRegistry<CAPACITY>,
        target: DomainHandle,
        gate: TransitionGate,
    ) -> Result<TransitionLease, AuthorityError> {
        if registry.kernel_address_space_root != self.kernel_address_space_root {
            return Err(AuthorityError::KernelAddressSpaceMismatch);
        }
        if self.active != ActiveExecution::Kernel {
            return Err(AuthorityError::KernelNotActive);
        }
        if self.pending.is_some() {
            return Err(AuthorityError::TransitionAlreadyPending);
        }
        let descriptor = registry.descriptor(target)?;
        let target_ring = descriptor.role.ring();
        if !valid_dispatch_gate(PrivilegeRing::Kernel, target_ring, gate) {
            return Err(AuthorityError::InvalidTransitionGate);
        }
        if descriptor.address_space_root == self.kernel_address_space_root {
            return Err(AuthorityError::SharedAddressSpace);
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(AuthorityError::GenerationExhausted)?;
        registry.retain_execution(target)?;
        let pending = PendingTransition {
            sequence,
            target,
            target_ring,
            address_space_root: descriptor.address_space_root,
            gate,
        };
        self.sequence = sequence;
        self.pending = Some(pending);
        self.invocation = None;
        Ok(TransitionLease {
            cpu_id: self.cpu_id,
            cpu_generation: self.cpu_generation,
            pending,
        })
    }

    /// Commits immediately before the non-returning architecture trampoline.
    ///
    /// Rust validation runs under the observed kernel CR3 so Ring 1/2 address
    /// spaces never need to map the registry, frontier, or kernel stack. The
    /// returned certificate names the exact target CR3 and sole admitted final
    /// return gate; the trampoline must load them without another Rust call.
    pub fn commit<const CAPACITY: usize>(
        &mut self,
        registry: &DomainRegistry<CAPACITY>,
        lease: &TransitionLease,
        observed_kernel_address_space_root: u64,
    ) -> Result<CommittedTransition, AuthorityError> {
        if lease.cpu_id != self.cpu_id || lease.cpu_generation != self.cpu_generation {
            return Err(AuthorityError::WrongCpu);
        }
        if self.pending != Some(lease.pending) {
            return Err(AuthorityError::StaleTransition);
        }
        let descriptor = registry.descriptor(lease.pending.target)?;
        if descriptor.address_space_root != lease.pending.address_space_root
            || descriptor.role.ring() != lease.pending.target_ring
        {
            return Err(AuthorityError::StaleDomain);
        }
        if observed_kernel_address_space_root != self.kernel_address_space_root
            || registry.kernel_address_space_root != self.kernel_address_space_root
        {
            return Err(AuthorityError::KernelAddressSpaceMismatch);
        }
        if !valid_dispatch_gate(
            PrivilegeRing::Kernel,
            lease.pending.target_ring,
            lease.pending.gate,
        ) {
            return Err(AuthorityError::InvalidTransitionGate);
        }
        self.pending = None;
        self.active = ActiveExecution::Domain(lease.pending.target);
        Ok(CommittedTransition {
            target: lease.pending.target,
            target_ring: lease.pending.target_ring,
            address_space_root: lease.pending.address_space_root,
            gate: lease.pending.gate,
            sequence: lease.pending.sequence,
        })
    }

    pub fn abort<const CAPACITY: usize>(
        &mut self,
        registry: &mut DomainRegistry<CAPACITY>,
        lease: TransitionLease,
    ) -> Result<(), AuthorityError> {
        if lease.cpu_id != self.cpu_id || lease.cpu_generation != self.cpu_generation {
            return Err(AuthorityError::WrongCpu);
        }
        if self.pending != Some(lease.pending) {
            return Err(AuthorityError::StaleTransition);
        }
        registry.release_execution(lease.pending.target)?;
        self.pending = None;
        Ok(())
    }

    /// Records an interrupt, trap, or syscall only after the entry stub has
    /// restored the kernel CR3. The resulting invocation is what IO/MMIO/DMA
    /// brokers must present while servicing the trapped caller.
    pub fn enter_kernel<const CAPACITY: usize>(
        &mut self,
        registry: &mut DomainRegistry<CAPACITY>,
        caller: DomainHandle,
        source_address_space_root: u64,
        installed_kernel_address_space_root: u64,
    ) -> Result<KernelInvocation, AuthorityError> {
        if self.pending.is_some() {
            return Err(AuthorityError::TransitionAlreadyPending);
        }
        if self.active != ActiveExecution::Domain(caller) {
            return Err(AuthorityError::UnexpectedCaller);
        }
        let descriptor = registry.descriptor(caller)?;
        if descriptor.address_space_root != source_address_space_root {
            return Err(AuthorityError::SourceAddressSpaceMismatch);
        }
        if installed_kernel_address_space_root != self.kernel_address_space_root
            || registry.kernel_address_space_root != self.kernel_address_space_root
        {
            return Err(AuthorityError::KernelAddressSpaceMismatch);
        }
        let entry_sequence = self
            .sequence
            .checked_add(1)
            .ok_or(AuthorityError::GenerationExhausted)?;
        registry.release_execution(caller)?;
        self.sequence = entry_sequence;
        self.active = ActiveExecution::Kernel;
        self.invocation = Some((caller, entry_sequence));
        Ok(KernelInvocation {
            cpu_id: self.cpu_id,
            cpu_generation: self.cpu_generation,
            caller,
            entry_sequence,
        })
    }

    fn validate_invocation(&self, invocation: &KernelInvocation) -> Result<(), AuthorityError> {
        if invocation.cpu_id != self.cpu_id || invocation.cpu_generation != self.cpu_generation {
            return Err(AuthorityError::WrongCpu);
        }
        if self.active != ActiveExecution::Kernel
            || self.pending.is_some()
            || self.invocation != Some((invocation.caller, invocation.entry_sequence))
        {
            return Err(AuthorityError::StaleInvocation);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityError {
    InvalidAddressSpaceRoot,
    SharedAddressSpace,
    InvalidCpuGeneration,
    InvalidIoPortRange,
    InvalidMmioRange,
    InvalidDmaGrant,
    OverlappingIoPortRange,
    OverlappingMmioRange,
    IoPortCapacity,
    MmioCapacity,
    DmaAlreadyGranted,
    HardwareCellHasNoAuthority,
    HardwareAuthorityForbidden,
    HardwareRequestDenied,
    HardwareAuthorityConflict,
    DomainCapacity,
    DomainBusy,
    GenerationExhausted,
    CorruptExecutionReference,
    StaleDomain,
    KernelAddressSpaceMismatch,
    KernelNotActive,
    TransitionAlreadyPending,
    InvalidTransitionGate,
    WrongCpu,
    StaleTransition,
    UnexpectedCaller,
    SourceAddressSpaceMismatch,
    StaleInvocation,
}

#[cfg(test)]
mod tests {
    use super::*;

    const KERNEL_ROOT: u64 = 0x1000;
    const CELL_ROOT: u64 = 0x2000;
    const PERSONALITY_ROOT: u64 = 0x3000;
    const USER_ROOT: u64 = 0x4000;

    fn hardware_authority() -> HardwareAuthority {
        let mut authority = HardwareAuthority::NONE;
        authority
            .grant_io(IoPortRange::new(0x3f8, 0x3ff).unwrap())
            .unwrap();
        authority
            .grant_mmio(MmioRange::new(0x8000_0000, 0x2000, MmioAccess::READ_WRITE).unwrap())
            .unwrap();
        authority
            .grant_dma(DmaGrant {
                iommu_domain: 9,
                generation: 4,
                requester_id: 0x108,
            })
            .unwrap();
        authority
    }

    fn register_three() -> (DomainRegistry<4>, DomainHandle, DomainHandle, DomainHandle) {
        let mut registry = DomainRegistry::new(KERNEL_ROOT).unwrap();
        let cell = registry
            .register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: CELL_ROOT,
                authority: hardware_authority(),
            })
            .unwrap();
        let personality = registry
            .register(DomainDescriptor {
                role: DomainRole::PersonalityTranslator,
                address_space_root: PERSONALITY_ROOT,
                authority: HardwareAuthority::NONE,
            })
            .unwrap();
        let user = registry
            .register(DomainDescriptor {
                role: DomainRole::UserProcess,
                address_space_root: USER_ROOT,
                authority: HardwareAuthority::NONE,
            })
            .unwrap();
        (registry, cell, personality, user)
    }

    #[test]
    fn dispatch_gate_matrix_is_exhaustive_and_architecturally_valid() {
        for source in PrivilegeRing::ALL {
            for target in PrivilegeRing::ALL {
                for gate in [TransitionGate::Iretq, TransitionGate::Sysretq] {
                    let expected = source == PrivilegeRing::Kernel
                        && target != PrivilegeRing::Kernel
                        && (gate == TransitionGate::Iretq || target == PrivilegeRing::User);
                    assert_eq!(valid_dispatch_gate(source, target, gate), expected);
                }
            }
        }
    }

    #[test]
    fn supervisor_domains_must_have_distinct_address_spaces() {
        let mut registry = DomainRegistry::<4>::new(KERNEL_ROOT).unwrap();
        let cell = registry
            .register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: CELL_ROOT,
                authority: hardware_authority(),
            })
            .unwrap();
        assert_eq!(
            registry.descriptor(cell).unwrap().role.ring(),
            PrivilegeRing::HardwareCell
        );
        assert_eq!(
            registry.register(DomainDescriptor {
                role: DomainRole::PersonalityTranslator,
                address_space_root: CELL_ROOT,
                authority: HardwareAuthority::NONE,
            }),
            Err(AuthorityError::SharedAddressSpace)
        );
        assert_eq!(
            registry.register(DomainDescriptor {
                role: DomainRole::PersonalityTranslator,
                address_space_root: KERNEL_ROOT,
                authority: HardwareAuthority::NONE,
            }),
            Err(AuthorityError::SharedAddressSpace)
        );
    }

    #[test]
    fn only_hardware_cells_can_hold_hardware_authority() {
        let mut registry = DomainRegistry::<2>::new(KERNEL_ROOT).unwrap();
        for role in [DomainRole::PersonalityTranslator, DomainRole::UserProcess] {
            assert_eq!(
                registry.register(DomainDescriptor {
                    role,
                    address_space_root: if role == DomainRole::UserProcess {
                        USER_ROOT
                    } else {
                        PERSONALITY_ROOT
                    },
                    authority: hardware_authority(),
                }),
                Err(AuthorityError::HardwareAuthorityForbidden)
            );
        }
        assert_eq!(
            registry.register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: CELL_ROOT,
                authority: HardwareAuthority::NONE,
            }),
            Err(AuthorityError::HardwareCellHasNoAuthority)
        );
    }

    #[test]
    fn hardware_ranges_enforce_bounds_width_permissions_and_identity() {
        let authority = hardware_authority();
        assert!(authority.permits_io(0x3f8, 4));
        assert!(authority.permits_io(0x3ff, 1));
        assert!(!authority.permits_io(0x3ff, 2));
        assert!(!authority.permits_io(0x3f8, 0));
        assert!(authority.permits_mmio(0x8000_0ff0, 16, MmioAccess::READ));
        assert!(authority.permits_mmio(0x8000_1000, 0x1000, MmioAccess::WRITE));
        assert!(!authority.permits_mmio(0x8000_1ff0, 32, MmioAccess::READ));
        assert!(authority.permits_dma(9, 4, 0x108));
        assert!(!authority.permits_dma(9, 5, 0x108));
        assert!(!authority.permits_dma(9, 4, 0x109));
    }

    #[test]
    fn overlapping_and_malformed_grants_are_rejected() {
        assert_eq!(
            IoPortRange::new(8, 7),
            Err(AuthorityError::InvalidIoPortRange)
        );
        assert_eq!(
            MmioRange::new(0x8000_0001, 0x1000, MmioAccess::READ),
            Err(AuthorityError::InvalidMmioRange)
        );
        let mut authority = HardwareAuthority::NONE;
        authority
            .grant_io(IoPortRange::new(10, 20).unwrap())
            .unwrap();
        assert_eq!(
            authority.grant_io(IoPortRange::new(20, 30).unwrap()),
            Err(AuthorityError::OverlappingIoPortRange)
        );
        authority
            .grant_mmio(MmioRange::new(0x9000_0000, 0x2000, MmioAccess::READ).unwrap())
            .unwrap();
        assert_eq!(
            authority.grant_mmio(MmioRange::new(0x9000_1000, 0x1000, MmioAccess::WRITE).unwrap()),
            Err(AuthorityError::OverlappingMmioRange)
        );
    }

    #[test]
    fn hardware_cells_cannot_alias_ports_mmio_or_dma_authority() {
        let mut registry = DomainRegistry::<5>::new(KERNEL_ROOT).unwrap();
        registry
            .register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: CELL_ROOT,
                authority: hardware_authority(),
            })
            .unwrap();

        let mut port_conflict = HardwareAuthority::NONE;
        port_conflict
            .grant_io(IoPortRange::new(0x3fc, 0x403).unwrap())
            .unwrap();
        assert_eq!(
            registry.register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: 0x5000,
                authority: port_conflict,
            }),
            Err(AuthorityError::HardwareAuthorityConflict)
        );

        let mut mmio_conflict = HardwareAuthority::NONE;
        mmio_conflict
            .grant_mmio(MmioRange::new(0x8000_1000, 0x1000, MmioAccess::READ).unwrap())
            .unwrap();
        assert_eq!(
            registry.register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: 0x6000,
                authority: mmio_conflict,
            }),
            Err(AuthorityError::HardwareAuthorityConflict)
        );

        let mut dma_conflict = HardwareAuthority::NONE;
        dma_conflict
            .grant_dma(DmaGrant {
                iommu_domain: 9,
                generation: 99,
                requester_id: 0x200,
            })
            .unwrap();
        assert_eq!(
            registry.register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: 0x7000,
                authority: dma_conflict,
            }),
            Err(AuthorityError::HardwareAuthorityConflict)
        );
        assert_eq!(registry.live_domains(), 1);

        let mut disjoint = HardwareAuthority::NONE;
        disjoint
            .grant_io(IoPortRange::new(0x2f8, 0x2ff).unwrap())
            .unwrap();
        disjoint
            .grant_mmio(MmioRange::new(0x9000_0000, 0x1000, MmioAccess::READ_WRITE).unwrap())
            .unwrap();
        disjoint
            .grant_dma(DmaGrant {
                iommu_domain: 10,
                generation: 1,
                requester_id: 0x109,
            })
            .unwrap();
        registry
            .register(DomainDescriptor {
                role: DomainRole::NativeHardwareCell,
                address_space_root: 0x8000,
                authority: disjoint,
            })
            .unwrap();
        assert_eq!(registry.live_domains(), 2);
    }

    #[test]
    fn transition_is_two_phase_cpu_bound_and_requires_kernel_cr3() {
        let (mut registry, cell, _, _) = register_three();
        let mut frontier = TransitionFrontier::new(3, 7, KERNEL_ROOT).unwrap();
        let lease = frontier
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap();
        assert_eq!(lease.target_ring(), PrivilegeRing::HardwareCell);
        assert_eq!(lease.address_space_root(), CELL_ROOT);
        assert_eq!(
            frontier.commit(&registry, &lease, CELL_ROOT),
            Err(AuthorityError::KernelAddressSpaceMismatch)
        );
        let pending_error = frontier
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap_err();
        assert_eq!(pending_error, AuthorityError::TransitionAlreadyPending);
        frontier.abort(&mut registry, lease).unwrap();
    }

    #[test]
    fn committed_transition_and_entry_mint_scoped_invocation() {
        let (mut registry, cell, _, _) = register_three();
        let mut frontier = TransitionFrontier::new(3, 7, KERNEL_ROOT).unwrap();
        let lease = frontier
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap();
        let committed = frontier.commit(&registry, &lease, KERNEL_ROOT).unwrap();
        assert_eq!(committed.target(), cell);
        assert_eq!(committed.gate(), TransitionGate::Iretq);

        let invocation = frontier
            .enter_kernel(&mut registry, cell, CELL_ROOT, KERNEL_ROOT)
            .unwrap();
        assert_eq!(
            registry.authorize_io(&frontier, &invocation, 0x3f8, 1),
            Ok(())
        );
        assert_eq!(
            registry.authorize_mmio(
                &frontier,
                &invocation,
                0x8000_1000,
                0x1000,
                MmioAccess::WRITE,
            ),
            Ok(())
        );
        assert_eq!(
            registry.authorize_dma(&frontier, &invocation, 9, 4, 0x108),
            Ok(())
        );
        assert_eq!(
            registry.authorize_io(&frontier, &invocation, 0x60, 1),
            Err(AuthorityError::HardwareRequestDenied)
        );
    }

    #[test]
    fn personalities_and_users_cannot_use_sysret_or_hardware_incorrectly() {
        let (mut registry, _, personality, user) = register_three();
        let mut frontier = TransitionFrontier::new(0, 1, KERNEL_ROOT).unwrap();
        assert_eq!(
            frontier.prepare(&mut registry, personality, TransitionGate::Sysretq),
            Err(AuthorityError::InvalidTransitionGate)
        );
        let lease = frontier
            .prepare(&mut registry, personality, TransitionGate::Iretq)
            .unwrap();
        frontier.commit(&registry, &lease, KERNEL_ROOT).unwrap();
        let invocation = frontier
            .enter_kernel(&mut registry, personality, PERSONALITY_ROOT, KERNEL_ROOT)
            .unwrap();
        assert_eq!(
            registry.authorize_io(&frontier, &invocation, 0x3f8, 1),
            Err(AuthorityError::HardwareRequestDenied)
        );
        let lease = frontier
            .prepare(&mut registry, user, TransitionGate::Sysretq)
            .unwrap();
        assert_eq!(lease.target_ring(), PrivilegeRing::User);
        frontier.abort(&mut registry, lease).unwrap();
    }

    #[test]
    fn invocation_expires_at_next_dispatch() {
        let (mut registry, cell, _, user) = register_three();
        let mut frontier = TransitionFrontier::new(0, 1, KERNEL_ROOT).unwrap();
        let lease = frontier
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap();
        frontier.commit(&registry, &lease, KERNEL_ROOT).unwrap();
        let invocation = frontier
            .enter_kernel(&mut registry, cell, CELL_ROOT, KERNEL_ROOT)
            .unwrap();
        let next = frontier
            .prepare(&mut registry, user, TransitionGate::Sysretq)
            .unwrap();
        assert_eq!(
            registry.authorize_io(&frontier, &invocation, 0x3f8, 1),
            Err(AuthorityError::StaleInvocation)
        );
        frontier.abort(&mut registry, next).unwrap();
    }

    #[test]
    fn recycled_slots_invalidate_domain_generations() {
        let mut registry = DomainRegistry::<1>::new(KERNEL_ROOT).unwrap();
        let first = registry
            .register(DomainDescriptor {
                role: DomainRole::UserProcess,
                address_space_root: USER_ROOT,
                authority: HardwareAuthority::NONE,
            })
            .unwrap();
        registry.revoke(first).unwrap();
        let second = registry
            .register(DomainDescriptor {
                role: DomainRole::PersonalityTranslator,
                address_space_root: PERSONALITY_ROOT,
                authority: HardwareAuthority::NONE,
            })
            .unwrap();
        assert_eq!(first.slot(), second.slot());
        assert!(second.generation() > first.generation());
        assert_eq!(registry.descriptor(first), Err(AuthorityError::StaleDomain));
    }

    #[test]
    fn pending_and_active_domains_cannot_be_revoked() {
        let (mut registry, cell, _, _) = register_three();
        let mut frontier = TransitionFrontier::new(0, 1, KERNEL_ROOT).unwrap();
        let lease = frontier
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap();
        assert_eq!(registry.revoke(cell), Err(AuthorityError::DomainBusy));
        frontier.commit(&registry, &lease, KERNEL_ROOT).unwrap();
        assert_eq!(registry.revoke(cell), Err(AuthorityError::DomainBusy));
        let _invocation = frontier
            .enter_kernel(&mut registry, cell, CELL_ROOT, KERNEL_ROOT)
            .unwrap();
        assert_eq!(registry.revoke(cell), Ok(()));
    }

    #[test]
    fn one_domain_cannot_execute_on_two_cpus_concurrently() {
        let (mut registry, cell, _, _) = register_three();
        let mut first = TransitionFrontier::new(0, 1, KERNEL_ROOT).unwrap();
        let mut second = TransitionFrontier::new(1, 1, KERNEL_ROOT).unwrap();
        let lease = first
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap();
        assert_eq!(
            second.prepare(&mut registry, cell, TransitionGate::Iretq),
            Err(AuthorityError::DomainBusy)
        );
        first.abort(&mut registry, lease).unwrap();
        let second_lease = second
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap();
        second.abort(&mut registry, second_lease).unwrap();
    }

    #[test]
    fn wrong_cpu_and_unexpected_entry_are_rejected() {
        let (mut registry, cell, _, _) = register_three();
        let mut first = TransitionFrontier::new(0, 1, KERNEL_ROOT).unwrap();
        let mut second = TransitionFrontier::new(1, 1, KERNEL_ROOT).unwrap();
        let lease = first
            .prepare(&mut registry, cell, TransitionGate::Iretq)
            .unwrap();
        assert_eq!(
            second.commit(&registry, &lease, KERNEL_ROOT),
            Err(AuthorityError::WrongCpu)
        );
        assert_eq!(
            second.enter_kernel(&mut registry, cell, CELL_ROOT, KERNEL_ROOT),
            Err(AuthorityError::UnexpectedCaller)
        );
        first.abort(&mut registry, lease).unwrap();
    }
}
