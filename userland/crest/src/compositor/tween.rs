// userland/crest/src/compositor/tween.rs
//
// TWEEN ENGINE — fixed-point animation interpolator
//
// Tween<Fixed>: animates from `start` to `end` over `duration_ticks`.
// Easing functions: Linear, EaseInOutCubic, Spring (approximated).
// TweenBank: fixed-size array of active tweens, keyed by target_id (u32).
//
// Usage:
//   bank.start(id, start, end, duration, EasingFn::EaseInOutCubic);
//   loop { let v = bank.tick_and_sample(id, 1); write to layout }
//
// Spring approximation:
//   Uses underdamped harmonic: value approaches target with overshoot.
//   Modeled as: x(t) = end - (end-start) * e^(-decay*t) * cos(omega*t)
//   Approximated in fixed-point via iterative: v += (target - pos) * k - v * damping
//   k = stiffness (Fixed), damping = Fixed, both tunable per tween.

#![allow(dead_code)]

use crate::obsidian::Fixed;

pub const MAX_TWEENS: usize = 32;

// ─────────────────────────────────────────────
// EASING FUNCTIONS
// ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EasingFn {
    Linear,
    EaseInOutCubic,
    Spring { stiffness_raw: i32, damping_raw: i32 },
}

/// Compute eased value at progress t ∈ [0, ONE_RAW] (Fixed::ONE = 1.0)
/// Returns a Fixed in [0, Fixed::ONE] (linear) or outside for spring overshoot
fn ease(t: Fixed, easing: EasingFn) -> Fixed {
    match easing {
        EasingFn::Linear => t,
        EasingFn::EaseInOutCubic => ease_in_out_cubic(t),
        EasingFn::Spring { .. } => t, // spring is stateful — handled separately
    }
}

/// Smooth step cubic: 3t²-2t³
fn ease_in_out_cubic(t: Fixed) -> Fixed {
    // 3t² - 2t³
    let t2 = t.multiply(t);
    let t3 = t2.multiply(t);
    let three = Fixed::from_integer(3);
    let two   = Fixed::from_integer(2);
    three.multiply(t2).saturating_sub(two.multiply(t3))
}

// ─────────────────────────────────────────────
// TWEEN STATE
// ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct Tween {
    pub target_id:      u32,
    pub start:          Fixed,
    pub end:            Fixed,
    pub duration_ticks: u32,
    pub elapsed_ticks:  u32,
    pub easing:         EasingFn,
    pub active:         bool,
    // Spring state (only used when easing == Spring)
    pub spring_pos:     Fixed,
    pub spring_vel:     Fixed,
}

impl Tween {
    const INACTIVE: Self = Self {
        target_id: 0,
        start: Fixed::ZERO,
        end: Fixed::ZERO,
        duration_ticks: 0,
        elapsed_ticks: 0,
        easing: EasingFn::Linear,
        active: false,
        spring_pos: Fixed::ZERO,
        spring_vel: Fixed::ZERO,
    };

    pub fn new(
        target_id: u32,
        start: Fixed,
        end: Fixed,
        duration_ticks: u32,
        easing: EasingFn,
    ) -> Self {
        Self {
            target_id,
            start,
            end,
            duration_ticks: duration_ticks.max(1),
            elapsed_ticks: 0,
            easing,
            active: true,
            spring_pos: start,
            spring_vel: Fixed::ZERO,
        }
    }

    /// Advance by `delta_ticks`, return current interpolated value.
    pub fn tick(&mut self, delta_ticks: u32) -> Fixed {
        if !self.active { return self.end; }
        self.elapsed_ticks = self.elapsed_ticks.saturating_add(delta_ticks);

        match self.easing {
            EasingFn::Spring { stiffness_raw, damping_raw } => {
                self.tick_spring(stiffness_raw, damping_raw, delta_ticks)
            }
            _ => {
                if self.elapsed_ticks >= self.duration_ticks {
                    self.active = false;
                    return self.end;
                }
                // t = elapsed / duration ∈ [0, 1]
                let t = match Fixed::from_ratio(
                    self.elapsed_ticks as i32,
                    self.duration_ticks as i32,
                ) {
                    Ok(v) => v,
                    Err(_) => Fixed::ONE,
                };
                let eased = ease(t, self.easing);
                let delta = self.end.saturating_sub(self.start);
                self.start.saturating_add(delta.multiply(eased))
            }
        }
    }

