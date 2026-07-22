use crate::paging::PhysicalAddress;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationKind {
    LowMemory,
    KernelImage,
    BootInformation,
    BootModule,
    BootstrapHeap,
    AllocatorMetadata,
    Firmware,
    DeviceMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reservation {
    pub start: PhysicalAddress,
    pub end: PhysicalAddress,
    pub kind: ReservationKind,
}

impl Reservation {
    const EMPTY: Self = Self {
        start: PhysicalAddress::new(0),
        end: PhysicalAddress::new(0),
        kind: ReservationKind::LowMemory,
    };

    pub const fn new(start: PhysicalAddress, end: PhysicalAddress, kind: ReservationKind) -> Self {
        Self { start, end, kind }
    }
}

pub struct ReservationTable<const N: usize> {
    entries: [Reservation; N],
    length: usize,
}

impl<const N: usize> ReservationTable<N> {
    pub const fn new() -> Self {
        Self {
            entries: [Reservation::EMPTY; N],
            length: 0,
        }
    }

    pub fn push(&mut self, reservation: Reservation) -> Result<(), ReservationError> {
        if reservation.start.as_u64() >= reservation.end.as_u64() {
            return Err(ReservationError::InvalidRange);
        }
        let slot = self
            .entries
            .get_mut(self.length)
            .ok_or(ReservationError::CapacityExceeded)?;
        *slot = reservation;
        self.length += 1;
        Ok(())
    }

    pub fn entries(&self) -> &[Reservation] {
        &self.entries[..self.length]
    }
}

impl<const N: usize> Default for ReservationTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationError {
    InvalidRange,
    CapacityExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_typed_reservations() {
        let mut table = ReservationTable::<2>::new();
        table
            .push(Reservation::new(
                PhysicalAddress::new(0x1000),
                PhysicalAddress::new(0x2000),
                ReservationKind::KernelImage,
            ))
            .unwrap();
        assert_eq!(table.entries().len(), 1);
        assert_eq!(table.entries()[0].kind, ReservationKind::KernelImage);
    }
}
