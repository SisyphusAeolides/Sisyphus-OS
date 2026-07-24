use core::sync::atomic::{AtomicU64, Ordering};

use sisyphus_driver_abi::{STATUS_OK, Status};

use crate::boot::acpi::DmarRemappingUnit;
use crate::capability::{Capability, DeviceMemoryRight};
use crate::mmio::{MmioAccessError, MmioWindow as TypedMmioWindow};

use super::pci::PciAddress;

const PAGE_SIZE: u64 = 4096;
const PHYSICAL_PAGE_MASK: u64 = 0x000f_ffff_ffff_f000;
const PHYSICAL_ADDRESS_MASK: u64 = 0x000f_ffff_ffff_ffff;
const PRESENT: u64 = 1;

const REGISTER_WINDOW_BYTES: usize = 0x4000;
const VER: usize = 0x00;
const CAP: usize = 0x08;
const ECAP: usize = 0x10;
const GCMD: usize = 0x18;
const GSTS: usize = 0x1c;
const RTADDR: usize = 0x20;
const CCMD: usize = 0x28;
const FSTS: usize = 0x34;

const GCMD_TE: u32 = 1 << 31;
const GCMD_SRTP: u32 = 1 << 30;
const GCMD_WBF: u32 = 1 << 27;

const GSTS_TES: u32 = 1 << 31;
const GSTS_RTPS: u32 = 1 << 30;
const GSTS_AFLS: u32 = 1 << 28;
const GSTS_WBFS: u32 = 1 << 27;
const GSTS_QIES: u32 = 1 << 26;
const GSTS_IRES: u32 = 1 << 25;
const GSTS_CFIS: u32 = 1 << 23;
const GSTS_FOREIGN_OWNERSHIP: u32 =
    GSTS_TES | GSTS_AFLS | GSTS_WBFS | GSTS_QIES | GSTS_IRES | GSTS_CFIS;
const GSTS_PERSISTENT_COMMAND_STATE: u32 = GSTS_TES | GSTS_AFLS | GSTS_QIES | GSTS_IRES | GSTS_CFIS;

const CCMD_ICC: u64 = 1 << 63;
const CCMD_CIRG_GLOBAL: u64 = 1 << 61;
const CCMD_CAIG_SHIFT: u32 = 59;
const CCMD_CAIG_MASK: u64 = 0b11 << CCMD_CAIG_SHIFT;
const CCMD_CAIG_GLOBAL: u64 = 1 << CCMD_CAIG_SHIFT;

const IOTLB_IVT: u64 = 1 << 63;
const IOTLB_IIRG_GLOBAL: u64 = 1 << 60;
const IOTLB_IAIG_SHIFT: u32 = 57;
const IOTLB_IAIG_MASK: u64 = 0b11 << IOTLB_IAIG_SHIFT;
const IOTLB_IAIG_GLOBAL: u64 = 1 << IOTLB_IAIG_SHIFT;

const CAP_RWBF: u64 = 1 << 4;
const CAP_SAGAW_MASK: u64 = 0x0f << 8;
const CAP_MGAW_SHIFT: u32 = 16;
const CAP_MGAW_MASK: u64 = 0x3f << CAP_MGAW_SHIFT;
const ECAP_IRO_SHIFT: u32 = 8;
const ECAP_IRO_MASK: u64 = 0x3ff << ECAP_IRO_SHIFT;
const FSTS_ITE: u32 = 1 << 6;

#[repr(C, align(16))]
pub struct RootEntry {
    lower: AtomicU64,
    upper: AtomicU64,
}

impl RootEntry {
    pub const fn new() -> Self {
        Self {
            lower: AtomicU64::new(0),
            upper: AtomicU64::new(0),
        }
    }

    pub fn install_context_table(&self, physical_address: u64) -> Result<(), TableError> {
        validate_page(physical_address)?;
        self.upper.store(0, Ordering::Relaxed);
        self.lower.store(
            (physical_address & PHYSICAL_PAGE_MASK) | PRESENT,
            Ordering::Release,
        );
        Ok(())
    }

    pub fn clear(&self) {
        self.lower.store(0, Ordering::Release);
        self.upper.store(0, Ordering::Relaxed);
    }

    pub fn raw(&self) -> (u64, u64) {
        (
            self.lower.load(Ordering::Acquire),
            self.upper.load(Ordering::Acquire),
        )
    }
}

impl Default for RootEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(4096))]
pub struct RootEntryTable {
    entries: [RootEntry; 256],
}

impl RootEntryTable {
    pub const fn new() -> Self {
        Self {
            entries: [const { RootEntry::new() }; 256],
        }
    }

    pub fn entry(&self, bus: u8) -> &RootEntry {
        &self.entries[bus as usize]
    }
}

impl Default for RootEntryTable {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(16))]
pub struct ContextEntry {
    lower: AtomicU64,
    upper: AtomicU64,
}

impl ContextEntry {
    pub const fn new() -> Self {
        Self {
            lower: AtomicU64::new(0),
            upper: AtomicU64::new(0),
        }
    }

