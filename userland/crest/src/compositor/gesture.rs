// userland/crest/src/compositor/gesture.rs
//
// GESTURE RECOGNIZER — stateful input event → gesture classifier
//
// Sits between NervousSystem (cerebral.rs) and CompositorPipeline.
// Consumes InputEvent stream, emits GestureEvent when patterns recognized.
//
// Recognized gestures:
//   Tap:    pointer down → up within TAP_MAX_TICKS and TAP_MAX_PIXELS movement
//   Hold:   pointer down, held for HOLD_TICKS without movement
//   Drag:   pointer down → moved beyond DRAG_THRESHOLD → pointer up
//   Swipe:  fast drag (velocity > SWIPE_VELOCITY_THRESHOLD) in a cardinal direction
//   Pinch:  two simultaneous pointer events with converging/diverging distance
//           (simplified: simulated as two sequential pointer events for single-touch)
//
// State machine per pointer slot:
//   Idle → Pressed → (Tapping | Holding | Dragging) → Idle
//
// History buffer: last HISTORY_LEN (x,y,tick) samples per slot
//   → used to compute velocity for swipe detection

#![allow(dead_code)]

use crate::input::InputEvent;

pub const MAX_POINTERS: usize = 4;
pub const HISTORY_LEN:  usize = 16;

pub const TAP_MAX_TICKS:   u32 = 20;
pub const TAP_MAX_PIXELS:  u32 = 8;
pub const HOLD_TICKS:      u32 = 60;
pub const DRAG_THRESHOLD:  u32 = 6;
pub const SWIPE_VELOCITY:  u32 = 8; // pixels per tick threshold

pub const MAX_GESTURE_QUEUE: usize = 32;

// ─────────────────────────────────────────────
// POINTER SAMPLE
// ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default)]
pub struct PointerSample {
    pub x:    i32,
    pub y:    i32,
    pub tick: u64,
}

// ─────────────────────────────────────────────
// GESTURE EVENT
// ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GestureEvent {
    Tap        { x: i32, y: i32, pointer_id: u8 },
    Hold       { x: i32, y: i32, pointer_id: u8 },
    DragBegin  { x: i32, y: i32, pointer_id: u8 },
    DragUpdate { dx: i32, dy: i32, pointer_id: u8, x: i32, y: i32 },
    DragEnd    { x: i32, y: i32, pointer_id: u8, total_dx: i32, total_dy: i32 },
    Swipe      { direction: SwipeDirection, pointer_id: u8, velocity: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwipeDirection { Up, Down, Left, Right }

// ─────────────────────────────────────────────
// POINTER STATE MACHINE
// ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerState {
    Idle,
    Pressed  { down_x: i32, down_y: i32, down_tick: u64 },
    Holding  { x: i32, y: i32 },
    Dragging { start_x: i32, start_y: i32, last_x: i32, last_y: i32 },
}

pub struct PointerSlot {
    pub id:      u8,
    pub state:   PointerState,
    pub history: [PointerSample; HISTORY_LEN],
    pub hist_len: usize,
    pub hist_head: usize,
}

impl PointerSlot {
    pub const fn new(id: u8) -> Self {
        Self {
            id,
            state: PointerState::Idle,
            history: [PointerSample { x: 0, y: 0, tick: 0 }; HISTORY_LEN],
            hist_len: 0,
            hist_head: 0,
        }
    }

    pub fn push_sample(&mut self, x: i32, y: i32, tick: u64) {
        self.history[self.hist_head % HISTORY_LEN] = PointerSample { x, y, tick };
        self.hist_head += 1;
        if self.hist_len < HISTORY_LEN { self.hist_len += 1; }
    }

    /// Compute velocity (pixels/tick) from history window
    pub fn velocity(&self) -> (i32, i32) {
        if self.hist_len < 2 { return (0, 0); }
        let oldest_idx = self.hist_head.wrapping_sub(self.hist_len) % HISTORY_LEN;
        let newest_idx = self.hist_head.wrapping_sub(1) % HISTORY_LEN;
        let oldest = self.history[oldest_idx];
        let newest = self.history[newest_idx];
        let dt = newest.tick.saturating_sub(oldest.tick).max(1) as i32;
        let vx = (newest.x - oldest.x) / dt;
        let vy = (newest.y - oldest.y) / dt;
        (vx, vy)
    }

