//! Single-context Intel VT-d DMA-remapping backend.
//!
//! This is an owning, fixed-capacity integration core.  It deliberately does
//! not interpret a shared or include-all DRHD as permission to translate every
//! requester. A scope is admissible only when firmware routing selects the
//! target requester and construction starts with empty root/context tables,
//! then publishes exactly that requester's context.
//! Production storage is supplied by `vtd_memory`: exclusive root/context and
//! SLPT pages pinned in the same physical-frame ledger used by process address
//! spaces. This module still makes no system-wide isolation claim until the
//! platform owner supplies those pages and the live register transport.

use sisyphus_driver_abi::{
    Handle, STATUS_BUSY, STATUS_INVALID_ARGUMENT, STATUS_IO_ERROR, STATUS_NOT_FOUND, STATUS_OK,
    STATUS_UNSUPPORTED, Status,
};

use crate::boot::acpi::{DmarEndpoint, DmarInfo, DmarRemappingUnit, DmarRouteError};
use crate::sync::SpinLock;

use super::iommu::{DmaAccess, DmaRemappingBackend};
use super::pci::PciAddress;
use super::vtd::{
    ContextEntryTable, RootEntryTable, TableError, VtdEngineFault, VtdEngineState,
    VtdMmioRegisters, VtdRegisterBackend, VtdRemappingEngine,
};
use super::vtd_slpt::{
    DmaPermissions, MapOutcome, MapReceipt, MappingHandle, Slpt, SlptConfig, SlptFault, SlptFrame,
    SlptInvalidationError, SlptInvalidator, SlptPageMemory, UnmapOutcome, UnmapReceipt,
};

const PAGE_SIZE: u64 = 4096;
const INVALIDATION_FAULT: u64 = 1;

// SAFETY: `VtdMmioRegisters` exclusively owns a handle for a mapping in the
// kernel's fixed MMIO window.  Moving that handle and stable virtual address
// does not change either mapping, and teardown is handle-based rather than
// thread-affine.  This grants movement, not shared access; this backend puts
// the transport behind `SpinLock` before implementing its `Sync` interface.
unsafe impl Send for VtdMmioRegisters {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SingleRequesterScopeError {
    IncludeAll,
    IncompleteScope,
    UnitNotPresent,
    NotExactlyOneEndpoint,
    EndpointMismatch,
    AmbiguousRouting,
}

/// Firmware evidence that one DRHD explicitly owns exactly one PCI requester.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SingleRequesterScope {
    unit: DmarRemappingUnit,
    requester: PciAddress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolatedIncludeAllScopeError {
    NotIncludeAll,
    NonZeroSegment,
    IncompleteScope,
    UnitNotPresent,
    ExplicitEndpointsPresent,
    RouteMismatch,
}

/// An include-all DRHD narrowed by an empty-table, single-context policy.
///
/// Firmware did not grant exclusive ownership here.  Instead, the eventual
/// backend construction proves that both root/context tables are empty and
/// installs exactly one requester context before translation is enabled.
/// Segment zero is required until PCI identity carries a segment field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IsolatedIncludeAllRequesterScope {
    unit: DmarRemappingUnit,
    requester: PciAddress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolatedSharedUnitScopeError {
    IncludeAll,
    NonZeroSegment,
    IncompleteScope,
    UnitNotPresent,
    MissingEndpoints,
    EndpointMismatch,
    AmbiguousRouting,
}

/// A multi-requester DRHD narrowed by an empty-table, single-context policy.
///
/// The unit is shared by firmware routing, but that does not grant its other
/// requesters translation access: backend construction starts from empty root
/// and context tables and installs only this requester's context entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IsolatedSharedUnitRequesterScope {
    unit: DmarRemappingUnit,
    requester: PciAddress,
}

impl IsolatedSharedUnitRequesterScope {
    pub fn from_dmar(
        dmar: &DmarInfo,
        unit: DmarRemappingUnit,
        requester: PciAddress,
    ) -> Result<Self, IsolatedSharedUnitScopeError> {
        if unit.include_all {
            return Err(IsolatedSharedUnitScopeError::IncludeAll);
        }
        if unit.segment != 0 {
            return Err(IsolatedSharedUnitScopeError::NonZeroSegment);
        }
        if unit.has_unresolved_scopes() {
            return Err(IsolatedSharedUnitScopeError::IncompleteScope);
        }
        let endpoints = dmar
            .explicit_endpoints_for(unit)
            .ok_or(IsolatedSharedUnitScopeError::UnitNotPresent)?;
        if endpoints.is_empty() {
            return Err(IsolatedSharedUnitScopeError::MissingEndpoints);
        }
        let expected = DmarEndpoint {
            segment: 0,
            bus: requester.bus,
            slot: requester.slot,
            function: requester.function,
        };
        if !endpoints.contains(&expected) {
            return Err(IsolatedSharedUnitScopeError::EndpointMismatch);
        }
        if dmar.remapping_unit_for(expected).ok() != Some(Some(unit)) {
            return Err(IsolatedSharedUnitScopeError::AmbiguousRouting);
        }
        Ok(Self { unit, requester })
    }

    pub const fn requester(self) -> PciAddress {
        self.requester
    }

    pub const fn unit(self) -> DmarRemappingUnit {
        self.unit
    }
}

impl IsolatedIncludeAllRequesterScope {
    pub fn from_dmar(
        dmar: &DmarInfo,
        unit: DmarRemappingUnit,
        requester: PciAddress,
    ) -> Result<Self, IsolatedIncludeAllScopeError> {
        if !unit.include_all {
            return Err(IsolatedIncludeAllScopeError::NotIncludeAll);
        }
        if unit.segment != 0 {
            return Err(IsolatedIncludeAllScopeError::NonZeroSegment);
        }
        if unit.has_unresolved_scopes() {
            return Err(IsolatedIncludeAllScopeError::IncompleteScope);
        }
        let endpoints = dmar
            .explicit_endpoints_for(unit)
            .ok_or(IsolatedIncludeAllScopeError::UnitNotPresent)?;
        if !endpoints.is_empty() {
            return Err(IsolatedIncludeAllScopeError::ExplicitEndpointsPresent);
        }
        let endpoint = DmarEndpoint {
            segment: 0,
            bus: requester.bus,
            slot: requester.slot,
            function: requester.function,
        };
        if dmar.remapping_unit_for(endpoint).ok() != Some(Some(unit)) {
            return Err(IsolatedIncludeAllScopeError::RouteMismatch);
        }
        Ok(Self { unit, requester })
    }

    pub const fn requester(self) -> PciAddress {
        self.requester
    }

    pub const fn unit(self) -> DmarRemappingUnit {
        self.unit
    }
}

/// One requester selected either by exact firmware scope or by the explicit
/// empty-table include-all policy above.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdRequesterScope {
    Explicit(SingleRequesterScope),
    IsolatedIncludeAll(IsolatedIncludeAllRequesterScope),
    IsolatedSharedUnit(IsolatedSharedUnitRequesterScope),
}

impl VtdRequesterScope {
    pub const fn requester(self) -> PciAddress {
        match self {
            Self::Explicit(scope) => scope.requester(),
            Self::IsolatedIncludeAll(scope) => scope.requester(),
            Self::IsolatedSharedUnit(scope) => scope.requester(),
        }
    }