    pub fn install_second_level_translation(
        &self,
        domain_id: u16,
        address_width: u8,
        page_table_root: u64,
    ) -> Result<(), TableError> {
        validate_page(page_table_root)?;
        if address_width > 3 {
            return Err(TableError::InvalidAddressWidth);
        }
        let upper = (u64::from(domain_id) << 8) | u64::from(address_width);
        self.upper.store(upper, Ordering::Relaxed);
        self.lower.store(
            (page_table_root & PHYSICAL_PAGE_MASK) | PRESENT,
            Ordering::Release,
        );
        Ok(())
    }

    pub fn clear(&self) {
        self.lower.store(0, Ordering::Release);
        self.upper.store(0, Ordering::Relaxed);
    }

    pub fn raw(&self) -> (u64, u64) {
        (
            self.lower.load(Ordering::Acquire),
            self.upper.load(Ordering::Acquire),
        )
    }
}

impl Default for ContextEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(4096))]
pub struct ContextEntryTable {
    entries: [ContextEntry; 256],
}

impl ContextEntryTable {
    pub const fn new() -> Self {
        Self {
            entries: [const { ContextEntry::new() }; 256],
        }
    }

    pub fn entry(&self, device: PciAddress) -> &ContextEntry {
        &self.entries[device.slot as usize * 8 + device.function as usize]
    }
}

impl Default for ContextEntryTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdRegisterError {
    Access,
    Unstable,
}

/// Register access required by the remapping-engine state machine.
///
/// The production implementation below uses Boulder's owned typed MMIO
/// window. Tests implement this trait with a deterministic register model.
pub trait VtdRegisterBackend {
    fn read_u32(&self, offset: usize) -> Result<u32, VtdRegisterError>;
    fn write_u32(&self, offset: usize, value: u32) -> Result<(), VtdRegisterError>;
    fn read_u64(&self, offset: usize) -> Result<u64, VtdRegisterError>;
    fn write_u64(&self, offset: usize, value: u64) -> Result<(), VtdRegisterError>;
    fn relax(&self);
}

impl<Registers: VtdRegisterBackend + ?Sized> VtdRegisterBackend for &Registers {
    fn read_u32(&self, offset: usize) -> Result<u32, VtdRegisterError> {
        (**self).read_u32(offset)
    }

    fn write_u32(&self, offset: usize, value: u32) -> Result<(), VtdRegisterError> {
        (**self).write_u32(offset, value)
    }

    fn read_u64(&self, offset: usize) -> Result<u64, VtdRegisterError> {
        (**self).read_u64(offset)
    }

    fn write_u64(&self, offset: usize, value: u64) -> Result<(), VtdRegisterError> {
        (**self).write_u64(offset, value)
    }

    fn relax(&self) {
        (**self).relax();
    }
}

pub struct VtdMmioRegisters {
    unit: DmarRemappingUnit,
    window: TypedMmioWindow,
}

impl VtdMmioRegisters {
    pub fn map(
        unit: DmarRemappingUnit,
        authority: &Capability<'_, DeviceMemoryRight>,
    ) -> Result<Self, VtdEngineFault> {
        validate_unit(unit)?;
        let window = TypedMmioWindow::map(unit.register_base, REGISTER_WINDOW_BYTES, authority)
            .map_err(map_mmio_map_fault)?;
        Ok(Self { unit, window })
    }

    pub const fn unit(&self) -> DmarRemappingUnit {
        self.unit
    }

    pub fn into_engine(self) -> Result<VtdRemappingEngine<Self>, VtdProbeFailure<Self>> {
        VtdRemappingEngine::probe(self.unit, self)
    }

    pub fn close(
        self,
        authority: &Capability<'_, DeviceMemoryRight>,
    ) -> Result<(), VtdEngineFault> {
        let status = self.window.close(authority);
        if status == STATUS_OK {
            Ok(())
        } else {
            Err(VtdEngineFault::MmioMap(status))
        }
    }
}

impl VtdRegisterBackend for VtdMmioRegisters {
    fn read_u32(&self, offset: usize) -> Result<u32, VtdRegisterError> {
        self.window
            .read_u32(offset)
            .map_err(|_| VtdRegisterError::Access)
    }

    fn write_u32(&self, offset: usize, value: u32) -> Result<(), VtdRegisterError> {
        self.window
            .write_u32(offset, value)
            .map_err(|_| VtdRegisterError::Access)
    }

    fn read_u64(&self, offset: usize) -> Result<u64, VtdRegisterError> {
        if offset & 7 != 0 {
            return Err(VtdRegisterError::Access);
        }
        for _ in 0..3 {
            let high_before = self.read_u32(offset + 4)?;
            let low = self.read_u32(offset)?;
            let high_after = self.read_u32(offset + 4)?;
            if high_before == high_after {
                return Ok(u64::from(low) | (u64::from(high_after) << 32));
            }
        }
        Err(VtdRegisterError::Unstable)
    }

