//! Fixed-capacity, generation-safe I/O virtual address lease management.

pub const IOVA_PAGE_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IovaRange {
    start: u64,
    end: u64,
}

impl IovaRange {
    const EMPTY: Self = Self { start: 0, end: 0 };

    pub fn new(start: u64, length: u64) -> Result<Self, IovaError> {
        let end = start.checked_add(length).ok_or(IovaError::InvalidRange)?;
        if length == 0
            || start % IOVA_PAGE_SIZE != 0
            || length % IOVA_PAGE_SIZE != 0
            || end % IOVA_PAGE_SIZE != 0
        {
            return Err(IovaError::InvalidRange);
        }
        Ok(Self { start, end })
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn end(self) -> u64 {
        self.end
    }

    pub const fn length(self) -> u64 {
        self.end - self.start
    }

    pub const fn page_count(self) -> u64 {
        self.length() / IOVA_PAGE_SIZE
    }

    pub const fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IovaLease(u64);

impl IovaLease {
    pub const INVALID: Self = Self(0);

    pub const fn from_raw(raw: u64) -> Option<Self> {
        let slot = raw as u32;
        let generation = (raw >> 32) as u32;
        if slot == 0 || generation == 0 {
            None
        } else {
            Some(Self(raw))
        }
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    const fn new(slot: usize, generation: u32) -> Self {
        Self(((generation as u64) << 32) | (slot as u64 + 1))
    }

    const fn slot(self) -> Option<usize> {
        let encoded = self.0 as u32;
        if encoded == 0 {
            None
        } else {
            Some((encoded - 1) as usize)
        }
    }

    const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IovaRequest {
    page_count: u64,
    alignment_pages: u64,
}

impl IovaRequest {
    pub fn new(page_count: u64, alignment_pages: u64) -> Result<Self, IovaError> {
        if page_count == 0 || alignment_pages == 0 || !alignment_pages.is_power_of_two() {
            return Err(IovaError::InvalidRequest);
        }
        page_count
            .checked_mul(IOVA_PAGE_SIZE)
            .and_then(|_| alignment_pages.checked_mul(IOVA_PAGE_SIZE))
            .ok_or(IovaError::InvalidRequest)?;
        Ok(Self {
            page_count,
            alignment_pages,
        })
    }

    pub const fn page_count(self) -> u64 {
        self.page_count
    }

    pub const fn alignment_pages(self) -> u64 {
        self.alignment_pages
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IovaError {
    InvalidRange,
    InvalidRequest,
    InvalidCapacity,
    ReservedCapacity,
    ReservedOutsideAperture,
    OverlappingReservedRanges,
    LeaseCapacity,
    AddressSpaceExhausted,
    ExactRangeUnavailable,
    StaleLease,
    BatchOutputMismatch,
}

#[derive(Clone, Copy)]
struct LeaseSlot {
    range: IovaRange,
    generation: u32,
    active: bool,
}

impl LeaseSlot {
    const EMPTY: Self = Self {
        range: IovaRange::EMPTY,
        generation: 0,
        active: false,
    };
}

/// A deterministic first-fit IOVA allocator with a fixed lease ledger.
///
/// Released slots retain their generation, so stale handles cannot release or
/// inspect a replacement lease. A slot is retired instead of wrapping its
/// generation, preventing handle ABA at the cost of eventual capacity loss.
#[derive(Clone, Copy)]
pub struct IovaLedger<const LEASES: usize, const RESERVED: usize> {
    aperture: IovaRange,
    reserved: [IovaRange; RESERVED],
    reserved_count: usize,
    leases: [LeaseSlot; LEASES],
    active_count: usize,
}

impl<const LEASES: usize, const RESERVED: usize> IovaLedger<LEASES, RESERVED> {
    pub fn new(aperture: IovaRange, reserved: &[IovaRange]) -> Result<Self, IovaError> {
        if LEASES == 0 || LEASES > u32::MAX as usize {
            return Err(IovaError::InvalidCapacity);
        }
        if reserved.len() > RESERVED {
            return Err(IovaError::ReservedCapacity);
        }
        let mut ledger = Self {
            aperture,
            reserved: [IovaRange::EMPTY; RESERVED],
            reserved_count: 0,
            leases: [LeaseSlot::EMPTY; LEASES],
            active_count: 0,
        };
        for range in reserved.iter().copied() {
            ledger.insert_reserved(range)?;
        }
        Ok(ledger)
    }

    pub const fn aperture(&self) -> IovaRange {
        self.aperture
    }

    pub fn reserved_ranges(&self) -> &[IovaRange] {
        &self.reserved[..self.reserved_count]
    }

    pub const fn active_lease_count(&self) -> usize {
        self.active_count
    }

    pub fn reserve_pages(&mut self, page_count: u64) -> Result<IovaLease, IovaError> {
        self.reserve(IovaRequest::new(page_count, 1)?)
    }

    pub fn reserve_aligned(
        &mut self,
        page_count: u64,
        alignment_pages: u64,
    ) -> Result<IovaLease, IovaError> {
        self.reserve(IovaRequest::new(page_count, alignment_pages)?)
    }

    pub fn reserve(&mut self, request: IovaRequest) -> Result<IovaLease, IovaError> {
        let slot_index = self
            .leases
            .iter()
            .position(|slot| !slot.active && slot.generation != u32::MAX)
            .ok_or(IovaError::LeaseCapacity)?;
        let range = self.first_fit(request)?;
        let generation = self.leases[slot_index].generation + 1;
        self.leases[slot_index] = LeaseSlot {
            range,
            generation,
            active: true,
        };
        self.active_count += 1;
        Ok(IovaLease::new(slot_index, generation))
    }

    /// Reserves one caller-selected range without silently relocating it.
    /// This is required by devices whose DMA descriptors contain physical
    /// addresses and by translated domains that intentionally use IOVA==PA.
    pub fn reserve_exact(&mut self, range: IovaRange) -> Result<IovaLease, IovaError> {
        if !self.aperture.contains(range)
            || self.reserved[..self.reserved_count]
                .iter()
                .any(|blocker| blocker.overlaps(range))
            || self
                .leases
                .iter()
                .filter(|slot| slot.active)
                .any(|blocker| blocker.range.overlaps(range))
        {
            return Err(IovaError::ExactRangeUnavailable);
        }
        let slot_index = self
            .leases
            .iter()
            .position(|slot| !slot.active && slot.generation != u32::MAX)
            .ok_or(IovaError::LeaseCapacity)?;
        let generation = self.leases[slot_index].generation + 1;
        self.leases[slot_index] = LeaseSlot {
            range,
            generation,
            active: true,
        };
        self.active_count += 1;
        Ok(IovaLease::new(slot_index, generation))
    }

    /// Reserves every request atomically. Neither the ledger nor `output` is
    /// changed unless the full batch succeeds.
    pub fn reserve_many(
        &mut self,
        requests: &[IovaRequest],
        output: &mut [IovaLease],
    ) -> Result<(), IovaError> {
        if requests.is_empty() || requests.len() != output.len() || requests.len() > LEASES {
            return Err(IovaError::BatchOutputMismatch);
        }
        let mut candidate = *self;
        let mut staged = [IovaLease::INVALID; LEASES];
        for (index, request) in requests.iter().copied().enumerate() {
            staged[index] = candidate.reserve(request)?;
        }
        *self = candidate;
        output.copy_from_slice(&staged[..requests.len()]);
        Ok(())
    }

    pub fn range(&self, lease: IovaLease) -> Result<IovaRange, IovaError> {
        let slot = self.slot(lease)?;
        Ok(slot.range)
    }

    pub fn release(&mut self, lease: IovaLease) -> Result<IovaRange, IovaError> {
        let slot_index = lease.slot().ok_or(IovaError::StaleLease)?;
        let slot = self
            .leases
            .get_mut(slot_index)
            .filter(|slot| slot.active && slot.generation == lease.generation())
            .ok_or(IovaError::StaleLease)?;
        let range = slot.range;
        slot.range = IovaRange::EMPTY;
        slot.active = false;
        self.active_count -= 1;
        Ok(range)
    }

    fn insert_reserved(&mut self, range: IovaRange) -> Result<(), IovaError> {
        if !self.aperture.contains(range) {
            return Err(IovaError::ReservedOutsideAperture);
        }
        if self.reserved[..self.reserved_count]
            .iter()
            .any(|existing| existing.overlaps(range))
        {
            return Err(IovaError::OverlappingReservedRanges);
        }
        let position = self.reserved[..self.reserved_count]
            .iter()
            .position(|existing| range.start < existing.start)
            .unwrap_or(self.reserved_count);
        for index in (position..self.reserved_count).rev() {
            self.reserved[index + 1] = self.reserved[index];
        }
        self.reserved[position] = range;
        self.reserved_count += 1;
        Ok(())
    }

    fn first_fit(&self, request: IovaRequest) -> Result<IovaRange, IovaError> {
        let length = request
            .page_count
            .checked_mul(IOVA_PAGE_SIZE)
            .ok_or(IovaError::InvalidRequest)?;
        let alignment = request
            .alignment_pages
            .checked_mul(IOVA_PAGE_SIZE)
            .ok_or(IovaError::InvalidRequest)?;
        let mut start =
            align_up(self.aperture.start, alignment).ok_or(IovaError::AddressSpaceExhausted)?;

        loop {
            let end = start
                .checked_add(length)
                .filter(|end| *end <= self.aperture.end)
                .ok_or(IovaError::AddressSpaceExhausted)?;
            let candidate = IovaRange { start, end };
            let mut advance_to = 0_u64;
            for blocker in self.reserved[..self.reserved_count].iter().copied().chain(
                self.leases
                    .iter()
                    .filter(|slot| slot.active)
                    .map(|slot| slot.range),
            ) {
                if candidate.overlaps(blocker) {
                    advance_to = advance_to.max(blocker.end);
                }
            }
            if advance_to == 0 {
                return Ok(candidate);
            }
            start = align_up(advance_to, alignment).ok_or(IovaError::AddressSpaceExhausted)?;
        }
    }

    fn slot(&self, lease: IovaLease) -> Result<&LeaseSlot, IovaError> {
        let slot_index = lease.slot().ok_or(IovaError::StaleLease)?;
        self.leases
            .get(slot_index)
            .filter(|slot| slot.active && slot.generation == lease.generation())
            .ok_or(IovaError::StaleLease)
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(start_page: u64, page_count: u64) -> IovaRange {
        IovaRange::new(start_page * IOVA_PAGE_SIZE, page_count * IOVA_PAGE_SIZE).unwrap()
    }

    #[test]
    fn allocates_first_fit_around_sorted_reserved_ranges() {
        let aperture = pages(0, 12);
        let reserved = [pages(7, 1), pages(2, 2)];
        let mut ledger = IovaLedger::<8, 4>::new(aperture, &reserved).unwrap();

        assert_eq!(ledger.reserved_ranges(), &[pages(2, 2), pages(7, 1)]);
        let first = ledger.reserve_pages(2).unwrap();
        let second = ledger.reserve_pages(3).unwrap();
        let aligned = ledger.reserve_aligned(1, 4).unwrap();

        assert_eq!(ledger.range(first), Ok(pages(0, 2)));
        assert_eq!(ledger.range(second), Ok(pages(4, 3)));
        assert_eq!(ledger.range(aligned), Ok(pages(8, 1)));
        assert_eq!(ledger.active_lease_count(), 3);
    }

    #[test]
    fn release_recycles_ranges_but_rejects_stale_generations() {
        let mut ledger = IovaLedger::<2, 0>::new(pages(1, 4), &[]).unwrap();
        let original = ledger.reserve_pages(2).unwrap();
        assert_eq!(ledger.release(original), Ok(pages(1, 2)));
        assert_eq!(ledger.release(original), Err(IovaError::StaleLease));

        let replacement = ledger.reserve_pages(2).unwrap();
        assert_eq!(ledger.range(replacement), Ok(pages(1, 2)));
        assert_ne!(replacement, original);
        assert_eq!(ledger.range(original), Err(IovaError::StaleLease));
        assert_eq!(ledger.release(original), Err(IovaError::StaleLease));
        assert_eq!(ledger.active_lease_count(), 1);

        let forged = IovaLease::from_raw(replacement.raw() + (1_u64 << 32)).unwrap();
        assert_eq!(ledger.range(forged), Err(IovaError::StaleLease));
    }

    #[test]
    fn exact_reservation_never_relocates_or_overlaps() {
        let mut ledger: IovaLedger<4, 1> = IovaLedger::new(pages(1, 8), &[pages(3, 1)]).unwrap();
        let exact = ledger.reserve_exact(pages(5, 1)).unwrap();
        assert_eq!(ledger.range(exact), Ok(pages(5, 1)));
        assert_eq!(
            ledger.reserve_exact(pages(5, 1)),
            Err(IovaError::ExactRangeUnavailable)
        );
        assert_eq!(
            ledger.reserve_exact(pages(3, 1)),
            Err(IovaError::ExactRangeUnavailable)
        );
    }

    #[test]
    fn adjacent_releases_coalesce_for_a_larger_first_fit() {
        let mut ledger = IovaLedger::<6, 0>::new(pages(0, 8), &[]).unwrap();
        let first = ledger.reserve_pages(2).unwrap();
        let second = ledger.reserve_pages(2).unwrap();
        let third = ledger.reserve_pages(2).unwrap();
        assert_eq!(ledger.range(third), Ok(pages(4, 2)));

        ledger.release(first).unwrap();
        ledger.release(second).unwrap();
        let combined = ledger.reserve_pages(4).unwrap();
        assert_eq!(ledger.range(combined), Ok(pages(0, 4)));

        ledger.release(third).unwrap();
        ledger.release(combined).unwrap();
        let complete = ledger.reserve_pages(8).unwrap();
        assert_eq!(ledger.range(complete), Ok(pages(0, 8)));
    }

    #[test]
    fn failed_batch_changes_neither_ledger_nor_outputs() {
        let mut ledger = IovaLedger::<3, 0>::new(pages(4, 4), &[]).unwrap();
        let requests = [
            IovaRequest::new(2, 1).unwrap(),
            IovaRequest::new(3, 1).unwrap(),
        ];
        let sentinel = IovaLease::from_raw((9_u64 << 32) | 7).unwrap();
        let mut output = [sentinel; 2];

        assert_eq!(
            ledger.reserve_many(&requests, &mut output),
            Err(IovaError::AddressSpaceExhausted)
        );
        assert_eq!(ledger.active_lease_count(), 0);
        assert_eq!(output, [sentinel; 2]);

        let all = ledger.reserve_pages(4).unwrap();
        assert_eq!(ledger.range(all), Ok(pages(4, 4)));
        assert_eq!(all.raw() >> 32, 1);
    }

    #[test]
    fn successful_batch_is_contiguous_deterministic_and_non_overlapping() {
        let mut ledger = IovaLedger::<4, 1>::new(pages(0, 10), &[pages(3, 1)]).unwrap();
        let requests = [
            IovaRequest::new(2, 1).unwrap(),
            IovaRequest::new(2, 2).unwrap(),
            IovaRequest::new(1, 1).unwrap(),
        ];
        let mut leases = [IovaLease::INVALID; 3];
        ledger.reserve_many(&requests, &mut leases).unwrap();

        let ranges = [
            ledger.range(leases[0]).unwrap(),
            ledger.range(leases[1]).unwrap(),
            ledger.range(leases[2]).unwrap(),
        ];
        assert_eq!(ranges, [pages(0, 2), pages(4, 2), pages(2, 1)]);
        for left in 0..ranges.len() {
            for right in left + 1..ranges.len() {
                assert!(!ranges[left].overlaps(ranges[right]));
            }
        }
        assert!(ranges.iter().all(|range| !range.overlaps(pages(3, 1))));
    }

    #[test]
    fn rejects_malformed_configuration_requests_and_generation_wrap() {
        assert_eq!(
            IovaRange::new(1, IOVA_PAGE_SIZE),
            Err(IovaError::InvalidRange)
        );
        assert_eq!(IovaRange::new(0, 0), Err(IovaError::InvalidRange));
        assert_eq!(
            IovaRange::new(u64::MAX - (IOVA_PAGE_SIZE - 1), IOVA_PAGE_SIZE),
            Err(IovaError::InvalidRange)
        );
        assert_eq!(IovaRequest::new(0, 1), Err(IovaError::InvalidRequest));
        assert_eq!(IovaRequest::new(1, 3), Err(IovaError::InvalidRequest));
        assert!(matches!(
            IovaLedger::<0, 0>::new(pages(0, 1), &[]),
            Err(IovaError::InvalidCapacity)
        ));
        assert!(matches!(
            IovaLedger::<1, 1>::new(pages(0, 4), &[pages(1, 2), pages(2, 1)]),
            Err(IovaError::ReservedCapacity)
        ));
        assert!(matches!(
            IovaLedger::<1, 2>::new(pages(0, 4), &[pages(1, 2), pages(2, 1)]),
            Err(IovaError::OverlappingReservedRanges)
        ));
        assert!(matches!(
            IovaLedger::<1, 1>::new(pages(0, 4), &[pages(4, 1)]),
            Err(IovaError::ReservedOutsideAperture)
        ));

        let mut ledger = IovaLedger::<1, 0>::new(pages(0, 1), &[]).unwrap();
        ledger.leases[0].generation = u32::MAX - 1;
        let final_generation = ledger.reserve_pages(1).unwrap();
        assert_eq!(final_generation.raw() >> 32, u64::from(u32::MAX));
        ledger.release(final_generation).unwrap();
        assert_eq!(ledger.reserve_pages(1), Err(IovaError::LeaseCapacity));
    }
}
