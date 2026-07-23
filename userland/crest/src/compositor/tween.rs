// TWEEN ENGINE — fixed-point animation interpolator
//
// Tween: animates a Fixed value from `start` to `end` over `duration_ticks`.
// Easing: Linear, EaseInOutCubic, Spring (iterative underdamped harmonic).
// TweenBank: up to MAX_TWEENS concurrent tweens keyed by u32 target_id.
// Usage: bank.start(id, from, to, ticks, EasingFn::EaseInOutCubic);
//        loop { let v = bank.sample(id).unwrap(); }

use crate::obsidian::Fixed;

pub const MAX_TWEENS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EasingFn {
    Linear,
    EaseInOutCubic,
    Spring {
        stiffness_raw: i32,
        damping_raw: i32,
    },
}

fn ease_in_out_cubic(t: Fixed) -> Fixed {
    // 3t² - 2t³
    let t2 = t.multiply(t);
    let t3 = t2.multiply(t);
    Fixed::from_integer(3)
        .multiply(t2)
        .saturating_sub(Fixed::from_integer(2).multiply(t3))
}

#[derive(Clone, Copy, Debug)]
pub struct Tween {
    pub target_id: u32,
    pub start: Fixed,
    pub end: Fixed,
    pub duration_ticks: u32,
    pub elapsed_ticks: u32,
    pub easing: EasingFn,
    pub active: bool,
    pub spring_pos: Fixed,
    pub spring_vel: Fixed,
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

    pub fn tick(&mut self, delta: u32) -> Fixed {
        if !self.active {
            return self.end;
        }
        self.elapsed_ticks = self.elapsed_ticks.saturating_add(delta);
        match self.easing {
            EasingFn::Spring {
                stiffness_raw,
                damping_raw,
            } => self.tick_spring(stiffness_raw, damping_raw, delta),
            _ => {
                if self.elapsed_ticks >= self.duration_ticks {
                    self.active = false;
                    return self.end;
                }
                let t = Fixed::from_ratio(self.elapsed_ticks as i32, self.duration_ticks as i32)
                    .unwrap_or(Fixed::ONE);
                let eased = match self.easing {
                    EasingFn::Linear => t,
                    EasingFn::EaseInOutCubic => ease_in_out_cubic(t),
                    _ => t,
                };
                self.start
                    .saturating_add(self.end.saturating_sub(self.start).multiply(eased))
            }
        }
    }

    fn tick_spring(&mut self, stiffness_raw: i32, damping_raw: i32, delta: u32) -> Fixed {
        let k = Fixed::from_raw(stiffness_raw);
        let damping = Fixed::from_raw(damping_raw);
        for _ in 0..delta.min(64) {
            let force = self.end.saturating_sub(self.spring_pos).multiply(k);
            let drag = self.spring_vel.multiply(damping);
            let accel = force.saturating_sub(drag);
            self.spring_vel = self.spring_vel.saturating_add(accel);
            self.spring_pos = self.spring_pos.saturating_add(self.spring_vel);
        }
        let eps = Fixed::from_raw(256);
        if self.spring_pos.saturating_sub(self.end).abs() <= eps && self.spring_vel.abs() <= eps {
            self.spring_pos = self.end;
            self.spring_vel = Fixed::ZERO;
            self.active = false;
        }
        self.spring_pos
    }

    pub fn is_complete(&self) -> bool {
        !self.active
    }
    pub fn current(&self) -> Fixed {
        if self.active {
            self.spring_pos
        } else {
            self.end
        }
    }
}

pub struct TweenBank {
    tweens: [Tween; MAX_TWEENS],
    count: usize,
}

impl TweenBank {
    pub const fn new() -> Self {
        Self {
            tweens: [Tween::INACTIVE; MAX_TWEENS],
            count: 0,
        }
    }

    pub fn start(
        &mut self,
        target_id: u32,
        start: Fixed,
        end: Fixed,
        duration_ticks: u32,
        easing: EasingFn,
    ) -> bool {
        for t in self.tweens[..self.count].iter_mut() {
            if t.target_id == target_id {
                *t = Tween::new(target_id, start, end, duration_ticks, easing);
                return true;
            }
        }
        if self.count >= MAX_TWEENS {
            return false;
        }
        self.tweens[self.count] = Tween::new(target_id, start, end, duration_ticks, easing);
        self.count += 1;
        true
    }

    pub fn tick_all(&mut self, delta: u32) {
        for t in self.tweens[..self.count].iter_mut() {
            if t.active {
                t.tick(delta);
            }
        }
    }

    pub fn sample(&self, target_id: u32) -> Option<Fixed> {
        self.tweens[..self.count]
            .iter()
            .find(|t| t.target_id == target_id)
            .map(|t| t.current())
    }

    pub fn gc(&mut self) {
        let mut w = 0;
        for r in 0..self.count {
            if self.tweens[r].active {
                self.tweens[w] = self.tweens[r];
                w += 1;
            }
        }
        for i in w..self.count {
            self.tweens[i] = Tween::INACTIVE;
        }
        self.count = w;
    }

    pub fn active_count(&self) -> usize {
        self.tweens[..self.count]
            .iter()
            .filter(|t| t.active)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_reaches_end() {
        let mut t = Tween::new(1, Fixed::ZERO, Fixed::ONE, 10, EasingFn::Linear);
        for _ in 0..10 {
            t.tick(1);
        }
        assert_eq!(t.current(), Fixed::ONE);
        assert!(t.is_complete());
    }

    #[test]
    fn ease_in_out_midpoint_is_half() {
        let half = Fixed::from_ratio(1, 2).unwrap();
        let eased = ease_in_out_cubic(half);
        assert_eq!(eased, half);
    }

    #[test]
    fn bank_replaces_existing_tween_for_same_id() {
        let mut bank = TweenBank::new();
        bank.start(1, Fixed::ZERO, Fixed::ONE, 100, EasingFn::Linear);
        bank.start(1, Fixed::ONE, Fixed::from_integer(2), 50, EasingFn::Linear);
        assert_eq!(bank.active_count(), 1);
    }

    #[test]
    fn spring_settles_near_target() {
        let k = Fixed::from_ratio(1, 10).unwrap().raw();
        let d = Fixed::from_ratio(1, 4).unwrap().raw();
        let mut t = Tween::new(
            1,
            Fixed::ZERO,
            Fixed::ONE,
            500,
            EasingFn::Spring {
                stiffness_raw: k,
                damping_raw: d,
            },
        );
        for _ in 0..500 {
            t.tick(1);
        }
        assert!(t.current().saturating_sub(Fixed::ONE).abs().raw() < Fixed::ONE.raw() / 64);
    }
}