    fn write_u64(&self, offset: usize, value: u64) -> Result<(), VtdRegisterError> {
        if offset & 7 != 0 {
            return Err(VtdRegisterError::Access);
        }
        // VT-d command registers trigger when their upper dword is written.
        self.write_u32(offset, value as u32)?;
        self.write_u32(offset + 4, (value >> 32) as u32)
    }

    fn relax(&self) {
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VtdVersion {
    pub major: u8,
    pub minor: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VtdCapabilities {
    pub raw: u64,
    pub extended_raw: u64,
    pub supported_adjusted_guest_widths: u8,
    pub maximum_guest_address_width: u8,
    pub requires_write_buffer_flush: bool,
    pub iotlb_register_offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdEngineState {
    Disabled,
    Enabled,
    ForeignOwned,
    Poisoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdOperation {
    WriteBufferFlush,
    RootTablePointer,
    ContextInvalidation,
    IotlbInvalidation,
    EnableTranslation,
    DisableTranslation,
    Rollback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VtdEngineFault {
    InvalidUnit,
    InvalidRootTable,
    InvalidPollLimit,
    UnsupportedVersion,
    UnsupportedCapabilities,
    ForeignOwnership,
    InvalidState,
    Register(VtdRegisterError),
    MmioMap(Status),
    PendingCommand(VtdOperation),
    Timeout(VtdOperation),
    CompletionRejected(VtdOperation),
    HardwareInvalidationTimeout,
    RollbackFailed(VtdOperation),
}

impl From<VtdRegisterError> for VtdEngineFault {
    fn from(error: VtdRegisterError) -> Self {
        Self::Register(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VtdEnableReceipt {
    pub segment: u16,
    pub register_base: u64,
    pub root_table_address: u64,
    pub version: VtdVersion,
    pub capabilities: VtdCapabilities,
}

/// A failed probe returns the still-owned register transport so its MMIO
/// window can be closed or retried. Probe failure never silently abandons a
/// kernel mapping.
pub struct VtdProbeFailure<Registers> {
    fault: VtdEngineFault,
    registers: Registers,
}

impl<Registers> VtdProbeFailure<Registers> {
    pub const fn fault(&self) -> VtdEngineFault {
        self.fault
    }

    pub fn into_registers(self) -> Registers {
        self.registers
    }

    pub fn into_parts(self) -> (VtdEngineFault, Registers) {
        (self.fault, self.registers)
    }
}

impl<Registers> core::fmt::Debug for VtdProbeFailure<Registers> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("VtdProbeFailure")
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

#[must_use = "a live remapping engine owns hardware translation state and must be disabled explicitly"]
pub struct VtdRemappingEngine<Registers: VtdRegisterBackend> {
    segment: u16,
    register_base: u64,
    registers: Registers,
    version: VtdVersion,
    capabilities: VtdCapabilities,
    state: VtdEngineState,
    owned_root: Option<u64>,
}

impl<Registers: VtdRegisterBackend> VtdRemappingEngine<Registers> {
    pub fn probe(
        unit: DmarRemappingUnit,
        registers: Registers,
    ) -> Result<Self, VtdProbeFailure<Registers>> {
        if let Err(fault) = validate_unit(unit) {
            return Err(VtdProbeFailure { fault, registers });
        }
        Self::probe_at(unit.segment, unit.register_base, registers)
    }

    fn probe_at(
        segment: u16,
        register_base: u64,
        registers: Registers,
    ) -> Result<Self, VtdProbeFailure<Registers>> {
        let inspected = (|| -> Result<_, VtdEngineFault> {
            validate_register_base(register_base)?;
            let version_raw = registers.read_u32(VER)?;
            let version = VtdVersion {
                major: ((version_raw >> 4) & 0x0f) as u8,
                minor: (version_raw & 0x0f) as u8,
            };
            if version.major == 0 {
                return Err(VtdEngineFault::UnsupportedVersion);
            }

            let capability_raw = registers.read_u64(CAP)?;
            let extended_raw = registers.read_u64(ECAP)?;
            let supported_adjusted_guest_widths = ((capability_raw & CAP_SAGAW_MASK) >> 8) as u8;
            let maximum_guest_address_width =
                (((capability_raw & CAP_MGAW_MASK) >> CAP_MGAW_SHIFT) + 1) as u8;
            let iotlb_register_offset =
                (((extended_raw & ECAP_IRO_MASK) >> ECAP_IRO_SHIFT) as usize) * 16;
            if supported_adjusted_guest_widths == 0
                || !(30..=64).contains(&maximum_guest_address_width)
                || iotlb_register_offset == 0
                || iotlb_register_offset
                    .checked_add(16)
                    .is_none_or(|end| end > REGISTER_WINDOW_BYTES)
            {
                return Err(VtdEngineFault::UnsupportedCapabilities);
            }
            let capabilities = VtdCapabilities {
                raw: capability_raw,
                extended_raw,
                supported_adjusted_guest_widths,
                maximum_guest_address_width,
                requires_write_buffer_flush: capability_raw & CAP_RWBF != 0,
                iotlb_register_offset,
            };

            let status = registers.read_u32(GSTS)?;
            let context_command_pending = registers.read_u64(CCMD)? & CCMD_ICC != 0;
            let iotlb_command_pending =
                registers.read_u64(iotlb_register_offset + 8)? & IOTLB_IVT != 0;
            let state = if status & GSTS_FOREIGN_OWNERSHIP != 0
                || context_command_pending
                || iotlb_command_pending
            {
                VtdEngineState::ForeignOwned
            } else {
                VtdEngineState::Disabled
            };

            Ok((version, capabilities, state))
        })();

        let (version, capabilities, state) = match inspected {
            Ok(inspected) => inspected,
            Err(fault) => return Err(VtdProbeFailure { fault, registers }),
        };

        Ok(Self {
            segment,
            register_base,
            registers,
            version,
            capabilities,
            state,
            owned_root: None,
        })
    }

    pub const fn state(&self) -> VtdEngineState {
        self.state
    }

    pub const fn version(&self) -> VtdVersion {
        self.version
    }

    pub const fn capabilities(&self) -> VtdCapabilities {
        self.capabilities
    }

    pub const fn registers(&self) -> &Registers {
        &self.registers
    }

    /// Returns the owned register transport only when this state machine has
    /// no live translation authority. A poisoned engine is deliberately not
    /// destructured because hardware ownership is then uncertain.
    pub fn into_registers(self) -> Result<Registers, Self> {
        if matches!(
            self.state,
            VtdEngineState::Disabled | VtdEngineState::ForeignOwned
        ) && self.owned_root.is_none()
        {
            Ok(self.registers)
        } else {
            Err(self)
        }
    }

    pub fn enable(
        &mut self,
        root_table_address: u64,
        poll_limit: u32,
    ) -> Result<VtdEnableReceipt, VtdEngineFault> {
        if poll_limit == 0 {
            return Err(VtdEngineFault::InvalidPollLimit);
        }
        validate_page(root_table_address).map_err(|_| VtdEngineFault::InvalidRootTable)?;
        match self.state {
            VtdEngineState::Disabled => {}
            VtdEngineState::ForeignOwned => return Err(VtdEngineFault::ForeignOwnership),
            VtdEngineState::Enabled | VtdEngineState::Poisoned => {
                return Err(VtdEngineFault::InvalidState);
            }
        }

        let status = self.registers.read_u32(GSTS)?;
        if status & GSTS_FOREIGN_OWNERSHIP != 0 {
            self.state = VtdEngineState::ForeignOwned;
            return Err(VtdEngineFault::ForeignOwnership);
        }
        if self.registers.read_u64(CCMD)? & CCMD_ICC != 0
            || self
                .registers
                .read_u64(self.capabilities.iotlb_register_offset + 8)?
                & IOTLB_IVT
                != 0
        {
            self.state = VtdEngineState::ForeignOwned;
            return Err(VtdEngineFault::ForeignOwnership);
        }
        if self.registers.read_u32(FSTS)? & FSTS_ITE != 0 {
            return Err(VtdEngineFault::HardwareInvalidationTimeout);
        }
        let previous_root = self.registers.read_u64(RTADDR)?;

        let result = self.configure_root(root_table_address, poll_limit);
        if let Err(error) = result {
            if self.rollback_enable(previous_root, poll_limit).is_err() {
                self.state = VtdEngineState::Poisoned;
                return Err(VtdEngineFault::RollbackFailed(error.operation()));
            }
            self.state = VtdEngineState::Disabled;
            return Err(error);
        }

        self.state = VtdEngineState::Enabled;
        self.owned_root = Some(root_table_address);
        Ok(VtdEnableReceipt {
            segment: self.segment,
            register_base: self.register_base,
            root_table_address,
            version: self.version,
            capabilities: self.capabilities,
        })
    }

    pub fn disable(&mut self, poll_limit: u32) -> Result<(), VtdEngineFault> {
        if poll_limit == 0 {
            return Err(VtdEngineFault::InvalidPollLimit);
        }
        if self.state != VtdEngineState::Enabled || self.owned_root.is_none() {
            return Err(match self.state {
                VtdEngineState::ForeignOwned => VtdEngineFault::ForeignOwnership,
                _ => VtdEngineFault::InvalidState,
            });
        }

        let owned_root = self.owned_root.ok_or(VtdEngineFault::InvalidState)?;
        if let Err(error) = self.disable_and_purge(poll_limit) {
            if self.rollback_disable(owned_root, poll_limit).is_err() {
                self.state = VtdEngineState::Poisoned;
                return Err(VtdEngineFault::RollbackFailed(error.operation()));
            }
            self.state = VtdEngineState::Enabled;
            return Err(error);
        }
        self.state = VtdEngineState::Disabled;
        self.owned_root = None;
        Ok(())
    }

    /// Commits page-table or context-table mutations to the active unit.
    /// Any failed flush/invalidation poisons the engine because hardware may
    /// have observed only a prefix of the mutation; callers must quarantine
    /// the affected tables and DMA addresses instead of rolling them forward.
    pub fn commit_table_update(&mut self, poll_limit: u32) -> Result<(), VtdEngineFault> {
        if poll_limit == 0 {
            return Err(VtdEngineFault::InvalidPollLimit);
        }
        if self.state != VtdEngineState::Enabled || self.owned_root.is_none() {
            return Err(match self.state {
                VtdEngineState::ForeignOwned => VtdEngineFault::ForeignOwnership,
                _ => VtdEngineFault::InvalidState,
            });
        }

        let result = self
            .flush_write_buffer(poll_limit)
            .and_then(|_| self.invalidate_context(poll_limit))
            .and_then(|_| self.invalidate_iotlb(poll_limit));
        if result.is_err() {
            self.state = VtdEngineState::Poisoned;
        }
        result
    }

    fn configure_root(
        &self,
        root_table_address: u64,
        poll_limit: u32,
    ) -> Result<(), VtdEngineFault> {
        self.install_root_pointer(root_table_address, poll_limit)?;
        self.flush_write_buffer(poll_limit)?;
        self.invalidate_context(poll_limit)?;
        self.invalidate_iotlb(poll_limit)?;
        self.issue_global_command(Some(true), 0)?;
        self.wait_u32(
            GSTS,
            GSTS_TES,
            true,
            poll_limit,
            VtdOperation::EnableTranslation,
        )?;
        Ok(())
    }

    fn rollback_enable(&self, previous_root: u64, poll_limit: u32) -> Result<(), VtdEngineFault> {
        self.issue_global_command(Some(false), 0)?;
        self.wait_u32(GSTS, GSTS_TES, false, poll_limit, VtdOperation::Rollback)?;
        self.install_root_pointer(previous_root, poll_limit)?;
        self.flush_write_buffer(poll_limit)?;
        self.invalidate_context(poll_limit)?;
        self.invalidate_iotlb(poll_limit)?;
        Ok(())
    }

    fn install_root_pointer(
        &self,
        root_table_address: u64,
        poll_limit: u32,
    ) -> Result<(), VtdEngineFault> {
        self.registers.write_u64(RTADDR, root_table_address)?;
        if self.registers.read_u64(RTADDR)? != root_table_address {
            return Err(VtdEngineFault::CompletionRejected(
                VtdOperation::RootTablePointer,
            ));
        }
        self.issue_global_command(None, GCMD_SRTP)?;
        self.wait_u32(
            GSTS,
            GSTS_RTPS,
            true,
            poll_limit,
            VtdOperation::RootTablePointer,
        )?;
        Ok(())
    }

    fn disable_and_purge(&self, poll_limit: u32) -> Result<(), VtdEngineFault> {
        self.issue_global_command(Some(false), 0)?;
        self.wait_u32(
            GSTS,
            GSTS_TES,
            false,
            poll_limit,
            VtdOperation::DisableTranslation,
        )?;
        self.invalidate_context(poll_limit)?;
        self.invalidate_iotlb(poll_limit)
    }

    fn rollback_disable(&self, owned_root: u64, poll_limit: u32) -> Result<(), VtdEngineFault> {
        if self.registers.read_u32(GSTS)? & GSTS_TES != 0 {
            return Ok(());
        }
        self.configure_root(owned_root, poll_limit)
    }

    fn flush_write_buffer(&self, poll_limit: u32) -> Result<(), VtdEngineFault> {
        if !self.capabilities.requires_write_buffer_flush {
            return Ok(());
        }
        self.issue_global_command(None, GCMD_WBF)?;
        self.wait_u32(
            GSTS,
            GSTS_WBFS,
            false,
            poll_limit,
            VtdOperation::WriteBufferFlush,
        )
        .map(|_| ())
    }

    /// Composes GCMD from the persistent state mirrored in GSTS, clears every
    /// one-shot status bit, then applies exactly one requested transition.
    /// Intel requires this serialization because writing WBF/SRTP with a bare
    /// command value can unintentionally toggle TE or an auxiliary engine.
    fn issue_global_command(
        &self,
        translation_enabled: Option<bool>,
        one_shot: u32,
    ) -> Result<(), VtdEngineFault> {
        let mut command = self.registers.read_u32(GSTS)? & GSTS_PERSISTENT_COMMAND_STATE;
        if let Some(enabled) = translation_enabled {
            if enabled {
                command |= GCMD_TE;
            } else {
                command &= !GCMD_TE;
            }
        }
        self.registers.write_u32(GCMD, command | one_shot)?;
        Ok(())
    }

    fn invalidate_context(&self, poll_limit: u32) -> Result<(), VtdEngineFault> {
        if self.registers.read_u64(CCMD)? & CCMD_ICC != 0 {
            return Err(VtdEngineFault::PendingCommand(
                VtdOperation::ContextInvalidation,
            ));
        }
        self.registers
            .write_u64(CCMD, CCMD_ICC | CCMD_CIRG_GLOBAL)?;
        let completed = self.wait_u64(
            CCMD,
            CCMD_ICC,
            false,
            poll_limit,
            VtdOperation::ContextInvalidation,
        )?;
        if completed & CCMD_CAIG_MASK != CCMD_CAIG_GLOBAL {
            return Err(VtdEngineFault::CompletionRejected(
                VtdOperation::ContextInvalidation,
            ));
        }
        self.check_invalidation_status()
    }

    fn invalidate_iotlb(&self, poll_limit: u32) -> Result<(), VtdEngineFault> {
        let offset = self.capabilities.iotlb_register_offset + 8;
        if self.registers.read_u64(offset)? & IOTLB_IVT != 0 {
            return Err(VtdEngineFault::PendingCommand(
                VtdOperation::IotlbInvalidation,
            ));
        }
        self.registers
            .write_u64(offset, IOTLB_IVT | IOTLB_IIRG_GLOBAL)?;
        let completed = self.wait_u64(
            offset,
            IOTLB_IVT,
            false,
            poll_limit,
            VtdOperation::IotlbInvalidation,
        )?;
        if completed & IOTLB_IAIG_MASK != IOTLB_IAIG_GLOBAL {
            return Err(VtdEngineFault::CompletionRejected(
                VtdOperation::IotlbInvalidation,
            ));
        }
        self.check_invalidation_status()
    }

    fn check_invalidation_status(&self) -> Result<(), VtdEngineFault> {
        if self.registers.read_u32(FSTS)? & FSTS_ITE != 0 {
            Err(VtdEngineFault::HardwareInvalidationTimeout)
        } else {
            Ok(())
        }
    }

    fn wait_u32(
        &self,
        offset: usize,
        mask: u32,
        set: bool,
        poll_limit: u32,
        operation: VtdOperation,
    ) -> Result<u32, VtdEngineFault> {
        for _ in 0..poll_limit {
            let value = self.registers.read_u32(offset)?;
            if (value & mask != 0) == set {
                return Ok(value);
            }
            self.registers.relax();
        }
        Err(VtdEngineFault::Timeout(operation))
    }

    fn wait_u64(
        &self,
        offset: usize,
        mask: u64,
        set: bool,
        poll_limit: u32,
        operation: VtdOperation,
    ) -> Result<u64, VtdEngineFault> {
        for _ in 0..poll_limit {
            let value = self.registers.read_u64(offset)?;
            if (value & mask != 0) == set {
                return Ok(value);
            }
            self.registers.relax();
        }
        Err(VtdEngineFault::Timeout(operation))
    }
}

impl VtdEngineFault {
    const fn operation(self) -> VtdOperation {
        match self {
            Self::PendingCommand(operation)
            | Self::Timeout(operation)
            | Self::CompletionRejected(operation)
            | Self::RollbackFailed(operation) => operation,
            _ => VtdOperation::Rollback,
        }
    }
}

fn validate_unit(unit: DmarRemappingUnit) -> Result<(), VtdEngineFault> {
    validate_register_base(unit.register_base)
}

fn validate_register_base(register_base: u64) -> Result<(), VtdEngineFault> {
    if register_base == 0
        || register_base % PAGE_SIZE != 0
        || register_base & !PHYSICAL_PAGE_MASK != 0
        || register_base
            .checked_add(REGISTER_WINDOW_BYTES as u64 - 1)
            .is_none_or(|last| last & !PHYSICAL_ADDRESS_MASK != 0)
    {
        Err(VtdEngineFault::InvalidUnit)
    } else {
        Ok(())
    }
}

fn map_mmio_map_fault(error: MmioAccessError) -> VtdEngineFault {
    match error {
        MmioAccessError::Map(status) => VtdEngineFault::MmioMap(status),
        MmioAccessError::OutOfBounds | MmioAccessError::Misaligned => {
            VtdEngineFault::Register(VtdRegisterError::Access)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableError {
    InvalidPageAddress,
    InvalidAddressWidth,
}

fn validate_page(physical_address: u64) -> Result<(), TableError> {
    if physical_address == 0
        || physical_address % PAGE_SIZE != 0
        || physical_address & !PHYSICAL_PAGE_MASK != 0
    {
        Err(TableError::InvalidPageAddress)
    } else {
        Ok(())
    }
}

const _: () = assert!(core::mem::size_of::<RootEntry>() == 16);
const _: () = assert!(core::mem::size_of::<RootEntryTable>() == 4096);
const _: () = assert!(core::mem::size_of::<ContextEntry>() == 16);
const _: () = assert!(core::mem::size_of::<ContextEntryTable>() == 4096);

#[cfg(test)]
mod tests {
    use crate::sync::SpinLock;

    use super::*;

    #[test]
    fn builds_full_width_root_and_context_entries() {
        let roots = RootEntryTable::new();
        let contexts = ContextEntryTable::new();
        let device = PciAddress::new(2, 3, 1).unwrap();
        contexts
            .entry(device)
            .install_second_level_translation(7, 2, 0x3000)
            .unwrap();
        roots.entry(2).install_context_table(0x2000).unwrap();
        assert_eq!(roots.entry(2).raw(), (0x2001, 0));
        assert_eq!(contexts.entry(device).raw(), (0x3001, 7 << 8 | 2));
    }

    #[test]
    fn rejects_unaligned_table_addresses() {
        let entry = RootEntry::new();
        assert_eq!(
            entry.install_context_table(0x1234),
            Err(TableError::InvalidPageAddress)
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RegisterFault {
        None,
        RejectIotlbOnce,
        StallIotlb,
        StallDisableOnce,
    }

    struct RegisterState {
        version: u32,
        capability: u64,
        extended_capability: u64,
        global_status: u32,
        root_address: u64,
        loaded_root_address: u64,
        root_pointer_loads: usize,
        context_command: u64,
        iotlb_command: u64,
        fault_status: u32,
        fault: RegisterFault,
        context_invalidations: usize,
        iotlb_invalidations: usize,
    }

    impl RegisterState {
        const fn new(fault: RegisterFault) -> Self {
            Self {
                version: 0x10,
                capability: (47 << CAP_MGAW_SHIFT) | (1 << 8) | CAP_RWBF,
                extended_capability: 0x10 << ECAP_IRO_SHIFT,
                global_status: 0,
                root_address: 0x9000,
                loaded_root_address: 0x9000,
                root_pointer_loads: 0,
                context_command: 0,
                iotlb_command: 0,
                fault_status: 0,
                fault,
                context_invalidations: 0,
                iotlb_invalidations: 0,
            }
        }
    }

    struct TestRegisters {
        state: SpinLock<RegisterState>,
    }

    impl TestRegisters {
        const fn new(fault: RegisterFault) -> Self {
            Self {
                state: SpinLock::new(RegisterState::new(fault)),
            }
        }
    }

    impl VtdRegisterBackend for TestRegisters {
        fn read_u32(&self, offset: usize) -> Result<u32, VtdRegisterError> {
            let state = self.state.lock();
            match offset {
                VER => Ok(state.version),
                GSTS => Ok(state.global_status),
                FSTS => Ok(state.fault_status),
                _ => Err(VtdRegisterError::Access),
            }
        }

        fn write_u32(&self, offset: usize, value: u32) -> Result<(), VtdRegisterError> {
            if offset != GCMD {
                return Err(VtdRegisterError::Access);
            }
            let mut state = self.state.lock();
            if value & GCMD_SRTP != 0 {
                state.loaded_root_address = state.root_address;
                state.root_pointer_loads += 1;
                state.global_status |= GSTS_RTPS;
            }
            if value & GCMD_WBF != 0 {
                state.global_status &= !GSTS_WBFS;
            }
            if value & GCMD_TE != 0 {
                state.global_status |= GSTS_TES;
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
                CAP => Ok(state.capability),
                ECAP => Ok(state.extended_capability),
                RTADDR => Ok(state.root_address),
                CCMD => Ok(state.context_command),
                0x108 => Ok(state.iotlb_command),
                _ => Err(VtdRegisterError::Access),
            }
        }

        fn write_u64(&self, offset: usize, value: u64) -> Result<(), VtdRegisterError> {
            let mut state = self.state.lock();
            match offset {
                RTADDR => state.root_address = value,
                CCMD if value & CCMD_ICC != 0 => {
                    state.context_invalidations += 1;
                    state.context_command = CCMD_CAIG_GLOBAL;
                }
                0x108 if value & IOTLB_IVT != 0 => {
                    state.iotlb_invalidations += 1;
                    match state.fault {
                        RegisterFault::RejectIotlbOnce => {
                            state.fault = RegisterFault::None;
                            state.iotlb_command = 0;
                        }
                        RegisterFault::StallIotlb => {
                            state.iotlb_command = value;
                        }
                        _ => state.iotlb_command = IOTLB_IAIG_GLOBAL,
                    }
                }
                _ => return Err(VtdRegisterError::Access),
            }
            Ok(())
        }

        fn relax(&self) {}
    }

    fn engine(registers: &TestRegisters) -> VtdRemappingEngine<&TestRegisters> {
        VtdRemappingEngine::probe_at(0, 0xfed9_0000, registers).unwrap()
    }

    #[test]
    fn enables_and_disables_with_global_invalidation() {
        let registers = TestRegisters::new(RegisterFault::None);
        let mut engine = engine(&registers);
        let receipt = engine.enable(0x1000, 4).unwrap();
        assert_eq!(receipt.segment, 0);
        assert_eq!(receipt.register_base, 0xfed9_0000);
        assert_eq!(receipt.root_table_address, 0x1000);
        assert_eq!(engine.state(), VtdEngineState::Enabled);

        engine.disable(4).unwrap();
        assert_eq!(engine.state(), VtdEngineState::Disabled);
        let state = registers.state.lock();
        assert_eq!(state.global_status & GSTS_TES, 0);
        assert_eq!(state.root_address, 0x1000);
        assert_eq!(state.loaded_root_address, 0x1000);
        assert_eq!(state.root_pointer_loads, 1);
        assert_eq!(state.context_invalidations, 2);
        assert_eq!(state.iotlb_invalidations, 2);
    }

    #[test]
    fn rejected_iotlb_completion_rolls_back_the_prior_root() {
        let registers = TestRegisters::new(RegisterFault::RejectIotlbOnce);
        let mut engine = engine(&registers);

        assert_eq!(
            engine.enable(0x1000, 4),
            Err(VtdEngineFault::CompletionRejected(
                VtdOperation::IotlbInvalidation
            )),
        );
        assert_eq!(engine.state(), VtdEngineState::Disabled);
        let state = registers.state.lock();
        assert_eq!(state.global_status & GSTS_TES, 0);
        assert_eq!(state.root_address, 0x9000);
        assert_eq!(state.loaded_root_address, 0x9000);
        assert_eq!(state.root_pointer_loads, 2);
        assert_eq!(state.context_invalidations, 2);
        assert_eq!(state.iotlb_invalidations, 2);
    }

    #[test]
    fn failed_enable_rollback_poisons_the_engine() {
        let registers = TestRegisters::new(RegisterFault::StallIotlb);
        let mut engine = engine(&registers);

        assert_eq!(
            engine.enable(0x1000, 2),
            Err(VtdEngineFault::RollbackFailed(
                VtdOperation::IotlbInvalidation
            )),
        );
        assert_eq!(engine.state(), VtdEngineState::Poisoned);
        assert_eq!(engine.enable(0x2000, 2), Err(VtdEngineFault::InvalidState));
    }

    #[test]
    fn disable_timeout_rolls_back_to_the_owned_enabled_state() {
        let registers = TestRegisters::new(RegisterFault::None);
        let mut engine = engine(&registers);
        engine.enable(0x1000, 2).unwrap();
        registers.state.lock().fault = RegisterFault::StallDisableOnce;

        assert_eq!(
            engine.disable(2),
            Err(VtdEngineFault::Timeout(VtdOperation::DisableTranslation)),
        );
        assert_eq!(engine.state(), VtdEngineState::Enabled);
        assert_ne!(registers.state.lock().global_status & GSTS_TES, 0);
    }

    #[test]
    fn table_update_failure_poisons_authority_instead_of_claiming_commit() {
        let registers = TestRegisters::new(RegisterFault::None);
        let mut engine = engine(&registers);
        engine.enable(0x1000, 2).unwrap();

        registers.state.lock().fault = RegisterFault::RejectIotlbOnce;
        assert_eq!(
            engine.commit_table_update(2),
            Err(VtdEngineFault::CompletionRejected(
                VtdOperation::IotlbInvalidation
            ))
        );
        assert_eq!(engine.state(), VtdEngineState::Poisoned);
        assert!(engine.into_registers().is_err());
    }

    #[test]
    fn live_write_buffer_flush_preserves_translation_enable() {
        let registers = TestRegisters::new(RegisterFault::None);
        let mut engine = engine(&registers);
        engine.enable(0x1000, 2).unwrap();
        let invalidations_before = registers.state.lock().iotlb_invalidations;

        engine.commit_table_update(2).unwrap();

        let state = registers.state.lock();
        assert_ne!(state.global_status & GSTS_TES, 0);
        assert_eq!(state.iotlb_invalidations, invalidations_before + 1);
    }

    #[test]
    fn refuses_a_remapping_engine_already_owned_by_firmware() {
        let registers = TestRegisters::new(RegisterFault::None);
        registers.state.lock().global_status = GSTS_QIES;
        let mut engine = engine(&registers);
        assert_eq!(engine.state(), VtdEngineState::ForeignOwned);
        assert_eq!(
            engine.enable(0x1000, 2),
            Err(VtdEngineFault::ForeignOwnership)
        );
        assert_eq!(registers.state.lock().root_address, 0x9000);
    }

    #[test]
    fn refuses_an_engine_with_a_command_already_in_flight() {
        let registers = TestRegisters::new(RegisterFault::None);
        registers.state.lock().context_command = CCMD_ICC;
        let mut engine = engine(&registers);
        assert_eq!(engine.state(), VtdEngineState::ForeignOwned);
        assert_eq!(
            engine.enable(0x1000, 2),
            Err(VtdEngineFault::ForeignOwnership)
        );
        let state = registers.state.lock();
        assert_eq!(state.root_address, 0x9000);
        assert_eq!(state.root_pointer_loads, 0);
    }

    #[test]
    fn probe_failure_and_disabled_engine_return_owned_register_transport() {
        let invalid = TestRegisters::new(RegisterFault::None);
        invalid.state.lock().version = 0;
        let failure = match VtdRemappingEngine::probe_at(0, 0xfed9_0000, invalid) {
            Ok(_) => panic!("invalid version was accepted"),
            Err(failure) => failure,
        };
        assert_eq!(failure.fault(), VtdEngineFault::UnsupportedVersion);
        let invalid = failure.into_registers();
        assert_eq!(invalid.state.lock().version, 0);

        let registers = TestRegisters::new(RegisterFault::None);
        let engine = VtdRemappingEngine::probe_at(0, 0xfed9_0000, registers).unwrap();
        let registers = match engine.into_registers() {
            Ok(registers) => registers,
            Err(_) => panic!("disabled engine retained transport"),
        };
        assert_eq!(registers.state.lock().root_pointer_loads, 0);
    }
}
