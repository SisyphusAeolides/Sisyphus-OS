//! Identity-DMA storage for the pre-remapping xHCI bootstrap path.
//!
//! Identity DMA is accepted only through an explicit platform proof bound to
//! one PCI requester.  The proof cannot be inferred from a missing DMAR table:
//! its caller must establish coherent x86 aliases and verify that translation
//! is inactive for the exact requester by platform-specific means.

use core::marker::PhantomData;
use core::sync::atomic::{Ordering, compiler_fence, fence};

use abyss::frame::FrameAllocatorError;
use abyss::paging::{PAGE_SIZE, PhysicalAddress};

use crate::capability::{Capability, DmaControl};
use crate::hw::pci::PciAddress;
use crate::memory::frame_pool::PhysicalFramePool;

pub const XHCI_BASE_REGION_COUNT: usize = 4;
const FIXED_REGION_CAPACITY: usize = XHCI_BASE_REGION_COUNT + 2;
const MAXIMUM_SCRATCHPAD_BUFFERS: u16 = 1_023;
const IDENTITY_ROOT_DOMAIN: u64 = 0x5848_4349_4944_4d41;
const ARENA_ROOT_DOMAIN: u64 = 0x5848_4349_4152_454e;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityDmaObservation {
    pub x86_cache_coherent: bool,
    pub requester_remapping_active: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityDmaError {
    InvalidGeneration,
    InvalidEvidenceRoot,
    UnalignedPhysicalAperture,
    EmptyPhysicalAperture,
    PhysicalApertureOverflow,
    CpuApertureOverflow,
    DeviceLimitBelowAperture,
    X86CoherencyUnproven,
    ActiveRequesterRemapping,
}

/// Non-copy proof that one requester may issue physical addresses directly.
pub struct IdentityDmaWindow {
    device: PciAddress,
    physical_start: u64,
    physical_end: u64,
    direct_map_base: usize,
    cpu_start: usize,
    cpu_end: usize,
    device_address_limit: u64,
    generation: u32,
    evidence_root: u64,
    proof_root: u64,
    x86_cache_coherent: bool,
    requester_remapping_active: bool,
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl IdentityDmaWindow {
    /// Establishes an identity-DMA platform fact for one exact requester.
    ///
    /// # Safety
    ///
    /// The caller must establish all of the following independently of ACPI
    /// table presence: `device` has no active DMA translation; every physical
    /// byte in the aperture has exactly one writable CPU alias at
    /// `direct_map_base + physical`; and the platform is cache coherent for
    /// CPU/device DMA with the fences used by this module.  The observation
    /// must describe current hardware state, not merely missing firmware data.
    pub unsafe fn establish(
        device: PciAddress,
        physical_start: u64,
        physical_bytes: u64,
        direct_map_base: usize,
        device_address_limit: u64,
        generation: u32,
        evidence_root: u64,
        observation: IdentityDmaObservation,
    ) -> Result<Self, IdentityDmaError> {
        if generation == 0 {
            return Err(IdentityDmaError::InvalidGeneration);
        }
        if evidence_root == 0 {
            return Err(IdentityDmaError::InvalidEvidenceRoot);
        }
        if physical_start % PAGE_SIZE as u64 != 0 || physical_bytes % PAGE_SIZE as u64 != 0 {
            return Err(IdentityDmaError::UnalignedPhysicalAperture);
        }
        if physical_bytes == 0 {
            return Err(IdentityDmaError::EmptyPhysicalAperture);
        }
        let physical_end = physical_start
            .checked_add(physical_bytes)
            .ok_or(IdentityDmaError::PhysicalApertureOverflow)?;
        if physical_end - 1 > device_address_limit {
            return Err(IdentityDmaError::DeviceLimitBelowAperture);
        }
        let physical_start_usize =
            usize::try_from(physical_start).map_err(|_| IdentityDmaError::CpuApertureOverflow)?;
        let physical_end_usize =
            usize::try_from(physical_end).map_err(|_| IdentityDmaError::CpuApertureOverflow)?;
        let cpu_start = direct_map_base
            .checked_add(physical_start_usize)
            .ok_or(IdentityDmaError::CpuApertureOverflow)?;
        let cpu_end = direct_map_base
            .checked_add(physical_end_usize)
            .ok_or(IdentityDmaError::CpuApertureOverflow)?;
        if cpu_start == 0 || cpu_start >= cpu_end {
            return Err(IdentityDmaError::CpuApertureOverflow);
        }
        if !observation.x86_cache_coherent {
            return Err(IdentityDmaError::X86CoherencyUnproven);
        }
        if observation.requester_remapping_active {
            return Err(IdentityDmaError::ActiveRequesterRemapping);
        }
        let mut proof_root = mix(evidence_root ^ IDENTITY_ROOT_DOMAIN, u64::from(device.bus));
        proof_root = mix(proof_root, u64::from(device.slot));
        proof_root = mix(proof_root, u64::from(device.function));
        proof_root = mix(proof_root, physical_start);
        proof_root = mix(proof_root, physical_end);
        proof_root = mix(proof_root, device_address_limit);
        proof_root = mix(proof_root, generation.into());
        proof_root = canonical_root(proof_root, IDENTITY_ROOT_DOMAIN);
        Ok(Self {
            device,
            physical_start,
            physical_end,
            direct_map_base,
            cpu_start,
            cpu_end,
            device_address_limit,
            generation,
            evidence_root,
            proof_root,
            x86_cache_coherent: true,
            requester_remapping_active: false,
            _not_send_or_sync: PhantomData,
        })
    }

    pub const fn device(&self) -> PciAddress {
        self.device
    }

    pub const fn physical_bounds(&self) -> (u64, u64) {
        (self.physical_start, self.physical_end)
    }

    pub const fn cpu_bounds(&self) -> (usize, usize) {
        (self.cpu_start, self.cpu_end)
    }

    pub const fn device_address_limit(&self) -> u64 {
        self.device_address_limit
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn evidence_root(&self) -> u64 {
        self.evidence_root
    }

    pub const fn proof_root(&self) -> u64 {
        self.proof_root
    }

    pub const fn x86_cache_coherent(&self) -> bool {
        self.x86_cache_coherent
    }

    pub const fn requester_remapping_inactive(&self) -> bool {
        !self.requester_remapping_active
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciDmaPurpose {
    Dcbaa,
    CommandRing,
    EventRing,
    EventRingSegmentTable,
    ScratchpadPointerArray,
    ScratchpadBuffers,
}

impl XhciDmaPurpose {
    const fn code(self) -> u64 {
        match self {
            Self::Dcbaa => 1,
            Self::CommandRing => 2,
            Self::EventRing => 3,
            Self::EventRingSegmentTable => 4,
            Self::ScratchpadPointerArray => 5,
            Self::ScratchpadBuffers => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciDmaRegionPhase {
    Empty,
    FrameOwned,
    Zeroed,
    Ready,
    Releasing,
    ReleaseDebt,
    Released,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciDmaRegionRecord {
    pub phase: XhciDmaRegionPhase,
    pub generation: u32,
    pub purpose: XhciDmaPurpose,
    pub device: PciAddress,
    pub physical_start: u64,
    pub physical_end: u64,
    pub device_address_start: u64,
    pub device_address_end: u64,
    pub cpu_start: usize,
    pub cpu_end: usize,
    pub page_count: usize,
    pub region_root: u64,
}

impl XhciDmaRegionRecord {
    const EMPTY: Self = Self {
        phase: XhciDmaRegionPhase::Empty,
        generation: 0,
        purpose: XhciDmaPurpose::Dcbaa,
        device: PciAddress {
            bus: 0,
            slot: 0,
            function: 0,
        },
        physical_start: 0,
        physical_end: 0,
        device_address_start: 0,
        device_address_end: 0,
        cpu_start: 0,
        cpu_end: 0,
        page_count: 0,
        region_root: 0,
    };

    pub const fn byte_length(self) -> usize {
        self.cpu_end - self.cpu_start
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciDmaRegionToken {
    generation: u32,
    purpose: XhciDmaPurpose,
}

impl XhciDmaRegionToken {
    pub const fn generation(self) -> u32 {
        self.generation
    }

    pub const fn purpose(self) -> XhciDmaPurpose {
        self.purpose
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciDmaError {
    InvalidSecret,
    ScratchpadCountUnsupported(u16),
    FrameUnavailable(XhciDmaPurpose),
    PhysicalRangeOverflow(XhciDmaPurpose),
    OutsideIdentityAperture(XhciDmaPurpose),
    DeviceAddressLimit(XhciDmaPurpose),
    CpuAliasOverflow(XhciDmaPurpose),
    RegionCapacity,
    MissingRegion(XhciDmaPurpose),
    StaleRegionToken,
    InactiveRegion(XhciDmaPurpose),
    EmptyTransfer,
    TransferOutsideRegion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciDmaReleaseFault {
    QuiescenceMismatch,
    FrameAllocator {
        purpose: XhciDmaPurpose,
        source: FrameAllocatorError,
    },
    AlreadyReleased,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciDmaReleaseReceipt {
    pub device: PciAddress,
    pub generation: u32,
    pub released_pages: usize,
    pub release_root: u64,
}

/// Proof that the exact controller generation can no longer access its arena.
pub struct XhciDmaQuiescence {
    device: PciAddress,
    generation: u32,
    evidence_root: u64,
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl XhciDmaQuiescence {
    /// # Safety
    ///
    /// The caller must have stopped the exact controller, disabled bus
    /// mastering, and drained every DMA transaction for this generation.
    pub const unsafe fn establish(
        device: PciAddress,
        generation: u32,
        evidence_root: u64,
    ) -> Option<Self> {
        if generation == 0 || evidence_root == 0 {
            None
        } else {
            Some(Self {
                device,
                generation,
                evidence_root,
                _not_send_or_sync: PhantomData,
            })
        }
    }
}

#[must_use = "the arena owns physical frames until release returns a receipt"]
pub struct XhciDmaArena<'arena, 'storage, 'authority> {
    frames: &'arena PhysicalFramePool<'storage>,
    _authority: &'arena Capability<'authority, DmaControl>,
    identity: IdentityDmaWindow,
    records: [XhciDmaRegionRecord; FIXED_REGION_CAPACITY],
    record_count: usize,
    scratchpad_count: u16,
    supports_64_bit_addresses: bool,
    effective_device_limit: u64,
    secret: u64,
    arena_root: u64,
    released_pages: usize,
    _not_send_or_sync: PhantomData<*mut ()>,
}

pub struct XhciDmaBuildFailure<'arena, 'storage, 'authority> {
    cause: XhciDmaError,
    debt: Option<XhciDmaReleaseDebt<'arena, 'storage, 'authority>>,
}

impl core::fmt::Debug for XhciDmaBuildFailure<'_, '_, '_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("XhciDmaBuildFailure")
            .field("cause", &self.cause)
            .field("has_release_debt", &self.debt.is_some())
            .finish()
    }
}

impl<'arena, 'storage, 'authority> XhciDmaBuildFailure<'arena, 'storage, 'authority> {
    pub const fn cause(&self) -> XhciDmaError {
        self.cause
    }

    pub const fn has_release_debt(&self) -> bool {
        self.debt.is_some()
    }

    pub fn into_release_debt(self) -> Option<XhciDmaReleaseDebt<'arena, 'storage, 'authority>> {
        self.debt
    }
}

#[must_use = "release debt retains every frame not confirmed reclaimed"]
pub struct XhciDmaReleaseDebt<'arena, 'storage, 'authority> {
    arena: XhciDmaArena<'arena, 'storage, 'authority>,
    fault: XhciDmaReleaseFault,
}

impl core::fmt::Debug for XhciDmaReleaseDebt<'_, '_, '_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("XhciDmaReleaseDebt")
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

impl<'arena, 'storage, 'authority> XhciDmaReleaseDebt<'arena, 'storage, 'authority> {
    pub const fn fault(&self) -> XhciDmaReleaseFault {
        self.fault
    }

    pub fn region(&self, purpose: XhciDmaPurpose) -> Option<XhciDmaRegionRecord> {
        self.arena.region(purpose)
    }

    pub fn retry(
        self,
        quiescence: XhciDmaQuiescence,
    ) -> Result<XhciDmaReleaseReceipt, XhciDmaReleaseDebt<'arena, 'storage, 'authority>> {
        self.arena.release(quiescence)
    }
}

impl<'arena, 'storage, 'authority> XhciDmaArena<'arena, 'storage, 'authority> {
    pub fn allocate(
        frames: &'arena PhysicalFramePool<'storage>,
        authority: &'arena Capability<'authority, DmaControl>,
        identity: IdentityDmaWindow,
        scratchpad_count: u16,
        supports_64_bit_addresses: bool,
        secret: u64,
    ) -> Result<Self, XhciDmaBuildFailure<'arena, 'storage, 'authority>> {
        let mut arena = Self {
            frames,
            _authority: authority,
            effective_device_limit: if supports_64_bit_addresses {
                identity.device_address_limit
            } else {
                identity.device_address_limit.min(u64::from(u32::MAX))
            },
            identity,
            records: [XhciDmaRegionRecord::EMPTY; FIXED_REGION_CAPACITY],
            record_count: 0,
            scratchpad_count,
            supports_64_bit_addresses,
            secret,
            arena_root: 0,
            released_pages: 0,
            _not_send_or_sync: PhantomData,
        };
        let validation = if secret == 0 {
            Err(XhciDmaError::InvalidSecret)
        } else if scratchpad_count > MAXIMUM_SCRATCHPAD_BUFFERS {
            Err(XhciDmaError::ScratchpadCountUnsupported(scratchpad_count))
        } else {
            arena.allocate_all_regions()
        };
        if let Err(cause) = validation {
            return Err(arena.rollback_build(cause));
        }
        arena.finish_arena_root();
        Ok(arena)
    }

    fn allocate_all_regions(&mut self) -> Result<(), XhciDmaError> {
        self.allocate_region(XhciDmaPurpose::Dcbaa, 1)?;
        self.allocate_region(XhciDmaPurpose::CommandRing, 1)?;
        self.allocate_region(XhciDmaPurpose::EventRing, 1)?;
        self.allocate_region(XhciDmaPurpose::EventRingSegmentTable, 1)?;
        if self.scratchpad_count != 0 {
            let pointer_bytes = usize::from(self.scratchpad_count)
                .checked_mul(core::mem::size_of::<u64>())
                .ok_or(XhciDmaError::ScratchpadCountUnsupported(
                    self.scratchpad_count,
                ))?;
            let pointer_pages = pointer_bytes.div_ceil(PAGE_SIZE);
            self.allocate_region(XhciDmaPurpose::ScratchpadPointerArray, pointer_pages)?;
            self.allocate_region(
                XhciDmaPurpose::ScratchpadBuffers,
                usize::from(self.scratchpad_count),
            )?;
            self.initialize_scratchpad_links()?;
        }
        for record in &mut self.records[..self.record_count] {
            record.phase = XhciDmaRegionPhase::Ready;
        }
        fence(Ordering::Release);
        Ok(())
    }

    fn allocate_region(
        &mut self,
        purpose: XhciDmaPurpose,
        page_count: usize,
    ) -> Result<(), XhciDmaError> {
        let slot = self.record_count;
        if slot >= FIXED_REGION_CAPACITY {
            return Err(XhciDmaError::RegionCapacity);
        }
        let physical = self
            .frames
            .allocate_contiguous(page_count, 1)
            .ok_or(XhciDmaError::FrameUnavailable(purpose))?;
        self.records[slot] = XhciDmaRegionRecord {
            phase: XhciDmaRegionPhase::FrameOwned,
            generation: self.identity.generation,
            purpose,
            device: self.identity.device,
            physical_start: physical.as_u64(),
            physical_end: physical.as_u64(),
            device_address_start: physical.as_u64(),
            device_address_end: physical.as_u64(),
            cpu_start: 0,
            cpu_end: 0,
            page_count,
            region_root: 0,
        };
        self.record_count += 1;

        let bytes = page_count
            .checked_mul(PAGE_SIZE)
            .ok_or(XhciDmaError::PhysicalRangeOverflow(purpose))?;
        let bytes_u64 =
            u64::try_from(bytes).map_err(|_| XhciDmaError::PhysicalRangeOverflow(purpose))?;
        let physical_end = physical
            .as_u64()
            .checked_add(bytes_u64)
            .ok_or(XhciDmaError::PhysicalRangeOverflow(purpose))?;
        if physical.as_u64() < self.identity.physical_start
            || physical_end > self.identity.physical_end
        {
            return Err(XhciDmaError::OutsideIdentityAperture(purpose));
        }
        if physical_end - 1 > self.effective_device_limit {
            return Err(XhciDmaError::DeviceAddressLimit(purpose));
        }
        let physical_usize = usize::try_from(physical.as_u64())
            .map_err(|_| XhciDmaError::CpuAliasOverflow(purpose))?;
        let cpu_start = self
            .identity
            .direct_map_base
            .checked_add(physical_usize)
            .ok_or(XhciDmaError::CpuAliasOverflow(purpose))?;
        let cpu_end = cpu_start
            .checked_add(bytes)
            .ok_or(XhciDmaError::CpuAliasOverflow(purpose))?;
        if cpu_start < self.identity.cpu_start || cpu_end > self.identity.cpu_end {
            return Err(XhciDmaError::CpuAliasOverflow(purpose));
        }

        let record = &mut self.records[slot];
        record.physical_end = physical_end;
        record.device_address_end = physical_end;
        record.cpu_start = cpu_start;
        record.cpu_end = cpu_end;
        zero_volatile(cpu_start, bytes);
        record.phase = XhciDmaRegionPhase::Zeroed;
        record.region_root = region_root(self.secret, *record);
        Ok(())
    }

    fn initialize_scratchpad_links(&self) -> Result<(), XhciDmaError> {
        let array = self.region(XhciDmaPurpose::ScratchpadPointerArray).ok_or(
            XhciDmaError::MissingRegion(XhciDmaPurpose::ScratchpadPointerArray),
        )?;
        let buffers =
            self.region(XhciDmaPurpose::ScratchpadBuffers)
                .ok_or(XhciDmaError::MissingRegion(
                    XhciDmaPurpose::ScratchpadBuffers,
                ))?;
        for index in 0..self.scratchpad_count {
            let address = buffers
                .device_address_start
                .checked_add(u64::from(index) * PAGE_SIZE as u64)
                .ok_or(XhciDmaError::PhysicalRangeOverflow(
                    XhciDmaPurpose::ScratchpadBuffers,
                ))?;
            volatile_write_u64(array, usize::from(index) * 8, address)?;
        }
        let dcbaa = self
            .region(XhciDmaPurpose::Dcbaa)
            .ok_or(XhciDmaError::MissingRegion(XhciDmaPurpose::Dcbaa))?;
        volatile_write_u64(dcbaa, 0, array.device_address_start)?;
        fence(Ordering::Release);
        Ok(())
    }

    fn finish_arena_root(&mut self) {
        let mut root = mix(self.secret ^ ARENA_ROOT_DOMAIN, self.identity.proof_root);
        root = mix(root, u64::from(self.identity.generation));
        root = mix(root, u64::from(self.scratchpad_count));
        root = mix(root, self.supports_64_bit_addresses as u64);
        for record in &self.records[..self.record_count] {
            root = mix(root, record.region_root);
        }
        self.arena_root = canonical_root(root, ARENA_ROOT_DOMAIN);
    }

    fn rollback_build(
        mut self,
        cause: XhciDmaError,
    ) -> XhciDmaBuildFailure<'arena, 'storage, 'authority> {
        if self.record_count == 0 {
            return XhciDmaBuildFailure { cause, debt: None };
        }
        let debt = match self.release_records() {
            Ok(_) => None,
            Err(fault) => Some(XhciDmaReleaseDebt { arena: self, fault }),
        };
        XhciDmaBuildFailure { cause, debt }
    }

    pub const fn device(&self) -> PciAddress {
        self.identity.device
    }

    pub const fn generation(&self) -> u32 {
        self.identity.generation
    }

    pub const fn scratchpad_count(&self) -> u16 {
        self.scratchpad_count
    }

    pub const fn supports_64_bit_addresses(&self) -> bool {
        self.supports_64_bit_addresses
    }

    pub const fn arena_root(&self) -> u64 {
        self.arena_root
    }

    pub const fn region_count(&self) -> usize {
        self.record_count
    }

    pub fn region(&self, purpose: XhciDmaPurpose) -> Option<XhciDmaRegionRecord> {
        self.records[..self.record_count]
            .iter()
            .copied()
            .find(|record| record.purpose == purpose)
    }

    pub fn token(&self, purpose: XhciDmaPurpose) -> Option<XhciDmaRegionToken> {
        self.region(purpose).map(|record| XhciDmaRegionToken {
            generation: record.generation,
            purpose,
        })
    }

    pub fn write(
        &self,
        token: XhciDmaRegionToken,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), XhciDmaError> {
        if bytes.is_empty() {
            return Err(XhciDmaError::EmptyTransfer);
        }
        let record = self.record_for_token(token)?;
        validate_transfer(record, offset, bytes.len())?;
        compiler_fence(Ordering::SeqCst);
        for (index, byte) in bytes.iter().enumerate() {
            // SAFETY: bounds were checked against the owned CPU alias and no
            // reference into DMA memory is created or returned.
            unsafe {
                (record.cpu_start as *mut u8)
                    .add(offset + index)
                    .write_volatile(*byte)
            };
        }
        fence(Ordering::Release);
        Ok(())
    }

    pub fn read(
        &self,
        token: XhciDmaRegionToken,
        offset: usize,
        output: &mut [u8],
    ) -> Result<(), XhciDmaError> {
        if output.is_empty() {
            return Err(XhciDmaError::EmptyTransfer);
        }
        let record = self.record_for_token(token)?;
        validate_transfer(record, offset, output.len())?;
        fence(Ordering::Acquire);
        for (index, byte) in output.iter_mut().enumerate() {
            // SAFETY: bounds were checked against the owned CPU alias and the
            // volatile value is copied out rather than exposed by reference.
            *byte = unsafe {
                (record.cpu_start as *const u8)
                    .add(offset + index)
                    .read_volatile()
            };
        }
        compiler_fence(Ordering::SeqCst);
        Ok(())
    }

    fn record_for_token(
        &self,
        token: XhciDmaRegionToken,
    ) -> Result<XhciDmaRegionRecord, XhciDmaError> {
        if token.generation != self.identity.generation {
            return Err(XhciDmaError::StaleRegionToken);
        }
        let record = self
            .region(token.purpose)
            .ok_or(XhciDmaError::MissingRegion(token.purpose))?;
        if !matches!(
            record.phase,
            XhciDmaRegionPhase::Zeroed | XhciDmaRegionPhase::Ready
        ) {
            return Err(XhciDmaError::InactiveRegion(token.purpose));
        }
        Ok(record)
    }

    pub fn release(
        mut self,
        quiescence: XhciDmaQuiescence,
    ) -> Result<XhciDmaReleaseReceipt, XhciDmaReleaseDebt<'arena, 'storage, 'authority>> {
        if quiescence.device != self.identity.device
            || quiescence.generation != self.identity.generation
            || quiescence.evidence_root == 0
        {
            return Err(XhciDmaReleaseDebt {
                arena: self,
                fault: XhciDmaReleaseFault::QuiescenceMismatch,
            });
        }
        match self.release_records() {
            Ok(receipt) => Ok(receipt),
            Err(fault) => Err(XhciDmaReleaseDebt { arena: self, fault }),
        }
    }

    fn release_records(&mut self) -> Result<XhciDmaReleaseReceipt, XhciDmaReleaseFault> {
        if self.records[..self.record_count].iter().all(|record| {
            matches!(
                record.phase,
                XhciDmaRegionPhase::Empty | XhciDmaRegionPhase::Released
            )
        }) {
            return Err(XhciDmaReleaseFault::AlreadyReleased);
        }
        fence(Ordering::SeqCst);
        for index in (0..self.record_count).rev() {
            let record = self.records[index];
            if matches!(
                record.phase,
                XhciDmaRegionPhase::Empty | XhciDmaRegionPhase::Released
            ) {
                continue;
            }
            self.records[index].phase = XhciDmaRegionPhase::Releasing;
            if let Err(source) = self.frames.release_contiguous(
                PhysicalAddress::new(record.physical_start),
                record.page_count,
            ) {
                self.records[index].phase = XhciDmaRegionPhase::ReleaseDebt;
                return Err(XhciDmaReleaseFault::FrameAllocator {
                    purpose: record.purpose,
                    source,
                });
            }
            self.records[index].phase = XhciDmaRegionPhase::Released;
            self.released_pages += record.page_count;
        }
        let mut root = mix(
            self.arena_root ^ ARENA_ROOT_DOMAIN,
            u64::from(self.identity.generation),
        );
        root = mix(root, self.released_pages as u64);
        root = mix(root, self.identity.proof_root);
        Ok(XhciDmaReleaseReceipt {
            device: self.identity.device,
            generation: self.identity.generation,
            released_pages: self.released_pages,
            release_root: canonical_root(root, ARENA_ROOT_DOMAIN),
        })
    }
}

fn validate_transfer(
    record: XhciDmaRegionRecord,
    offset: usize,
    length: usize,
) -> Result<(), XhciDmaError> {
    if offset
        .checked_add(length)
        .is_none_or(|end| end > record.byte_length())
    {
        Err(XhciDmaError::TransferOutsideRegion)
    } else {
        Ok(())
    }
}

fn volatile_write_u64(
    record: XhciDmaRegionRecord,
    offset: usize,
    value: u64,
) -> Result<(), XhciDmaError> {
    validate_transfer(record, offset, core::mem::size_of::<u64>())?;
    compiler_fence(Ordering::SeqCst);
    // SAFETY: the complete u64 lies inside this exclusively owned, page-aligned
    // region. No reference is created and the value is naturally aligned.
    unsafe {
        (record.cpu_start as *mut u8)
            .add(offset)
            .cast::<u64>()
            .write_volatile(value)
    };
    fence(Ordering::Release);
    Ok(())
}

fn zero_volatile(cpu_start: usize, bytes: usize) {
    compiler_fence(Ordering::SeqCst);
    for offset in 0..bytes {
        // SAFETY: the caller supplies an exclusively owned CPU range of
        // exactly `bytes`; volatile writes prevent zeroing elision.
        unsafe { (cpu_start as *mut u8).add(offset).write_volatile(0) };
    }
    fence(Ordering::Release);
}

fn region_root(secret: u64, record: XhciDmaRegionRecord) -> u64 {
    let mut root = mix(secret ^ ARENA_ROOT_DOMAIN, record.purpose.code());
    root = mix(root, u64::from(record.generation));
    root = mix(root, u64::from(record.device.bus));
    root = mix(root, u64::from(record.device.slot));
    root = mix(root, u64::from(record.device.function));
    root = mix(root, record.physical_start);
    root = mix(root, record.physical_end);
    root = mix(root, record.device_address_start);
    root = mix(root, record.device_address_end);
    root = mix(root, record.page_count as u64);
    canonical_root(root, ARENA_ROOT_DOMAIN)
}

const fn canonical_root(root: u64, domain: u64) -> u64 {
    if root == 0 { domain } else { root }
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use abyss::frame::BitmapFrameAllocator;
    use abyss::memory::{MemoryMap, MemoryRegion, MemoryRegionKind};

    use super::*;
    use crate::capability::Authority;

    const RAM_PAGES: usize = 20;

    #[repr(C, align(4096))]
    struct TestRam([u8; RAM_PAGES * PAGE_SIZE]);

    fn pool<'a>(
        ram: &'a mut TestRam,
        bitmap: &'a mut [u64; 2],
        usable_pages: usize,
    ) -> (PhysicalFramePool<'a>, usize) {
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(PAGE_SIZE as u64),
            PhysicalAddress::new((usable_pages + 1) as u64 * PAGE_SIZE as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let allocator =
            BitmapFrameAllocator::new(&map, RAM_PAGES as u64 * PAGE_SIZE as u64, bitmap).unwrap();
        let direct_map_base = ram.0.as_mut_ptr() as usize - PAGE_SIZE;
        (PhysicalFramePool::new(allocator), direct_map_base)
    }

    fn identity(
        direct_map_base: usize,
        physical_start: u64,
        physical_bytes: u64,
        device_limit: u64,
    ) -> IdentityDmaWindow {
        unsafe {
            IdentityDmaWindow::establish(
                PciAddress::new(0, 5, 0).unwrap(),
                physical_start,
                physical_bytes,
                direct_map_base,
                device_limit,
                7,
                0x1234,
                IdentityDmaObservation {
                    x86_cache_coherent: true,
                    requester_remapping_active: false,
                },
            )
        }
        .unwrap()
    }

    fn quiescence() -> XhciDmaQuiescence {
        unsafe { XhciDmaQuiescence::establish(PciAddress::new(0, 5, 0).unwrap(), 7, 0x5678) }
            .unwrap()
    }

    #[test]
    fn allocates_four_base_pages_and_exact_scratchpad_storage_zeroed() {
        let mut ram = TestRam([0xa5; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let (pool, direct_map_base) = pool(&mut ram, &mut bitmap, RAM_PAGES - 1);
        let authority = unsafe { Authority::assume_root() };
        let dma = authority.grant::<DmaControl>();
        let free_before = pool.free_frames();
        let arena = XhciDmaArena::allocate(
            &pool,
            &dma,
            identity(
                direct_map_base,
                PAGE_SIZE as u64,
                (RAM_PAGES - 1) as u64 * PAGE_SIZE as u64,
                u64::MAX,
            ),
            2,
            true,
            0xfeed,
        )
        .unwrap();
        assert_eq!(arena.region_count(), 6);
        assert_eq!(
            pool.free_frames(),
            free_before - (XHCI_BASE_REGION_COUNT + 1 + 2)
        );
        for purpose in [
            XhciDmaPurpose::Dcbaa,
            XhciDmaPurpose::CommandRing,
            XhciDmaPurpose::EventRing,
            XhciDmaPurpose::EventRingSegmentTable,
        ] {
            assert_eq!(arena.region(purpose).unwrap().page_count, 1);
        }
        let array = arena
            .region(XhciDmaPurpose::ScratchpadPointerArray)
            .unwrap();
        let buffers = arena.region(XhciDmaPurpose::ScratchpadBuffers).unwrap();
        assert_eq!(buffers.page_count, 2);
        let mut pointer = [0_u8; 8];
        arena
            .read(arena.token(XhciDmaPurpose::Dcbaa).unwrap(), 0, &mut pointer)
            .unwrap();
        assert_eq!(u64::from_le_bytes(pointer), array.device_address_start);
        for index in 0..2 {
            arena
                .read(
                    arena.token(XhciDmaPurpose::ScratchpadPointerArray).unwrap(),
                    index * 8,
                    &mut pointer,
                )
                .unwrap();
            assert_eq!(
                u64::from_le_bytes(pointer),
                buffers.device_address_start + index as u64 * PAGE_SIZE as u64
            );
        }
        let mut zero = [0xff_u8; 2];
        arena
            .read(
                arena.token(XhciDmaPurpose::CommandRing).unwrap(),
                PAGE_SIZE - 2,
                &mut zero,
            )
            .unwrap();
        assert_eq!(zero, [0, 0]);
        assert_ne!(arena.arena_root(), 0);
        let receipt = arena.release(quiescence()).unwrap();
        assert_eq!(receipt.released_pages, XHCI_BASE_REGION_COUNT + 1 + 2);
        assert_eq!(pool.free_frames(), free_before);
    }

    #[test]
    fn volatile_access_is_bounded_and_generation_checked() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let (pool, direct_map_base) = pool(&mut ram, &mut bitmap, RAM_PAGES - 1);
        let authority = unsafe { Authority::assume_root() };
        let dma = authority.grant::<DmaControl>();
        let arena = XhciDmaArena::allocate(
            &pool,
            &dma,
            identity(
                direct_map_base,
                PAGE_SIZE as u64,
                (RAM_PAGES - 1) as u64 * PAGE_SIZE as u64,
                u64::MAX,
            ),
            0,
            true,
            9,
        )
        .unwrap();
        let token = arena.token(XhciDmaPurpose::CommandRing).unwrap();
        assert_eq!(arena.write(token, PAGE_SIZE - 1, &[0x5a]), Ok(()));
        let mut value = [0];
        assert_eq!(arena.read(token, PAGE_SIZE - 1, &mut value), Ok(()));
        assert_eq!(value, [0x5a]);
        assert_eq!(
            arena.write(token, PAGE_SIZE, &[1]),
            Err(XhciDmaError::TransferOutsideRegion)
        );
        assert_eq!(arena.write(token, 0, &[]), Err(XhciDmaError::EmptyTransfer));
        let stale = XhciDmaRegionToken {
            generation: token.generation + 1,
            purpose: token.purpose,
        };
        assert_eq!(
            arena.read(stale, 0, &mut value),
            Err(XhciDmaError::StaleRegionToken)
        );
        arena.release(quiescence()).unwrap();
    }

    #[test]
    fn partial_construction_rolls_back_every_frame() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let (pool, direct_map_base) = pool(&mut ram, &mut bitmap, 5);
        let authority = unsafe { Authority::assume_root() };
        let dma = authority.grant::<DmaControl>();
        let free_before = pool.free_frames();
        let failure = match XhciDmaArena::allocate(
            &pool,
            &dma,
            identity(
                direct_map_base,
                PAGE_SIZE as u64,
                5 * PAGE_SIZE as u64,
                u64::MAX,
            ),
            1,
            true,
            11,
        ) {
            Ok(_) => panic!("undersized pool unexpectedly built the arena"),
            Err(failure) => failure,
        };
        assert_eq!(
            failure.cause(),
            XhciDmaError::FrameUnavailable(XhciDmaPurpose::ScratchpadBuffers)
        );
        assert!(!failure.has_release_debt());
        assert_eq!(pool.free_frames(), free_before);
    }

    #[test]
    fn non_ac64_rejects_frames_above_four_gibibytes_and_rolls_back() {
        let physical_start = u64::from(u32::MAX) + 1;
        let maximum = physical_start + 8 * PAGE_SIZE as u64;
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let words = BitmapFrameAllocator::storage_words(maximum).unwrap();
        let mut bitmap = vec![0_u64; words];
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(physical_start),
            PhysicalAddress::new(maximum),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let allocator = BitmapFrameAllocator::new(&map, maximum, &mut bitmap).unwrap();
        let pool = PhysicalFramePool::new(allocator);
        let direct_map_base = ram.0.as_mut_ptr() as usize - physical_start as usize;
        let authority = unsafe { Authority::assume_root() };
        let dma = authority.grant::<DmaControl>();
        let free_before = pool.free_frames();
        let failure = match XhciDmaArena::allocate(
            &pool,
            &dma,
            identity(
                direct_map_base,
                physical_start,
                maximum - physical_start,
                u64::MAX,
            ),
            0,
            false,
            13,
        ) {
            Ok(_) => panic!("non-AC64 controller accepted high DMA frames"),
            Err(failure) => failure,
        };
        assert_eq!(
            failure.cause(),
            XhciDmaError::DeviceAddressLimit(XhciDmaPurpose::Dcbaa)
        );
        assert!(!failure.has_release_debt());
        assert_eq!(pool.free_frames(), free_before);
    }

    #[test]
    fn double_release_is_rejected_without_touching_the_pool_twice() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let (pool, direct_map_base) = pool(&mut ram, &mut bitmap, RAM_PAGES - 1);
        let authority = unsafe { Authority::assume_root() };
        let dma = authority.grant::<DmaControl>();
        let free_before = pool.free_frames();
        let mut arena = XhciDmaArena::allocate(
            &pool,
            &dma,
            identity(
                direct_map_base,
                PAGE_SIZE as u64,
                (RAM_PAGES - 1) as u64 * PAGE_SIZE as u64,
                u64::MAX,
            ),
            0,
            true,
            17,
        )
        .unwrap();
        assert!(arena.release_records().is_ok());
        assert_eq!(pool.free_frames(), free_before);
        assert_eq!(
            arena.release_records(),
            Err(XhciDmaReleaseFault::AlreadyReleased)
        );
        assert_eq!(pool.free_frames(), free_before);
    }

    #[test]
    fn release_fault_retains_debt_and_every_unreleased_region() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let (pool, direct_map_base) = pool(&mut ram, &mut bitmap, RAM_PAGES - 1);
        let authority = unsafe { Authority::assume_root() };
        let dma = authority.grant::<DmaControl>();
        let arena = XhciDmaArena::allocate(
            &pool,
            &dma,
            identity(
                direct_map_base,
                PAGE_SIZE as u64,
                (RAM_PAGES - 1) as u64 * PAGE_SIZE as u64,
                u64::MAX,
            ),
            0,
            true,
            19,
        )
        .unwrap();
        let event = arena.region(XhciDmaPurpose::EventRing).unwrap();
        pool.release(PhysicalAddress::new(event.physical_start))
            .unwrap();
        let debt = arena.release(quiescence()).unwrap_err();
        assert_eq!(
            debt.fault(),
            XhciDmaReleaseFault::FrameAllocator {
                purpose: XhciDmaPurpose::EventRing,
                source: FrameAllocatorError::DoubleFree
            }
        );
        assert_eq!(
            debt.region(XhciDmaPurpose::EventRing).unwrap().phase,
            XhciDmaRegionPhase::ReleaseDebt
        );
        assert_eq!(
            debt.region(XhciDmaPurpose::CommandRing).unwrap().phase,
            XhciDmaRegionPhase::Ready
        );
        assert_eq!(
            debt.region(XhciDmaPurpose::Dcbaa).unwrap().phase,
            XhciDmaRegionPhase::Ready
        );
    }

    #[test]
    fn identity_proof_rejects_coherency_and_remapping_shortcuts() {
        let address = PciAddress::new(0, 5, 0).unwrap();
        let base = PAGE_SIZE as u64;
        let common = |observation| unsafe {
            IdentityDmaWindow::establish(
                address,
                base,
                PAGE_SIZE as u64,
                0x1000,
                u64::MAX,
                1,
                1,
                observation,
            )
        };
        assert_eq!(
            common(IdentityDmaObservation {
                x86_cache_coherent: false,
                requester_remapping_active: false
            })
            .map(|_| ()),
            Err(IdentityDmaError::X86CoherencyUnproven)
        );
        assert_eq!(
            common(IdentityDmaObservation {
                x86_cache_coherent: true,
                requester_remapping_active: true
            })
            .map(|_| ()),
            Err(IdentityDmaError::ActiveRequesterRemapping)
        );
    }
}
