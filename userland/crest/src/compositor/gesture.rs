// GESTURE RECOGNIZER — InputEvent stream → GestureEvent classifier
//
// State machine per pointer slot: Idle → Pressed → Tapping|Holding|Dragging → Idle
// History ring buffer per slot → velocity for swipe detection.
// GestureQueue: fixed-size FIFO of emitted GestureEvents.

use crate::input::InputEvent;

pub const MAX_POINTERS:       usize = 4;
pub const HISTORY_LEN:        usize = 16;
pub const TAP_MAX_TICKS:      u64   = 20;
pub const TAP_MAX_PIXELS:     u32   = 8;
pub const HOLD_TICKS:         u64   = 60;
pub const DRAG_THRESHOLD:     u32   = 6;
pub const SWIPE_VELOCITY:     u32   = 8;
pub const MAX_GESTURE_QUEUE:  usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwipeDirection { Up, Down, Left, Right }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GestureEvent {
    Tap        { x: i32, y: i32, pointer_id: u8 },
    Hold       { x: i32, y: i32, pointer_id: u8 },
    DragBegin  { x: i32, y: i32, pointer_id: u8 },
    DragUpdate { dx: i32, dy: i32, x: i32, y: i32, pointer_id: u8 },
    DragEnd    { x: i32, y: i32, total_dx: i32, total_dy: i32, pointer_id: u8 },
    Swipe      { direction: SwipeDirection, velocity: u32, pointer_id: u8 },
    ScrollV    { value: i32, x: i32, y: i32 },
    ScrollH    { value: i32, x: i32, y: i32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerState {
    Idle,
    Pressed  { down_x: i32, down_y: i32, down_tick: u64 },
    Holding  { x: i32, y: i32 },
    Dragging { start_x: i32, start_y: i32, last_x: i32, last_y: i32 },
}

#[derive(Clone, Copy, Default)]
struct Sample { x: i32, y: i32, tick: u64 }

struct Slot {
    id:        u8,
    state:     PointerState,
    history:   [Sample; HISTORY_LEN],
    hist_head: usize,
    hist_len:  usize,
}

impl Slot {
    const fn new(id: u8) -> Self {
        Self {
            id, state: PointerState::Idle,
            history: [Sample { x: 0, y: 0, tick: 0 }; HISTORY_LEN],
            hist_head: 0, hist_len: 0,
        }
    }

    fn push(&mut self, x: i32, y: i32, tick: u64) {
        self.history[self.hist_head % HISTORY_LEN] = Sample { x, y, tick };
        self.hist_head += 1;
        if self.hist_len < HISTORY_LEN { self.hist_len += 1; }
    }

    fn velocity(&self) -> (i32, i32) {
        if self.hist_len < 2 { return (0, 0); }
        let oldest = self.history[self.hist_head.wrapping_sub(self.hist_len) % HISTORY_LEN];
        let newest = self.history[self.hist_head.wrapping_sub(1)             % HISTORY_LEN];
        let dt = newest.tick.saturating_sub(oldest.tick).max(1) as i32;
        ((newest.x - oldest.x) / dt, (newest.y - oldest.y) / dt)
    }

    fn max_movement(&self, down_x: i32, down_y: i32, x: i32, y: i32) -> u32 {
        ((x - down_x).unsigned_abs()).max((y - down_y).unsigned_abs())
    }
}

struct GestureQueue {
    events: [Option<GestureEvent>; MAX_GESTURE_QUEUE],
    head:   usize,
    tail:   usize,
    count:  usize,
}

impl GestureQueue {
    const fn new() -> Self {
        Self { events: [None; MAX_GESTURE_QUEUE], head: 0, tail: 0, count: 0 }
    }

    fn push(&mut self, ev: GestureEvent) -> bool {
        if self.count >= MAX_GESTURE_QUEUE { return false; }
        self.events[self.tail % MAX_GESTURE_QUEUE] = Some(ev);
        self.tail += 1; self.count += 1;
        true
    }

    fn pop(&mut self) -> Option<GestureEvent> {
        if self.count == 0 { return None; }
        let ev = self.events[self.head % MAX_GESTURE_QUEUE].take()?;
        self.head += 1; self.count -= 1;
        Some(ev)
    }
}

pub struct GestureRecognizer {
    slots:  [Slot; MAX_POINTERS],
    queue:  GestureQueue,
    pub tick: u64,
}

impl GestureRecognizer {
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|i| Slot::new(i as u8)),
            queue: GestureQueue::new(),
            tick:  0,
        }
    }

    pub fn advance_tick(&mut self, delta: u64) {
        self.tick += delta;
        for slot in self.slots.iter_mut() {
            if let PointerState::Pressed { down_x, down_y, down_tick } = slot.state {
                if self.tick.saturating_sub(down_tick) >= HOLD_TICKS {
                    slot.state = PointerState::Holding { x: down_x, y: down_y };
                    let id = slot.id;
                    self.queue.push(GestureEvent::Hold { x: down_x, y: down_y, pointer_id: id });
                }
            }
        }
    }

    pub fn feed(&mut self, event: InputEvent) {
        match event {
            InputEvent::TouchDown { x, y, .. } |
            InputEvent::PointerAbs { x, y, .. } if matches!(event,
                InputEvent::PointerAbs { buttons, .. } if buttons & 1 != 0) =>
            {
                let id = if let InputEvent::TouchDown { id, .. } = event { id } else { 0 };
                self.on_down(id, x, y);
            }
            InputEvent::PointerAbs { x, y, buttons } => {
                if buttons & 1 != 0 { self.on_down(0, x, y); }
                else                { self.on_move(0, x, y); }
            }
            InputEvent::TouchMove { id, x, y } => self.on_move(id, x, y),
            InputEvent::TouchUp   { id, x, y } => self.on_up(id, x, y),
            InputEvent::Scroll { axis: 0, value, .. } => {
                let (px, py) = self.cursor_pos();
                self.queue.push(GestureEvent::ScrollV { value, x: px, y: py });
            }
            InputEvent::Scroll { axis: 1, value, .. } => {
                let (px, py) = self.cursor_pos();
                self.queue.push(GestureEvent::ScrollH { value, x: px, y: py });
            }
            _ => {}
        }
    }

    fn cursor_pos(&self) -> (i32, i32) {
        // Use slot 0's last known position
        if self.slots[0].hist_len > 0 {
            let s = self.slots[0].history[self.slots[0].hist_head.wrapping_sub(1) % HISTORY_LEN];
            (s.x, s.y)
        } else { (0, 0) }
    }

    fn on_down(&mut self, id: u8, x: i32, y: i32) {
        let tick = self.tick;
        if let Some(slot) = self.slots.iter_mut().find(|s| s.id == id) {
            slot.state = PointerState::Pressed { down_x: x, down_y: y, down_tick: tick };
            slot.hist_len = 0; slot.hist_head = 0;
            slot.push(x, y, tick);
        }
    }

    fn on_move(&mut self, id: u8, x: i32, y: i32) {
        let tick = self.tick;
        let queue = &mut self.queue;
        if let Some(slot) = self.slots.iter_mut().find(|s| s.id == id) {
            slot.push(x, y, tick);
            match slot.state {
                PointerState::Pressed { down_x, down_y, .. } => {
                    if slot.max_movement(down_x, down_y, x, y) >= DRAG_THRESHOLD {
                        slot.state = PointerState::Dragging {
                            start_x: down_x, start_y: down_y, last_x: x, last_y: y,
                        };
                        queue.push(GestureEvent::DragBegin { x, y, pointer_id: id });
                    }
                }
                PointerState::Dragging { ref mut last_x, ref mut last_y, .. } => {
                    let (dx, dy) = (x - *last_x, y - *last_y);
                    *last_x = x; *last_y = y;
                    queue.push(GestureEvent::DragUpdate { dx, dy, x, y, pointer_id: id });
                }
                _ => {}
            }
        }
    }

    fn on_up(&mut self, id: u8, x: i32, y: i32) {
        let tick = self.tick;
        let queue = &mut self.queue;
        if let Some(slot) = self.slots.iter_mut().find(|s| s.id == id) {
            slot.push(x, y, tick);
            match slot.state {
                PointerState::Pressed { down_x, down_y, down_tick } => {
                    let moved = slot.max_movement(down_x, down_y, x, y);
                    let held  = tick.saturating_sub(down_tick);
                    if moved <= TAP_MAX_PIXELS && held <= TAP_MAX_TICKS {
                        queue.push(GestureEvent::Tap { x, y, pointer_id: id });
                    }
                }
                PointerState::Dragging { start_x, start_y, .. } => {
                    let (total_dx, total_dy) = (x - start_x, y - start_y);
                    queue.push(GestureEvent::DragEnd { x, y, total_dx, total_dy, pointer_id: id });
                    let (vx, vy) = slot.velocity();
                    let speed = vx.unsigned_abs().max(vy.unsigned_abs());
                    if speed >= SWIPE_VELOCITY {
                        let direction = if vx.abs() > vy.abs() {
                            if vx > 0 { SwipeDirection::Right } else { SwipeDirection::Left }
                        } else {
                            if vy > 0 { SwipeDirection::Down } else { SwipeDirection::Up }
                        };
                        queue.push(GestureEvent::Swipe { direction, velocity: speed, pointer_id: id });
                    }
                }
                _ => {}
            }
            slot.state = PointerState::Idle;
        }
    }

    pub fn poll(&mut self) -> Option<GestureEvent> { self.queue.pop() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_on_short_press_release() {
        let mut r = GestureRecognizer::new();
        r.feed(InputEvent::TouchDown { id: 0, x: 100, y: 100 });
        r.advance_tick(5);
        r.feed(InputEvent::TouchUp { id: 0, x: 101, y: 100 });
        assert!(matches!(r.poll(), Some(GestureEvent::Tap { .. })));
    }

    #[test]
    fn drag_emits_begin_update_end() {
        let mut r = GestureRecognizer::new();
        r.feed(InputEvent::TouchDown { id: 0, x: 0, y: 0 });
        for i in 1..=10 {
            r.advance_tick(1);
            r.feed(InputEvent::TouchMove { id: 0, x: i * 10, y: 0 });
        }
        r.feed(InputEvent::TouchUp { id: 0, x: 100, y: 0 });
        let mut begin = false; let mut end = false;
        while let Some(ev) = r.poll() {
            match ev {
                GestureEvent::DragBegin { .. } => begin = true,
                GestureEvent::DragEnd   { .. } => end   = true,
                _ => {}
            }
        }
        assert!(begin && end);
    }

    #[test]
    fn fast_horizontal_drag_emits_swipe_right() {
        let mut r = GestureRecognizer::new();
        r.feed(InputEvent::TouchDown { id: 0, x: 0, y: 0 });
        for i in 1..=10 { r.advance_tick(1); r.feed(InputEvent::TouchMove { id: 0, x: i * 10, y: 0 }); }
        r.feed(InputEvent::TouchUp { id: 0, x: 100, y: 0 });
        let mut got = false;
        while let Some(ev) = r.poll() {
            if matches!(ev, GestureEvent::Swipe { direction: SwipeDirection::Right, .. }) { got = true; }
        }
        assert!(got);
    }
}