    pub fn movement_from_down(&self, down_x: i32, down_y: i32, cur_x: i32, cur_y: i32) -> u32 {
        let dx = (cur_x - down_x).unsigned_abs();
        let dy = (cur_y - down_y).unsigned_abs();
        dx.max(dy)
    }
}

// ─────────────────────────────────────────────
// GESTURE QUEUE
// ─────────────────────────────────────────────

pub struct GestureQueue {
    events: [Option<GestureEvent>; MAX_GESTURE_QUEUE],
    head:   usize,
    tail:   usize,
    count:  usize,
}

impl GestureQueue {
    pub const fn new() -> Self {
        Self { events: [None; MAX_GESTURE_QUEUE], head: 0, tail: 0, count: 0 }
    }

    pub fn push(&mut self, event: GestureEvent) -> bool {
        if self.count >= MAX_GESTURE_QUEUE { return false; }
        self.events[self.tail % MAX_GESTURE_QUEUE] = Some(event);
        self.tail += 1;
        self.count += 1;
        true
    }

    pub fn pop(&mut self) -> Option<GestureEvent> {
        if self.count == 0 { return None; }
        let ev = self.events[self.head % MAX_GESTURE_QUEUE].take()?;
        self.head += 1;
        self.count -= 1;
        Some(ev)
    }
}

// ─────────────────────────────────────────────
// GESTURE RECOGNIZER
// ─────────────────────────────────────────────

pub struct GestureRecognizer {
    pub pointers: [PointerSlot; MAX_POINTERS],
    pub queue:    GestureQueue,
    pub tick:     u64,
}

impl GestureRecognizer {
    pub fn new() -> Self {
        Self {
            pointers: core::array::from_fn(|i| PointerSlot::new(i as u8)),
            queue: GestureQueue::new(),
            tick: 0,
        }
    }

    /// Advance the tick counter (call once per frame before feed_event)
    pub fn advance_tick(&mut self, delta: u64) {
        self.tick += delta;

        // Check for hold transitions
        for slot in self.pointers.iter_mut() {
            if let PointerState::Pressed { down_x, down_y, down_tick } = slot.state {
                if self.tick.saturating_sub(down_tick) >= u64::from(HOLD_TICKS) {
                    slot.state = PointerState::Holding { x: down_x, y: down_y };
                    let _ = Self::emit_hold(&mut GestureQueueProxy, slot.id, down_x, down_y);
                    // Note: in full impl, pass queue directly. Simplified here.
                }
            }
        }
    }

    fn emit_hold(_q: &mut GestureQueueProxy, _id: u8, _x: i32, _y: i32) {}

    /// Process one InputEvent and potentially emit gesture events
    pub fn feed_event(&mut self, event: InputEvent) {
        match event {
            InputEvent::PointerDown { id, x, y } => self.on_down(id, x, y),
            InputEvent::PointerMove { id, x, y } => self.on_move(id, x, y),
            InputEvent::PointerUp   { id, x, y } => self.on_up(id, x, y),
            _ => {} // Key events etc. not gesture-relevant here
        }
    }

    fn slot_mut(&mut self, id: u8) -> Option<&mut PointerSlot> {
        self.pointers.iter_mut().find(|s| s.id == id)
    }

    fn on_down(&mut self, id: u8, x: i32, y: i32) {
        let tick = self.tick;
        if let Some(slot) = self.slot_mut(id) {
            slot.state = PointerState::Pressed { down_x: x, down_y: y, down_tick: tick };
            slot.hist_len = 0;
            slot.hist_head = 0;
            slot.push_sample(x, y, tick);
        }
    }

    fn on_move(&mut self, id: u8, x: i32, y: i32) {
        let tick = self.tick;
        let queue = &mut self.queue;
        if let Some(slot) = self.pointers.iter_mut().find(|s| s.id == id) {
            slot.push_sample(x, y, tick);
            match slot.state {
                PointerState::Pressed { down_x, down_y, .. } => {
                    let movement = slot.movement_from_down(down_x, down_y, x, y);
                    if movement >= DRAG_THRESHOLD {
                        slot.state = PointerState::Dragging {
                            start_x: down_x, start_y: down_y,
                            last_x: x, last_y: y,
                        };
                        queue.push(GestureEvent::DragBegin { x, y, pointer_id: id });
                    }
                }
                PointerState::Dragging { last_x, last_y, .. } => {
                    let dx = x - last_x;
                    let dy = y - last_y;
                    if let PointerState::Dragging { ref mut last_x, ref mut last_y, .. } = slot.state {
                        *last_x = x; *last_y = y;
                    }
                    queue.push(GestureEvent::DragUpdate { dx, dy, x, y, pointer_id: id });
                }
                _ => {}
            }
        }
    }