    fn tick_spring(&mut self, stiffness_raw: i32, damping_raw: i32, delta: u32) -> Fixed {
        let k        = Fixed::from_raw(stiffness_raw);
        let damping  = Fixed::from_raw(damping_raw);
        let target   = self.end;

        // Integrate for `delta` sub-steps (each step = 1 unit)
        for _ in 0..delta.min(64) { // cap at 64 sub-steps for safety
            let displacement = target.saturating_sub(self.spring_pos);
            let spring_force = displacement.multiply(k);
            let damping_force = self.spring_vel.multiply(damping);
            let accel = spring_force.saturating_sub(damping_force);
            self.spring_vel = self.spring_vel.saturating_add(accel);
            self.spring_pos = self.spring_pos.saturating_add(self.spring_vel);
        }

        // Settle when close enough: within 1/256 of target
        let epsilon = Fixed::from_raw(256);
        let diff = self.spring_pos.saturating_sub(target).abs();
        if diff <= epsilon && self.spring_vel.abs() <= epsilon {
            self.spring_pos = target;
            self.spring_vel = Fixed::ZERO;
            self.active = false;
        }

        self.spring_pos
    }

    pub fn is_complete(&self) -> bool { !self.active }
    pub fn current(&self) -> Fixed { if self.active { self.spring_pos } else { self.end } }
}

// ─────────────────────────────────────────────
// TWEEN BANK — manages multiple concurrent tweens
// ─────────────────────────────────────────────

pub struct TweenBank {
    tweens: [Tween; MAX_TWEENS],
    count:  usize,
}

impl TweenBank {
    pub const fn new() -> Self {
        Self { tweens: [Tween::INACTIVE; MAX_TWEENS], count: 0 }
    }

    /// Start a tween for `target_id`. Replaces any existing tween for same ID.
    pub fn start(
        &mut self,
        target_id: u32,
        start: Fixed,
        end: Fixed,
        duration_ticks: u32,
        easing: EasingFn,
    ) -> bool {
        // Replace existing
        for t in self.tweens[..self.count].iter_mut() {
            if t.target_id == target_id {
                *t = Tween::new(target_id, start, end, duration_ticks, easing);
                return true;
            }
        }
        // New slot
        if self.count >= MAX_TWEENS { return false; }
        self.tweens[self.count] = Tween::new(target_id, start, end, duration_ticks, easing);
        self.count += 1;
        true
    }

    /// Advance all tweens by `delta_ticks`.
    pub fn tick_all(&mut self, delta_ticks: u32) {
        for t in self.tweens[..self.count].iter_mut() {
            if t.active { t.tick(delta_ticks); }
        }
    }

    /// Sample current value of tween for `target_id`.
    pub fn sample(&self, target_id: u32) -> Option<Fixed> {
        self.tweens[..self.count].iter()
            .find(|t| t.target_id == target_id)
            .map(|t| t.current())
    }

    /// Remove completed tweens (compact the array)
    pub fn gc(&mut self) {
        let mut write = 0;
        for read in 0..self.count {
            if self.tweens[read].active {
                self.tweens[write] = self.tweens[read];
                write += 1;
            }
        }
        for i in write..self.count { self.tweens[i] = Tween::INACTIVE; }
        self.count = write;
    }

    pub fn active_count(&self) -> usize {
        self.tweens[..self.count].iter().filter(|t| t.active).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_tween_reaches_end_exactly() {
        let mut t = Tween::new(1, Fixed::ZERO, Fixed::ONE, 10, EasingFn::Linear);
        for _ in 0..9 { t.tick(1); }
        let final_v = t.tick(1);
        assert_eq!(final_v, Fixed::ONE);
        assert!(t.is_complete());
    }

    #[test]
    fn ease_in_out_cubic_midpoint_is_half() {
        let half = Fixed::from_ratio(1, 2).unwrap();
        let eased = ease_in_out_cubic(half);
        assert_eq!(eased, half); // 3*(0.5)^2 - 2*(0.5)^3 = 0.75 - 0.25 = 0.5 ✓
    }

    #[test]
    fn tween_bank_replaces_existing_tween_for_same_id() {
        let mut bank = TweenBank::new();
        bank.start(42, Fixed::ZERO, Fixed::ONE, 100, EasingFn::Linear);
        bank.start(42, Fixed::ONE, Fixed::from_integer(2), 50, EasingFn::Linear);
        // Should still be 1 tween, the replaced one
        assert_eq!(bank.active_count(), 1);
    }

    #[test]
    fn spring_tween_settles_near_target() {
        let stiffness = Fixed::from_ratio(1, 10).unwrap().raw();
        let damping   = Fixed::from_ratio(1, 4).unwrap().raw();
        let mut t = Tween::new(
            1,
            Fixed::ZERO,
            Fixed::ONE,
            500,
            EasingFn::Spring { stiffness_raw: stiffness, damping_raw: damping },
        );
        for _ in 0..500 { t.tick(1); }
        let diff = t.current().saturating_sub(Fixed::ONE).abs();
        // Should be within 1/64 of target
        assert!(diff.raw() < Fixed::ONE.raw() / 64);
    }
}
