#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Event {
    pub timestamp: u64,
    pub ticket: u64,
    pub cpu_id: u32,
    pub kind: u16,
    pub flags: u16,
    pub argument_zero: u64,
    pub argument_one: u64,
}

impl Event {
    const EMPTY: Self = Self {
        timestamp: 0,
        ticket: 0,
        cpu_id: 0,
        kind: 0,
        flags: 0,
        argument_zero: 0,
        argument_one: 0,
    };
}

/// Bounded deterministic event log with explicit single-writer ownership.
///
/// Callers provide timestamps and CPU identifiers so architecture access stays
/// in Boulder. Requiring `&mut self` prevents concurrent slot mutation. A
/// surrounding kernel lock may serialize writers when a global recorder is
/// required.
pub struct Recorder<const CAPACITY: usize> {
    events: [Event; CAPACITY],
    next_ticket: u64,
    retained: usize,
}

impl<const CAPACITY: usize> Recorder<CAPACITY> {
    pub const fn new() -> Self {
        assert!(CAPACITY > 0);
        Self {
            events: [Event::EMPTY; CAPACITY],
            next_ticket: 0,
            retained: 0,
        }
    }

    pub fn record(
        &mut self,
        timestamp: u64,
        cpu_id: u32,
        kind: u16,
        argument_zero: u64,
        argument_one: u64,
    ) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1);
        let slot = ticket as usize % CAPACITY;
        self.events[slot] = Event {
            timestamp,
            ticket,
            cpu_id,
            kind,
            flags: 0,
            argument_zero,
            argument_one,
        };
        self.retained = self.retained.saturating_add(1).min(CAPACITY);
        ticket
    }

    pub const fn next_ticket(&self) -> u64 {
        self.next_ticket
    }

    pub const fn retained(&self) -> usize {
        self.retained
    }

    pub fn event(&self, ticket: u64) -> Option<Event> {
        let oldest = self.next_ticket.wrapping_sub(self.retained as u64);
        if !ticket_in_window(ticket, oldest, self.retained as u64) {
            return None;
        }
        let event = self.events[ticket as usize % CAPACITY];
        (event.ticket == ticket).then_some(event)
    }

    pub fn replay(&self) -> Replay<'_, CAPACITY> {
        Replay {
            recorder: self,
            next_ticket: self.next_ticket.wrapping_sub(self.retained as u64),
            remaining: self.retained,
        }
    }
}

impl<const CAPACITY: usize> Default for Recorder<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

fn ticket_in_window(ticket: u64, oldest: u64, length: u64) -> bool {
    ticket.wrapping_sub(oldest) < length
}

pub struct Replay<'recorder, const CAPACITY: usize> {
    recorder: &'recorder Recorder<CAPACITY>,
    next_ticket: u64,
    remaining: usize,
}

impl<const CAPACITY: usize> Iterator for Replay<'_, CAPACITY> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let event = self.recorder.event(self.next_ticket)?;
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.remaining -= 1;
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_the_latest_bounded_window_in_ticket_order() {
        let mut recorder = Recorder::<3>::new();
        for value in 0..5 {
            recorder.record(value, 0, 7, value, 0);
        }
        let mut replay = recorder.replay();
        assert_eq!(replay.next().unwrap().argument_zero, 2);
        assert_eq!(replay.next().unwrap().argument_zero, 3);
        assert_eq!(replay.next().unwrap().argument_zero, 4);
        assert_eq!(replay.next(), None);
        assert_eq!(recorder.event(1), None);
        assert_eq!(recorder.event(4).unwrap().ticket, 4);
    }
}