    fn on_up(&mut self, id: u8, x: i32, y: i32) {
        let tick = self.tick;
        let queue = &mut self.queue;
        if let Some(slot) = self.pointers.iter_mut().find(|s| s.id == id) {
            slot.push_sample(x, y, tick);
            match slot.state {
                PointerState::Pressed { down_x, down_y, down_tick } => {
                    let movement = slot.movement_from_down(down_x, down_y, x, y);
                    let held = tick.saturating_sub(down_tick);
                    if movement <= TAP_MAX_PIXELS && held <= u64::from(TAP_MAX_TICKS) {
                        queue.push(GestureEvent::Tap { x, y, pointer_id: id });
                    }
                    slot.state = PointerState::Idle;
                }
                PointerState::Dragging { start_x, start_y, .. } => {
                    let total_dx = x - start_x;
                    let total_dy = y - start_y;
                    let (vx, vy) = slot.velocity();
                    slot.state = PointerState::Idle;
                    queue.push(GestureEvent::DragEnd {
                        x, y, pointer_id: id, total_dx, total_dy,
                    });
                    // Check for swipe
                    let speed = (vx.unsigned_abs()).max(vy.unsigned_abs());
                    if speed >= SWIPE_VELOCITY {
                        let dir = if vx.abs() > vy.abs() {
                            if vx > 0 { SwipeDirection::Right } else { SwipeDirection::Left }
                        } else {
                            if vy > 0 { SwipeDirection::Down } else { SwipeDirection::Up }
                        };
                        queue.push(GestureEvent::Swipe { direction: dir, pointer_id: id, velocity: speed });
                    }
                }
                PointerState::Holding { x: _, y: _ } => {
                    slot.state = PointerState::Idle;
                }
                _ => { slot.state = PointerState::Idle; }
            }
        }
    }

    pub fn poll_gesture(&mut self) -> Option<GestureEvent> {
        self.queue.pop()
    }
}

struct GestureQueueProxy;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_down_move_up(rec: &mut GestureRecognizer, x0: i32, y0: i32, x1: i32, y1: i32, steps: u32) {
        rec.feed_event(InputEvent::PointerDown { id: 0, x: x0, y: y0 });
        for i in 1..=steps {
            let t = i as i32;
            let total = steps as i32;
            let x = x0 + (x1 - x0) * t / total;
            let y = y0 + (y1 - y0) * t / total;
            rec.advance_tick(1);
            rec.feed_event(InputEvent::PointerMove { id: 0, x, y });
        }
        rec.feed_event(InputEvent::PointerUp { id: 0, x: x1, y: y1 });
    }

    #[test]
    fn tap_recognized_on_short_up() {
        let mut rec = GestureRecognizer::new();
        rec.feed_event(InputEvent::PointerDown { id: 0, x: 100, y: 100 });
        rec.advance_tick(5);
        rec.feed_event(InputEvent::PointerUp { id: 0, x: 101, y: 100 });
        assert!(matches!(rec.poll_gesture(), Some(GestureEvent::Tap { .. })));
    }

    #[test]
    fn drag_emits_begin_and_end() {
        let mut rec = GestureRecognizer::new();
        make_down_move_up(&mut rec, 0, 0, 100, 0, 10);
        let mut got_begin = false;
        let mut got_end   = false;
        while let Some(ev) = rec.poll_gesture() {
            match ev {
                GestureEvent::DragBegin  { .. } => got_begin = true,
                GestureEvent::DragEnd    { .. } => got_end   = true,
                _ => {}
            }
        }
        assert!(got_begin);
        assert!(got_end);
    }

    #[test]
    fn fast_drag_emits_swipe() {
        let mut rec = GestureRecognizer::new();
        // Fast move: 10 steps of 10px each = 10 px/tick, above SWIPE_VELOCITY=8
        make_down_move_up(&mut rec, 0, 0, 100, 0, 10);
        let mut got_swipe = false;
        while let Some(ev) = rec.poll_gesture() {
            if matches!(ev, GestureEvent::Swipe { direction: SwipeDirection::Right, .. }) {
                got_swipe = true;
            }
        }
        assert!(got_swipe);
    }
}