    pub const fn unit(self) -> DmarRemappingUnit {
        match self {
            Self::Explicit(scope) => scope.unit(),
            Self::IsolatedIncludeAll(scope) => scope.unit(),
            Self::IsolatedSharedUnit(scope) => scope.unit(),
        }
    }

    pub const fn policy_name(self) -> &'static str {
        match self {
            Self::Explicit(_) => "firmware-single",
            Self::IsolatedIncludeAll(_) => "isolated-include-all",
            Self::IsolatedSharedUnit(_) => "isolated-shared-unit",
        }
    }
}

impl From<SingleRequesterScope> for VtdRequesterScope {
    fn from(scope: SingleRequesterScope) -> Self {
        Self::Explicit(scope)
    }
}

impl From<IsolatedIncludeAllRequesterScope> for VtdRequesterScope {
    fn from(scope: IsolatedIncludeAllRequesterScope) -> Self {
        Self::IsolatedIncludeAll(scope)
    }
}

impl From<IsolatedSharedUnitRequesterScope> for VtdRequesterScope {
    fn from(scope: IsolatedSharedUnitRequesterScope) -> Self {
        Self::IsolatedSharedUnit(scope)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdRequesterScopeSelectionError {
    Routing(DmarRouteError),
    NoRemappingUnit,
    Explicit(SingleRequesterScopeError),
    IncludeAll(IsolatedIncludeAllScopeError),
    SharedUnit(IsolatedSharedUnitScopeError),
}

/// Selects the only admissible requester policy for a segment-zero PCI BDF.
/// The caller still needs pinned empty tables and live register proof before
/// hardware translation can be enabled.
pub fn select_requester_scope(
    dmar: &DmarInfo,
    requester: PciAddress,
) -> Result<VtdRequesterScope, VtdRequesterScopeSelectionError> {
    let endpoint = DmarEndpoint {
        segment: 0,
        bus: requester.bus,
        slot: requester.slot,
        function: requester.function,
    };
    let unit = dmar
        .remapping_unit_for(endpoint)
        .map_err(VtdRequesterScopeSelectionError::Routing)?
        .ok_or(VtdRequesterScopeSelectionError::NoRemappingUnit)?;
    if unit.include_all {
        IsolatedIncludeAllRequesterScope::from_dmar(dmar, unit, requester)
            .map(VtdRequesterScope::from)
            .map_err(VtdRequesterScopeSelectionError::IncludeAll)
    } else {
        let endpoint_count = dmar
            .explicit_endpoints_for(unit)
            .map_or(0, |endpoints| endpoints.len());
        if endpoint_count == 1 {
            SingleRequesterScope::from_dmar(dmar, unit, requester)
                .map(VtdRequesterScope::from)
                .map_err(VtdRequesterScopeSelectionError::Explicit)
        } else {
            IsolatedSharedUnitRequesterScope::from_dmar(dmar, unit, requester)
                .map(VtdRequesterScope::from)
                .map_err(VtdRequesterScopeSelectionError::SharedUnit)
        }
    }
}

impl SingleRequesterScope {
    pub fn from_dmar(
        dmar: &DmarInfo,
        unit: DmarRemappingUnit,
        requester: PciAddress,
    ) -> Result<Self, SingleRequesterScopeError> {
        if unit.include_all {
            return Err(SingleRequesterScopeError::IncludeAll);
        }
        if unit.has_unresolved_scopes() {
            return Err(SingleRequesterScopeError::IncompleteScope);
        }
        let endpoints = dmar
            .explicit_endpoints_for(unit)
            .ok_or(SingleRequesterScopeError::UnitNotPresent)?;
        if endpoints.len() != 1 {
            return Err(SingleRequesterScopeError::NotExactlyOneEndpoint);
        }
        let expected = DmarEndpoint {
            segment: unit.segment,
            bus: requester.bus,
            slot: requester.slot,
            function: requester.function,
        };
        if endpoints[0] != expected {
            return Err(SingleRequesterScopeError::EndpointMismatch);
        }
        if dmar.remapping_unit_for(expected).ok() != Some(Some(unit)) {
            return Err(SingleRequesterScopeError::AmbiguousRouting);
        }
        Ok(Self { unit, requester })
    }

    pub const fn requester(self) -> PciAddress {
        self.requester
    }

    pub const fn unit(self) -> DmarRemappingUnit {
        self.unit
    }
}

/// Stable storage for one legacy VT-d root table and one context table.
///
/// # Safety
///
/// The physical addresses must remain stable for the value's whole lifetime,
/// must identify the exact tables returned by the reference methods, and must
/// be visible to the remapping unit with the ordering supplied by the entry
/// installation methods.  Both pages must be exclusively owned by this core.
pub unsafe trait VtdRootContextStorage: Send {
    fn root_table(&self) -> &RootEntryTable;
    fn root_table_physical_address(&self) -> u64;
    fn context_table(&self) -> &ContextEntryTable;
    fn context_table_physical_address(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdBackendBuildFault {
    InvalidCapacity,
    InvalidTableAlias,
    RootTableNotEmpty,
    ContextTableNotEmpty,
    Probe(VtdEngineFault),
    Slpt(SlptFault),
    Table(TableError),
    Enable(VtdEngineFault),
}

/// Register ownership after an unsuccessful construction attempt.
pub enum VtdEngineOwnership<Registers: VtdRegisterBackend> {
    Registers(Registers),
    Engine(VtdRemappingEngine<Registers>),
}

/// All caller-owned resources returned after construction fails.
pub struct VtdBackendBuildOwnership<
    Registers: VtdRegisterBackend,
    Memory,
    Tables,
    const PAGES: usize,
> {
    pub engine: VtdEngineOwnership<Registers>,
    pub memory: Memory,
    pub tables: Tables,
    pub slpt: Option<Slpt<PAGES>>,
    pub slpt_root: SlptFrame,
    pub scope: VtdRequesterScope,
    pub tables_installed: bool,
}

pub struct VtdBackendBuildFailure<Registers: VtdRegisterBackend, Memory, Tables, const PAGES: usize>
{
    fault: VtdBackendBuildFault,
    ownership: VtdBackendBuildOwnership<Registers, Memory, Tables, PAGES>,
}

impl<Registers: VtdRegisterBackend, Memory, Tables, const PAGES: usize>
    VtdBackendBuildFailure<Registers, Memory, Tables, PAGES>
{
    pub const fn fault(&self) -> VtdBackendBuildFault {
        self.fault
    }

    pub fn into_ownership(self) -> VtdBackendBuildOwnership<Registers, Memory, Tables, PAGES> {
        self.ownership
    }
}

impl<Registers: VtdRegisterBackend, Memory, Tables, const PAGES: usize> core::fmt::Debug
    for VtdBackendBuildFailure<Registers, Memory, Tables, PAGES>
{
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("VtdBackendBuildFailure")
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VtdBatchHandle {
    slot: u16,
    generation: u32,
}

impl VtdBatchHandle {
    pub const fn slot(self) -> u16 {
        self.slot
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageRecord {
    Empty,
    Active(MappingHandle),
    MapPending(MapReceipt),
    UnmapPending(UnmapReceipt),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BatchPhase {
    Empty,
    Staging,
    Active,
    Quarantined,
}

#[derive(Clone, Copy)]
struct BatchSlot {
    generation: u32,
    retired: bool,
    domain_generation: u32,
    phase: BatchPhase,
    device_address: u64,
    physical_address: u64,
    length: usize,
    page_start: usize,
    page_count: usize,
    was_active: bool,
}

impl BatchSlot {
    const EMPTY: Self = Self {
        generation: 1,
        retired: false,
        domain_generation: 0,
        phase: BatchPhase::Empty,
        device_address: 0,
        physical_address: 0,
        length: 0,
        page_start: 0,
        page_count: 0,
        was_active: false,
    };
}

struct EngineInvalidator<'a, Registers: VtdRegisterBackend> {
    engine: &'a mut VtdRemappingEngine<Registers>,
    poll_limit: u32,
    last_fault: &'a mut Option<VtdEngineFault>,
}

impl<Registers: VtdRegisterBackend> SlptInvalidator for EngineInvalidator<'_, Registers> {
    fn invalidate_after_page_table_change(
        &mut self,
        _iova: u64,
    ) -> Result<(), SlptInvalidationError> {
        self.engine
            .commit_table_update(self.poll_limit)
            .map_err(|fault| {
                if self.last_fault.is_none() {
                    *self.last_fault = Some(fault);
                }
                SlptInvalidationError(INVALIDATION_FAULT)
            })
    }
}

struct VtdBackendCore<
    Registers: VtdRegisterBackend,
    Memory,
    Tables,
    const PAGES: usize,
    const BATCHES: usize,
    const MAX_BATCH_PAGES: usize,
> {
    engine: VtdRemappingEngine<Registers>,
    memory: Memory,
    tables: Tables,
    slpt: Slpt<PAGES>,
    scope: VtdRequesterScope,
    poll_limit: u32,
    domain_generation: u32,
    domain_active: bool,
    domain_retired: bool,
    authority_released: bool,
    poisoned: bool,
    last_engine_fault: Option<VtdEngineFault>,
    batches: [BatchSlot; BATCHES],
    page_records: [PageRecord; PAGES],
}

/// Fixed-capacity VT-d backend for one explicitly scoped requester and domain.
#[must_use = "a live VT-d backend owns requester translation authority and must be released explicitly"]
pub struct VtdDmaBackend<
    Registers: VtdRegisterBackend,
    Memory,
    Tables,
    const PAGES: usize,
    const BATCHES: usize,
    const MAX_BATCH_PAGES: usize,
> {
    core: SpinLock<
        Option<VtdBackendCore<Registers, Memory, Tables, PAGES, BATCHES, MAX_BATCH_PAGES>>,
    >,
}

impl<
    Registers: VtdRegisterBackend,
    Memory: SlptPageMemory,
    Tables: VtdRootContextStorage,
    const PAGES: usize,
    const BATCHES: usize,
    const MAX_BATCH_PAGES: usize,
> VtdDmaBackend<Registers, Memory, Tables, PAGES, BATCHES, MAX_BATCH_PAGES>
{
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        scope: impl Into<VtdRequesterScope>,
        registers: Registers,
        memory: Memory,
        tables: Tables,
        slpt_root: SlptFrame,
        domain_id: u16,
        poll_limit: u32,
    ) -> Result<Self, VtdBackendBuildFailure<Registers, Memory, Tables, PAGES>> {
        let scope = scope.into();
        let engine = match VtdRemappingEngine::probe(scope.unit(), registers) {
            Ok(engine) => engine,
            Err(failure) => {
                let fault = failure.fault();
                return Err(build_failure(
                    VtdBackendBuildFault::Probe(fault),
                    VtdEngineOwnership::Registers(failure.into_registers()),
                    memory,
                    tables,
                    None,
                    slpt_root,
                    scope,
                    false,
                ));
            }
        };

        if PAGES == 0
            || BATCHES == 0
            || MAX_BATCH_PAGES == 0
            || BATCHES > u16::MAX as usize
            || poll_limit == 0
        {
            return Err(build_failure(
                VtdBackendBuildFault::InvalidCapacity,
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                None,
                slpt_root,
                scope,
                false,
            ));
        }
        let root_pa = tables.root_table_physical_address();
        let context_pa = tables.context_table_physical_address();
        if root_pa == context_pa
            || root_pa == slpt_root.physical_address()
            || context_pa == slpt_root.physical_address()
        {
            return Err(build_failure(
                VtdBackendBuildFault::InvalidTableAlias,
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                None,
                slpt_root,
                scope,
                false,
            ));
        }
        if !root_table_is_empty(tables.root_table()) {
            return Err(build_failure(
                VtdBackendBuildFault::RootTableNotEmpty,
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                None,
                slpt_root,
                scope,
                false,
            ));
        }
        if !context_table_is_empty(tables.context_table()) {
            return Err(build_failure(
                VtdBackendBuildFault::ContextTableNotEmpty,
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                None,
                slpt_root,
                scope,
                false,
            ));
        }

        let capabilities = engine.capabilities();
        let Some(address_width_encoding) = select_address_width(
            capabilities.supported_adjusted_guest_widths,
            capabilities.maximum_guest_address_width,
        ) else {
            return Err(build_failure(
                VtdBackendBuildFault::Slpt(SlptFault::UnsupportedAddressWidth),
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                None,
                slpt_root,
                scope,
                false,
            ));
        };
        let config = SlptConfig {
            supported_adjusted_guest_widths: capabilities.supported_adjusted_guest_widths,
            maximum_guest_address_width: capabilities.maximum_guest_address_width,
            address_width_encoding,
        };
        let slpt = match Slpt::attach(&memory, slpt_root, config) {
            Ok(slpt) => slpt,
            Err(fault) => {
                return Err(build_failure(
                    VtdBackendBuildFault::Slpt(fault),
                    VtdEngineOwnership::Engine(engine),
                    memory,
                    tables,
                    None,
                    slpt_root,
                    scope,
                    false,
                ));
            }
        };

        if let Err(fault) = tables
            .context_table()
            .entry(scope.requester())
            .install_second_level_translation(
                domain_id,
                slpt.address_width_encoding(),
                slpt.root().physical_address(),
            )
        {
            return Err(build_failure(
                VtdBackendBuildFault::Table(fault),
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                Some(slpt),
                slpt_root,
                scope,
                false,
            ));
        }
        if let Err(fault) = tables
            .root_table()
            .entry(scope.requester().bus)
            .install_context_table(context_pa)
        {
            tables.context_table().entry(scope.requester()).clear();
            return Err(build_failure(
                VtdBackendBuildFault::Table(fault),
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                Some(slpt),
                slpt_root,
                scope,
                false,
            ));
        }

        let mut engine = engine;
        if let Err(fault) = engine.enable(root_pa, poll_limit) {
            let safe_to_clear = engine.state() == VtdEngineState::Disabled;
            if safe_to_clear {
                clear_requester_entries(&tables, scope.requester());
            }
            return Err(build_failure(
                VtdBackendBuildFault::Enable(fault),
                VtdEngineOwnership::Engine(engine),
                memory,
                tables,
                Some(slpt),
                slpt_root,
                scope,
                !safe_to_clear,
            ));
        }

        Ok(Self {
            core: SpinLock::new(Some(VtdBackendCore {
                engine,
                memory,
                tables,
                slpt,
                scope,
                poll_limit,
                domain_generation: 1,
                domain_active: false,
                domain_retired: false,
                authority_released: false,
                poisoned: false,
                last_engine_fault: None,
                batches: [BatchSlot::EMPTY; BATCHES],
                page_records: [PageRecord::Empty; PAGES],
            })),
        })
    }

    pub fn last_engine_fault(&self) -> Option<VtdEngineFault> {
        self.core
            .lock()
            .as_ref()
            .and_then(|core| core.last_engine_fault)
    }

    pub fn batch_handle(&self, device_address: u64, length: usize) -> Option<VtdBatchHandle> {
        let core = self.core.lock();
        let core = core.as_ref()?;
        core.batches.iter().enumerate().find_map(|(index, batch)| {
            (batch.phase != BatchPhase::Empty
                && batch.device_address == device_address
                && batch.length == length)
                .then_some(VtdBatchHandle {
                    slot: index as u16,
                    generation: batch.generation,
                })
        })
    }

    pub fn batch_is_live(&self, handle: VtdBatchHandle) -> bool {
        self.core
            .lock()
            .as_ref()
            .and_then(|core| core.batches.get(usize::from(handle.slot)))
            .is_some_and(|batch| {
                batch.phase != BatchPhase::Empty && batch.generation == handle.generation
            })
    }

    pub fn shutdown(
        self,
    ) -> Result<
        VtdReleasedResources<Registers, Memory, Tables, PAGES>,
        VtdBackendShutdownFailure<Self>,
    > {
        let failure = |fault, backend| Err(VtdBackendShutdownFailure { fault, backend });
        {
            let mut guard = self.core.lock();
            let Some(core) = guard.as_mut() else {
                drop(guard);
                return failure(VtdBackendShutdownFault::Unavailable, self);
            };
            if core.domain_active || core.batches.iter().any(|b| b.phase != BatchPhase::Empty) {
                drop(guard);
                return failure(VtdBackendShutdownFault::DomainActive, self);
            }
            if !core.authority_released {
                if let Err(fault) = core.engine.disable(core.poll_limit) {
                    core.last_engine_fault = Some(fault);
                    if core.engine.state() == VtdEngineState::Poisoned {
                        core.poisoned = true;
                    }
                    drop(guard);
                    return failure(VtdBackendShutdownFault::Disable(fault), self);
                }
                clear_requester_entries(&core.tables, core.scope.requester());
                core.authority_released = true;
            }
        }
        let mut guard = self.core.lock();
        let core = guard.take().unwrap();
        drop(guard);
        Ok(VtdReleasedResources {
            engine: core.engine,
            memory: core.memory,
            tables: core.tables,
            slpt: core.slpt,
            scope: core.scope,
        })
    }
}

impl<
    Registers: VtdRegisterBackend + Send,
    Memory: SlptPageMemory + Send,
    Tables: VtdRootContextStorage,
    const PAGES: usize,
    const BATCHES: usize,
    const MAX_BATCH_PAGES: usize,
> DmaRemappingBackend
    for VtdDmaBackend<Registers, Memory, Tables, PAGES, BATCHES, MAX_BATCH_PAGES>
{
    fn isolate_device(&self, device: PciAddress) -> Result<Handle, Status> {
        let mut guard = self.core.lock();
        let core = guard.as_mut().ok_or(STATUS_IO_ERROR)?;
        if device != core.scope.requester() {
            return Err(STATUS_UNSUPPORTED);
        }
        if core.poisoned || core.authority_released {
            return Err(STATUS_IO_ERROR);
        }
        if core.domain_active {
            return Err(STATUS_BUSY);
        }
        if core.domain_retired {
            return Err(STATUS_UNSUPPORTED);
        }
        core.domain_active = true;
        Ok(domain_handle(core.domain_generation))
    }

    fn map(
        &self,
        domain: Handle,
        device_address: u64,
        physical_address: u64,
        length: usize,
        access: DmaAccess,
    ) -> Status {
        let mut guard = self.core.lock();
        let Some(core) = guard.as_mut() else {
            return STATUS_IO_ERROR;
        };
        core.map_span(domain, device_address, physical_address, length, access)
    }

    fn unmap(&self, domain: Handle, device_address: u64, length: usize) -> Status {
        let mut guard = self.core.lock();
        let Some(core) = guard.as_mut() else {
            return STATUS_IO_ERROR;
        };
        core.unmap_span(domain, device_address, length)
    }

    fn release_domain(&self, domain: Handle) -> Status {
        let mut guard = self.core.lock();
        let Some(core) = guard.as_mut() else {
            return STATUS_IO_ERROR;
        };
        if !core.valid_domain(domain) {
            return STATUS_NOT_FOUND;
        }

        let mut cleanup_failed = false;
        for index in 0..BATCHES {
            if core.batches[index].phase != BatchPhase::Empty {
                match core.cleanup_batch(index) {
                    Ok(true) => {}
                    Ok(false) | Err(()) => cleanup_failed = true,
                }
            }
        }
        if cleanup_failed || core.batches.iter().any(|b| b.phase != BatchPhase::Empty) {
            return STATUS_IO_ERROR;
        }
        if let Err(fault) = core.engine.disable(core.poll_limit) {
            core.last_engine_fault = Some(fault);
            if core.engine.state() == VtdEngineState::Poisoned {
                core.poisoned = true;
            }
            return STATUS_IO_ERROR;
        }
        clear_requester_entries(&core.tables, core.scope.requester());
        core.authority_released = true;
        core.domain_active = false;
        if core.domain_generation == u32::MAX {
            core.domain_retired = true;
        } else {
            core.domain_generation += 1;
        }
        STATUS_OK
    }
}

impl<
    Registers: VtdRegisterBackend,
    Memory: SlptPageMemory,
    Tables: VtdRootContextStorage,
    const PAGES: usize,
    const BATCHES: usize,
    const MAX_BATCH_PAGES: usize,
> VtdBackendCore<Registers, Memory, Tables, PAGES, BATCHES, MAX_BATCH_PAGES>
{
    fn valid_domain(&self, domain: Handle) -> bool {
        self.domain_active
            && !self.authority_released
            && domain == domain_handle(self.domain_generation)
    }

    fn map_span(
        &mut self,
        domain: Handle,
        device_address: u64,
        physical_address: u64,
        length: usize,
        access: DmaAccess,
    ) -> Status {
        if !self.valid_domain(domain) {
            return STATUS_NOT_FOUND;
        }
        if self.poisoned
            || self
                .batches
                .iter()
                .any(|batch| batch.phase == BatchPhase::Quarantined)
        {
            return STATUS_IO_ERROR;
        }
        let Ok(length_u64) = u64::try_from(length) else {
            return STATUS_INVALID_ARGUMENT;
        };
        if length == 0
            || device_address % PAGE_SIZE != 0
            || physical_address % PAGE_SIZE != 0
            || length_u64 % PAGE_SIZE != 0
            || device_address.checked_add(length_u64).is_none()
            || physical_address.checked_add(length_u64).is_none()
            || access.bits() == 0
            || access.bits() & !DmaAccess::READ_WRITE.bits() != 0
        {
            return STATUS_INVALID_ARGUMENT;
        }
        let page_count = (length_u64 / PAGE_SIZE) as usize;
        if page_count > MAX_BATCH_PAGES || page_count > PAGES {
            return STATUS_BUSY;
        }
        if self.batches.iter().any(|batch| {
            batch.phase != BatchPhase::Empty
                && (ranges_overlap(batch.device_address, batch.length, device_address, length)
                    || ranges_overlap(
                        batch.physical_address,
                        batch.length,
                        physical_address,
                        length,
                    ))
        }) {
            return STATUS_BUSY;
        }
        let Some(index) = self
            .batches
            .iter()
            .position(|batch| batch.phase == BatchPhase::Empty && !batch.retired)
        else {
            return STATUS_BUSY;
        };
        let Some(page_start) = self.find_free_page_run(page_count) else {
            return STATUS_BUSY;
        };

        let generation = self.batches[index].generation;
        self.batches[index] = BatchSlot {
            generation,
            retired: false,
            domain_generation: self.domain_generation,
            phase: BatchPhase::Staging,
            device_address,
            physical_address,
            length,
            page_start,
            page_count,
            was_active: false,
        };

        let permissions = DmaPermissions {
            read: access.bits() & DmaAccess::READ.bits() != 0,
            write: access.bits() & DmaAccess::WRITE.bits() != 0,
        };
        let mut failure = None;
        for page in 0..page_count {
            let offset = page as u64 * PAGE_SIZE;
            let outcome = {
                let mut invalidator = EngineInvalidator {
                    engine: &mut self.engine,
                    poll_limit: self.poll_limit,
                    last_fault: &mut self.last_engine_fault,
                };
                self.slpt.map_page(
                    &mut self.memory,
                    &mut invalidator,
                    device_address + offset,
                    physical_address + offset,
                    permissions,
                )
            };
            let page_slot = page_start + page;
            match outcome {
                Ok(MapOutcome::Active(handle)) => {
                    self.page_records[page_slot] = PageRecord::Active(handle);
                }
                Ok(MapOutcome::Pending { receipt, .. }) => {
                    self.page_records[page_slot] = PageRecord::MapPending(receipt);
                    if self.engine.state() == VtdEngineState::Poisoned {
                        self.poisoned = true;
                    }
                    failure = Some(STATUS_IO_ERROR);
                    break;
                }
                Err(fault) => {
                    failure = Some(slpt_status(fault));
                    if matches!(fault, SlptFault::Poisoned | SlptFault::RollbackFailed) {
                        self.poisoned = true;
                    }
                    break;
                }
            }
        }

        if let Some(status) = failure {
            self.batches[index].phase = BatchPhase::Quarantined;
            let _ = self.cleanup_batch(index);
            return status;
        }
        self.batches[index].phase = BatchPhase::Active;
        self.batches[index].was_active = true;
        STATUS_OK
    }

    fn unmap_span(&mut self, domain: Handle, device_address: u64, length: usize) -> Status {
        if !self.valid_domain(domain) {
            return STATUS_NOT_FOUND;
        }
        let Some(index) = self.batches.iter().position(|batch| {
            batch.phase != BatchPhase::Empty
                && batch.domain_generation == self.domain_generation
                && batch.device_address == device_address
                && batch.length == length
        }) else {
            return STATUS_NOT_FOUND;
        };
        let was_active = self.batches[index].was_active;
        self.batches[index].phase = BatchPhase::Quarantined;
        match self.cleanup_batch(index) {
            Ok(true) if was_active => STATUS_OK,
            Ok(true) => STATUS_NOT_FOUND,
            Ok(false) | Err(()) => STATUS_IO_ERROR,
        }
    }

    /// Returns `Ok(true)` only after every page receipt has been discharged.
    fn cleanup_batch(&mut self, index: usize) -> Result<bool, ()> {
        let page_count = self.batches[index].page_count;
        let page_start = self.batches[index].page_start;
        for page in 0..page_count {
            let page_slot = page_start + page;
            let record = self.page_records[page_slot];
            let outcome = match record {
                PageRecord::Empty => continue,
                PageRecord::Active(handle) => {
                    let mut invalidator = EngineInvalidator {
                        engine: &mut self.engine,
                        poll_limit: self.poll_limit,
                        last_fault: &mut self.last_engine_fault,
                    };
                    self.slpt
                        .unmap_page(&mut self.memory, &mut invalidator, handle)
                }
                PageRecord::MapPending(receipt) => {
                    let mut invalidator = EngineInvalidator {
                        engine: &mut self.engine,
                        poll_limit: self.poll_limit,
                        last_fault: &mut self.last_engine_fault,
                    };
                    self.slpt
                        .cancel_pending_map(&mut self.memory, &mut invalidator, receipt)
                }
                PageRecord::UnmapPending(receipt) => {
                    let mut invalidator = EngineInvalidator {
                        engine: &mut self.engine,
                        poll_limit: self.poll_limit,
                        last_fault: &mut self.last_engine_fault,
                    };
                    self.slpt
                        .finish_unmap(&mut self.memory, &mut invalidator, receipt)
                }
            };
            match outcome {
                Ok(UnmapOutcome::Complete) => {
                    self.page_records[page_slot] = PageRecord::Empty;
                }
                Ok(UnmapOutcome::Pending { receipt, .. }) => {
                    self.page_records[page_slot] = PageRecord::UnmapPending(receipt);
                    if self.engine.state() == VtdEngineState::Poisoned {
                        self.poisoned = true;
                    }
                    // Reclamation is deliberately serialized within a batch.
                    // A cleanup-pending record can still retain a restored
                    // parent link; allowing a later sibling to reclaim that
                    // same table would invalidate the first receipt's path.
                    self.batches[index].phase = BatchPhase::Quarantined;
                    return Ok(false);
                }
                Err(_) => {
                    self.poisoned = true;
                    self.batches[index].phase = BatchPhase::Quarantined;
                    return Err(());
                }
            }
        }
        self.retire_batch(index);
        Ok(true)
    }

    fn retire_batch(&mut self, index: usize) {
        let generation = self.batches[index].generation;
        let retired = generation == u32::MAX;
        self.batches[index] = BatchSlot::EMPTY;
        if retired {
            self.batches[index].generation = generation;
            self.batches[index].retired = true;
        } else {
            self.batches[index].generation = generation + 1;
        }
    }

    fn find_free_page_run(&self, page_count: usize) -> Option<usize> {
        if page_count == 0 || page_count > PAGES {
            return None;
        }
        self.page_records.windows(page_count).position(|records| {
            records
                .iter()
                .all(|record| matches!(record, PageRecord::Empty))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdBackendShutdownFault {
    DomainActive,
    Disable(VtdEngineFault),
    Unavailable,
}

pub struct VtdBackendShutdownFailure<Backend> {
    fault: VtdBackendShutdownFault,
    backend: Backend,
}

impl<Backend> VtdBackendShutdownFailure<Backend> {
    pub const fn fault(&self) -> VtdBackendShutdownFault {
        self.fault
    }

    pub fn into_backend(self) -> Backend {
        self.backend
    }
}

impl<Backend> core::fmt::Debug for VtdBackendShutdownFailure<Backend> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("VtdBackendShutdownFailure")
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

pub struct VtdReleasedResources<Registers: VtdRegisterBackend, Memory, Tables, const PAGES: usize> {
    pub engine: VtdRemappingEngine<Registers>,
    pub memory: Memory,
    pub tables: Tables,
    pub slpt: Slpt<PAGES>,
    pub scope: VtdRequesterScope,
}

#[allow(clippy::too_many_arguments)]
fn build_failure<Registers: VtdRegisterBackend, Memory, Tables, const PAGES: usize>(
    fault: VtdBackendBuildFault,
    engine: VtdEngineOwnership<Registers>,
    memory: Memory,
    tables: Tables,
    slpt: Option<Slpt<PAGES>>,
    slpt_root: SlptFrame,
    scope: VtdRequesterScope,
    tables_installed: bool,
) -> VtdBackendBuildFailure<Registers, Memory, Tables, PAGES> {
    VtdBackendBuildFailure {
        fault,
        ownership: VtdBackendBuildOwnership {
            engine,
            memory,
            tables,
            slpt,
            slpt_root,
            scope,
            tables_installed,
        },
    }
}

fn select_address_width(supported: u8, maximum: u8) -> Option<u8> {
    [(3, 57), (2, 48), (1, 39), (0, 30)]
        .into_iter()
        .find_map(|(encoding, width)| {
            (supported & (1 << encoding) != 0 && width <= maximum).then_some(encoding)
        })
}

fn root_table_is_empty(table: &RootEntryTable) -> bool {
    (0..=u8::MAX).all(|bus| table.entry(bus).raw() == (0, 0))
}

fn context_table_is_empty(table: &ContextEntryTable) -> bool {
    (0..=u8::MAX).all(|bus| {
        (0..32).all(|slot| {
            (0..8).all(|function| {
                let device = PciAddress {
                    bus,
                    slot,
                    function,
                };
                table.entry(device).raw() == (0, 0)
            })
        })
    })
}

fn clear_requester_entries(tables: &impl VtdRootContextStorage, requester: PciAddress) {
    // Translation is disabled before this is called on a live backend.  Clear
    // the root first so no future walk can acquire the context table.
    tables.root_table().entry(requester.bus).clear();
    tables.context_table().entry(requester).clear();
}

const fn domain_handle(generation: u32) -> Handle {
    ((generation as u64) << 32) | 1
}

fn ranges_overlap(
    first_start: u64,
    first_length: usize,
    second_start: u64,
    second_length: usize,
) -> bool {
    let first_end = first_start.saturating_add(first_length as u64);
    let second_end = second_start.saturating_add(second_length as u64);
    first_start < second_end && second_start < first_end
}

fn slpt_status(fault: SlptFault) -> Status {
    match fault {
        SlptFault::InvalidIova
        | SlptFault::InvalidPhysicalAddress
        | SlptFault::InvalidPermissions => STATUS_INVALID_ARGUMENT,
        SlptFault::CapacityExhausted | SlptFault::IovaOverlap | SlptFault::PhysicalAlias => {
            STATUS_BUSY
        }
        _ => STATUS_IO_ERROR,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::thread;

    use super::super::vtd::{VtdOperation, VtdRegisterError};
    use super::*;

    const VER: usize = 0x00;
    const CAP: usize = 0x08;
    const ECAP: usize = 0x10;
    const GCMD: usize = 0x18;
    const GSTS: usize = 0x1c;
    const RTADDR: usize = 0x20;
    const CCMD: usize = 0x28;
    const FSTS: usize = 0x34;
    const IOTLB: usize = 0x108;
    const GCMD_TE: u32 = 1 << 31;
    const GCMD_SRTP: u32 = 1 << 30;
    const GCMD_WBF: u32 = 1 << 27;
    const GSTS_TES: u32 = 1 << 31;
    const GSTS_RTPS: u32 = 1 << 30;
    const CCMD_ICC: u64 = 1 << 63;
    const CCMD_CAIG_GLOBAL: u64 = 1 << 59;
    const IOTLB_IVT: u64 = 1 << 63;
    const IOTLB_IAIG_GLOBAL: u64 = 1 << 57;
    const TEST_PAGES: usize = 8;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RegisterFault {
        None,
        RejectNextIotlb,
        StallDisableOnce,
    }

    struct RegisterState {
        global_status: u32,
        root_address: u64,
        context_command: u64,
        iotlb_command: u64,
        fault: RegisterFault,
        enabled_with_root: u64,
        table_updates: usize,
    }

    struct TestRegisters {
        state: SpinLock<RegisterState>,
    }

    impl TestRegisters {
        fn new() -> Self {
            Self {
                state: SpinLock::new(RegisterState {
                    global_status: 0,
                    root_address: 0x9000,
                    context_command: 0,
                    iotlb_command: 0,
                    fault: RegisterFault::None,
                    enabled_with_root: 0,
                    table_updates: 0,
                }),
            }
        }

        fn fault(&self, fault: RegisterFault) {
            self.state.lock().fault = fault;
        }
    }

    impl VtdRegisterBackend for TestRegisters {
        fn read_u32(&self, offset: usize) -> Result<u32, VtdRegisterError> {
            let state = self.state.lock();
            match offset {
                VER => Ok(0x10),
                GSTS => Ok(state.global_status),
                FSTS => Ok(0),
                _ => Err(VtdRegisterError::Access),
            }
        }

        fn write_u32(&self, offset: usize, value: u32) -> Result<(), VtdRegisterError> {
            if offset != GCMD {
                return Err(VtdRegisterError::Access);
            }
            let mut state = self.state.lock();
            if value & GCMD_SRTP != 0 {
                state.global_status |= GSTS_RTPS;
            }
            if value & GCMD_WBF != 0 {
                // The model never exposes a pending write-buffer flush.
            }
            if value & GCMD_TE != 0 {
                state.global_status |= GSTS_TES;
                state.enabled_with_root = state.root_address;
            } else {
                if state.fault == RegisterFault::StallDisableOnce {
                    state.fault = RegisterFault::None;
                } else {
                    state.global_status &= !GSTS_TES;
                }
            }
            Ok(())
        }

        fn read_u64(&self, offset: usize) -> Result<u64, VtdRegisterError> {
            let state = self.state.lock();
            match offset {
                CAP => Ok((47 << 16) | (1 << 10) | (1 << 4)),
                ECAP => Ok(0x10 << 8),
                RTADDR => Ok(state.root_address),
                CCMD => Ok(state.context_command),
                IOTLB => Ok(state.iotlb_command),
                _ => Err(VtdRegisterError::Access),
            }
        }

        fn write_u64(&self, offset: usize, value: u64) -> Result<(), VtdRegisterError> {
            let mut state = self.state.lock();
            match offset {
                RTADDR => state.root_address = value,
                CCMD if value & CCMD_ICC != 0 => {
                    state.context_command = CCMD_CAIG_GLOBAL;
                }
                IOTLB if value & IOTLB_IVT != 0 => {
                    state.table_updates += 1;
                    if state.fault == RegisterFault::RejectNextIotlb {
                        state.fault = RegisterFault::None;
                        state.iotlb_command = 0;
                    } else {
                        state.iotlb_command = IOTLB_IAIG_GLOBAL;
                    }
                }
                _ => return Err(VtdRegisterError::Access),
            }
            Ok(())
        }

        fn relax(&self) {}
    }

    #[derive(Clone, Copy)]
    struct TestPage {
        physical_address: u64,
        allocated: bool,
        entries: [u64; 512],
    }

    impl TestPage {
        const EMPTY: Self = Self {
            physical_address: 0,
            allocated: false,
            entries: [0; 512],
        };
    }

    struct TestMemory {
        pages: [TestPage; TEST_PAGES],
    }

    impl TestMemory {
        fn new() -> Self {
            let mut pages = [TestPage::EMPTY; TEST_PAGES];
            for (index, page) in pages.iter_mut().enumerate() {
                page.physical_address = 0x3000 + index as u64 * PAGE_SIZE;
            }
            pages[0].allocated = true;
            Self { pages }
        }

        fn root(&self) -> SlptFrame {
            SlptFrame::from_physical_address(self.pages[0].physical_address).unwrap()
        }

        fn index(
            &self,
            frame: SlptFrame,
        ) -> Result<usize, super::super::vtd_slpt::SlptMemoryError> {
            self.pages
                .iter()
                .position(|page| {
                    page.allocated && page.physical_address == frame.physical_address()
                })
                .ok_or(super::super::vtd_slpt::SlptMemoryError(1))
        }

        fn allocated_count(&self) -> usize {
            self.pages.iter().filter(|page| page.allocated).count()
        }
    }

    impl SlptPageMemory for TestMemory {
        fn allocate_table(&mut self) -> Result<SlptFrame, super::super::vtd_slpt::SlptMemoryError> {
            let page = self
                .pages
                .iter_mut()
                .find(|page| !page.allocated)
                .ok_or(super::super::vtd_slpt::SlptMemoryError(2))?;
            page.allocated = true;
            SlptFrame::from_physical_address(page.physical_address)
                .ok_or(super::super::vtd_slpt::SlptMemoryError(3))
        }

        fn zero_table(
            &mut self,
            frame: SlptFrame,
        ) -> Result<(), super::super::vtd_slpt::SlptMemoryError> {
            let index = self.index(frame)?;
            self.pages[index].entries.fill(0);
            Ok(())
        }

        fn read_entry(
            &self,
            frame: SlptFrame,
            index: usize,
        ) -> Result<u64, super::super::vtd_slpt::SlptMemoryError> {
            let page = self.index(frame)?;
            self.pages[page]
                .entries
                .get(index)
                .copied()
                .ok_or(super::super::vtd_slpt::SlptMemoryError(4))
        }

        fn write_entry(
            &mut self,
            frame: SlptFrame,
            index: usize,
            value: u64,
        ) -> Result<(), super::super::vtd_slpt::SlptMemoryError> {
            let page = self.index(frame)?;
            *self.pages[page]
                .entries
                .get_mut(index)
                .ok_or(super::super::vtd_slpt::SlptMemoryError(4))? = value;
            Ok(())
        }

        fn release_table(
            &mut self,
            frame: SlptFrame,
        ) -> Result<(), super::super::vtd_slpt::SlptMemoryError> {
            let index = self.index(frame)?;
            if index == 0 {
                return Err(super::super::vtd_slpt::SlptMemoryError(5));
            }
            self.pages[index].allocated = false;
            Ok(())
        }
    }

    struct TestTables {
        roots: RootEntryTable,
        contexts: ContextEntryTable,
    }

    impl TestTables {
        fn new() -> Self {
            Self {
                roots: RootEntryTable::new(),
                contexts: ContextEntryTable::new(),
            }
        }
    }

    // SAFETY: deterministic tests model the two fields as pinned pages at the
    // two distinct synthetic addresses below; no real DMA occurs.
    unsafe impl VtdRootContextStorage for TestTables {
        fn root_table(&self) -> &RootEntryTable {
            &self.roots
        }

        fn root_table_physical_address(&self) -> u64 {
            0x1000
        }

        fn context_table(&self) -> &ContextEntryTable {
            &self.contexts
        }

        fn context_table_physical_address(&self) -> u64 {
            0x2000
        }
    }

    type Backend = VtdDmaBackend<TestRegisters, TestMemory, TestTables, 16, 4, 4>;
    type MaximumScratchpadBackend =
        VtdDmaBackend<TestRegisters, TestMemory, TestTables, 1029, 6, 1023>;

    fn unit() -> DmarRemappingUnit {
        // All fields are integers, so the all-zero value is valid; private
        // endpoint offsets remain zero and are irrelevant after scope proof.
        let mut unit: DmarRemappingUnit = unsafe { core::mem::zeroed() };
        unit.segment = 0;
        unit.register_base = 0xfed9_0000;
        unit.include_all = false;
        unit
    }

    fn scope() -> SingleRequesterScope {
        SingleRequesterScope {
            unit: unit(),
            requester: PciAddress::new(2, 3, 1).unwrap(),
        }
    }

    fn include_all_scope() -> IsolatedIncludeAllRequesterScope {
        let mut include_all = unit();
        include_all.include_all = true;
        IsolatedIncludeAllRequesterScope {
            unit: include_all,
            requester: scope().requester(),
        }
    }

    fn shared_unit_scope() -> IsolatedSharedUnitRequesterScope {
        IsolatedSharedUnitRequesterScope {
            unit: unit(),
            requester: scope().requester(),
        }
    }

    fn backend() -> Backend {
        let memory = TestMemory::new();
        let root = memory.root();
        Backend::build(
            scope(),
            TestRegisters::new(),
            memory,
            TestTables::new(),
            root,
            7,
            4,
        )
        .unwrap()
    }

    fn maximum_scratchpad_backend() -> MaximumScratchpadBackend {
        let memory = TestMemory::new();
        let root = memory.root();
        MaximumScratchpadBackend::build(
            scope(),
            TestRegisters::new(),
            memory,
            TestTables::new(),
            root,
            7,
            4,
        )
        .unwrap()
    }

    #[test]
    fn construction_rejects_a_context_entry_on_any_bus() {
        let memory = TestMemory::new();
        let root = memory.root();
        let tables = TestTables::new();
        tables
            .contexts
            .entry(PciAddress::new(9, 1, 0).unwrap())
            .install_second_level_translation(3, 2, root.physical_address())
            .unwrap();
        let failure =
            match Backend::build(scope(), TestRegisters::new(), memory, tables, root, 7, 4) {
                Ok(_) => panic!("a stale nonzero-bus context entry must be rejected"),
                Err(failure) => failure,
            };
        assert_eq!(failure.fault(), VtdBackendBuildFault::ContextTableNotEmpty);
    }

    #[test]
    fn include_all_policy_still_publishes_only_its_single_requester_context() {
        let memory = TestMemory::new();
        let root = memory.root();
        let backend = Backend::build(
            include_all_scope(),
            TestRegisters::new(),
            memory,
            TestTables::new(),
            root,
            7,
            4,
        )
        .unwrap();
        let guard = backend.core.lock();
        let core = guard.as_ref().unwrap();
        assert_eq!(
            core.scope,
            VtdRequesterScope::IsolatedIncludeAll(include_all_scope())
        );
        assert_ne!(
            core.tables.roots.entry(scope().requester().bus).raw(),
            (0, 0)
        );
        assert_eq!(core.tables.roots.entry(1).raw(), (0, 0));
        assert_eq!(
            core.tables.contexts.entry(scope().requester()).raw(),
            (0x3001, (7 << 8) | 2)
        );
        assert_eq!(
            core.tables
                .contexts
                .entry(PciAddress::new(2, 3, 0).unwrap())
                .raw(),
            (0, 0)
        );
    }

    #[test]
    fn shared_unit_policy_still_publishes_only_its_single_requester_context() {
        let memory = TestMemory::new();
        let root = memory.root();
        let backend = Backend::build(
            shared_unit_scope(),
            TestRegisters::new(),
            memory,
            TestTables::new(),
            root,
            7,
            4,
        )
        .unwrap();
        let guard = backend.core.lock();
        let core = guard.as_ref().unwrap();
        assert_eq!(
            core.scope,
            VtdRequesterScope::IsolatedSharedUnit(shared_unit_scope())
        );
        assert_ne!(
            core.tables.roots.entry(scope().requester().bus).raw(),
            (0, 0)
        );
        assert_eq!(core.tables.roots.entry(1).raw(), (0, 0));
        assert_eq!(
            core.tables.contexts.entry(scope().requester()).raw(),
            (0x3001, (7 << 8) | 2)
        );
        assert_eq!(
            core.tables
                .contexts
                .entry(PciAddress::new(2, 3, 0).unwrap())
                .raw(),
            (0, 0)
        );
    }

    #[test]
    fn publishes_only_the_owned_requester_before_enable() {
        let backend = backend();
        let guard = backend.core.lock();
        let core = guard.as_ref().unwrap();
        assert_eq!(core.engine.state(), VtdEngineState::Enabled);
        assert_eq!(
            core.engine.registers().state.lock().enabled_with_root,
            0x1000
        );
        assert_eq!(core.tables.roots.entry(2).raw(), (0x2001, 0));
        assert_eq!(
            core.tables.contexts.entry(scope().requester).raw(),
            (0x3001, (7 << 8) | 2)
        );
        assert_eq!(core.tables.roots.entry(1).raw(), (0, 0));
        assert_eq!(
            core.tables
                .contexts
                .entry(PciAddress::new(2, 3, 0).unwrap())
                .raw(),
            (0, 0)
        );
    }

    #[test]
    fn maps_and_unmaps_a_multi_page_batch_transactionally() {
        let backend = backend();
        let handle = backend.isolate_device(scope().requester).unwrap();
        assert_eq!(
            backend.map(
                handle,
                0x20_0000,
                0x80_0000,
                3 * PAGE_SIZE as usize,
                DmaAccess::READ_WRITE
            ),
            STATUS_OK
        );
        let batch = backend
            .batch_handle(0x20_0000, 3 * PAGE_SIZE as usize)
            .unwrap();
        assert_eq!(batch.generation(), 1);
        assert_eq!(
            backend.unmap(handle, 0x20_0000, 3 * PAGE_SIZE as usize),
            STATUS_OK
        );
        assert!(!backend.batch_is_live(batch));
        assert!(
            backend
                .batch_handle(0x20_0000, 3 * PAGE_SIZE as usize)
                .is_none()
        );
        assert_eq!(
            backend.map(
                handle,
                0x20_0000,
                0x80_0000,
                PAGE_SIZE as usize,
                DmaAccess::READ_WRITE,
            ),
            STATUS_OK
        );
        let replacement = backend.batch_handle(0x20_0000, PAGE_SIZE as usize).unwrap();
        assert_eq!(replacement.slot(), batch.slot());
        assert_eq!(replacement.generation(), batch.generation() + 1);
        assert!(!backend.batch_is_live(batch));
        assert!(backend.batch_is_live(replacement));
        assert_eq!(
            backend.unmap(handle, 0x20_0000, PAGE_SIZE as usize),
            STATUS_OK
        );
        let guard = backend.core.lock();
        assert_eq!(guard.as_ref().unwrap().memory.allocated_count(), 1);
    }

    #[test]
    fn maximum_xhci_scratchpad_profile_maps_and_revokes_six_exact_spans() {
        thread::Builder::new()
            // The production bootstrap stack is 8 MiB. Exercise the maximum
            // xHCI profile at that same bound instead of making this host test
            // depend on a runner-specific default worker stack.
            .stack_size(8 * 1024 * 1024)
            .spawn(maximum_xhci_scratchpad_profile)
            .unwrap()
            .join()
            .unwrap();
    }

    fn maximum_xhci_scratchpad_profile() {
        let backend = maximum_scratchpad_backend();
        let domain = backend.isolate_device(scope().requester).unwrap();
        let spans = [
            (0x0020_0000, 0x0020_0000, PAGE_SIZE as usize),
            (0x0020_1000, 0x0020_1000, PAGE_SIZE as usize),
            (0x0020_2000, 0x0020_2000, PAGE_SIZE as usize),
            (0x0020_3000, 0x0020_3000, PAGE_SIZE as usize),
            (0x0020_4000, 0x0020_4000, 2 * PAGE_SIZE as usize),
            (0x0040_0000, 0x0040_0000, 1023 * PAGE_SIZE as usize),
        ];
        for (device_address, physical_address, length) in spans {
            assert_eq!(
                backend.map(
                    domain,
                    device_address,
                    physical_address,
                    length,
                    DmaAccess::READ_WRITE,
                ),
                STATUS_OK
            );
        }
        {
            let guard = backend.core.lock();
            let core = guard.as_ref().unwrap();
            assert_eq!(
                core.batches.iter().filter(|batch| batch.was_active).count(),
                6
            );
            assert_eq!(core.memory.allocated_count(), 6);
        }
        for (device_address, _, length) in spans.into_iter().rev() {
            assert_eq!(backend.unmap(domain, device_address, length), STATUS_OK);
        }
        assert_eq!(backend.release_domain(domain), STATUS_OK);
        let released = backend.shutdown().unwrap();
        assert_eq!(released.memory.allocated_count(), 1);
    }

    #[test]
    fn invalidation_failure_quarantines_the_span_and_authority() {
        let backend = backend();
        let handle = backend.isolate_device(scope().requester).unwrap();
        {
            let guard = backend.core.lock();
            guard
                .as_ref()
                .unwrap()
                .engine
                .registers()
                .fault(RegisterFault::RejectNextIotlb);
        }
        assert_eq!(
            backend.map(
                handle,
                0x40_0000,
                0x90_0000,
                PAGE_SIZE as usize,
                DmaAccess::READ
            ),
            STATUS_IO_ERROR
        );
        assert!(
            backend
                .batch_handle(0x40_0000, PAGE_SIZE as usize)
                .is_some()
        );
        assert_eq!(
            backend.last_engine_fault(),
            Some(VtdEngineFault::CompletionRejected(
                VtdOperation::IotlbInvalidation
            ))
        );
        assert_eq!(backend.release_domain(handle), STATUS_IO_ERROR);
        let guard = backend.core.lock();
        let core = guard.as_ref().unwrap();
        assert!(core.domain_active);
        assert_ne!(core.tables.roots.entry(2).raw(), (0, 0));
    }

    #[test]
    fn disable_failure_retains_domain_and_retryable_backend_ownership() {
        let backend = backend();
        let handle = backend.isolate_device(scope().requester).unwrap();
        {
            let guard = backend.core.lock();
            guard
                .as_ref()
                .unwrap()
                .engine
                .registers()
                .fault(RegisterFault::StallDisableOnce);
        }
        assert_eq!(backend.release_domain(handle), STATUS_IO_ERROR);
        {
            let guard = backend.core.lock();
            let core = guard.as_ref().unwrap();
            assert!(core.domain_active);
            assert_ne!(core.tables.roots.entry(2).raw(), (0, 0));
        }
        assert_eq!(backend.release_domain(handle), STATUS_OK);
        assert_eq!(
            backend.map(
                handle,
                0x20_0000,
                0x80_0000,
                PAGE_SIZE as usize,
                DmaAccess::READ,
            ),
            STATUS_NOT_FOUND
        );
        let released = backend.shutdown().unwrap();
        assert_eq!(released.engine.state(), VtdEngineState::Disabled);
        assert_eq!(released.tables.roots.entry(2).raw(), (0, 0));
        assert_eq!(
            released.tables.contexts.entry(scope().requester).raw(),
            (0, 0)
        );
    }
}
