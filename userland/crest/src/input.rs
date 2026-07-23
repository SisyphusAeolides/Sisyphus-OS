#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Key {
        code: u32,
        pressed: bool,
    },
    PointerMotion {
        delta_x: i32,
        delta_y: i32,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    PointerAbs {
        x: i32,
        y: i32,
        buttons: u8,
    },
    Scroll {
        axis: u8,
        value: i32,
        discrete: bool,
    },
    TouchDown {
        id: u8,
        x: i32,
        y: i32,
    },
    TouchMove {
        id: u8,
        x: i32,
        y: i32,
    },
    TouchUp {
        id: u8,
        x: i32,
        y: i32,
    },
    StylusDown {
        x: i32,
        y: i32,
        pressure: u16,
    },
    StylusMove {
        x: i32,
        y: i32,
        pressure: u16,
        tilt_x: i16,
        tilt_y: i16,
    },
    StylusUp {
        x: i32,
        y: i32,
    },
    PointerDown {
        id: u8,
        x: i32,
        y: i32,
    }, // added back so gesture.rs still compiles!
    PointerMove {
        id: u8,
        x: i32,
        y: i32,
    }, // added back so gesture.rs still compiles!
    PointerUp {
        id: u8,
        x: i32,
        y: i32,
    }, // added back so gesture.rs still compiles!
}

pub mod modifier {
    pub const SHIFT: u16 = 1 << 0;
    pub const CTRL: u16 = 1 << 1;
    pub const ALT: u16 = 1 << 2;
    pub const SUPER: u16 = 1 << 3;
    pub const CAPS: u16 = 1 << 4;
    pub const NUM: u16 = 1 << 5;
    pub const ALT_GR: u16 = 1 << 6;
    pub const HYPER: u16 = 1 << 7;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct ModifierState(pub u16);

impl ModifierState {
    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn has(self, mask: u16) -> bool {
        self.0 & mask != 0
    }
    pub const fn with(self, mask: u16) -> Self {
        Self(self.0 | mask)
    }
    pub const fn without(self, mask: u16) -> Self {
        Self(self.0 & !mask)
    }

    pub fn update(self, code: u32, pressed: bool) -> Self {
        use modifier::*;
        let bit = match code {
            42 | 54 => SHIFT,
            29 | 97 => CTRL,
            56 => ALT,
            100 => ALT_GR,
            125 | 126 => SUPER,
            58 => return if pressed { Self(self.0 ^ CAPS) } else { self },
            69 => return if pressed { Self(self.0 ^ NUM) } else { self },
            _ => return self,
        };
        if pressed {
            self.with(bit)
        } else {
            self.without(bit)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntegratedEvent {
    pub raw: InputEvent,
    pub modifiers: ModifierState,
    pub pointer_x: i32,
    pub pointer_y: i32,
}

pub const MAX_FILTERS: usize = 8;

pub trait InputFilter {
    fn filter(&mut self, event: InputEvent, modifiers: ModifierState) -> Option<InputEvent>;
}

pub const MAX_REMAPS: usize = 16;

#[derive(Clone, Copy, Default)]
pub struct KeyRemap {
    from: [u32; MAX_REMAPS],
    to: [u32; MAX_REMAPS],
    count: usize,
}

impl KeyRemap {
    pub const fn new() -> Self {
        Self {
            from: [0; MAX_REMAPS],
            to: [0; MAX_REMAPS],
            count: 0,
        }
    }

    pub fn add(&mut self, from: u32, to: u32) -> bool {
        if self.count >= MAX_REMAPS {
            return false;
        }
        self.from[self.count] = from;
        self.to[self.count] = to;
        self.count += 1;
        true
    }

    fn remap(&self, code: u32) -> u32 {
        (0..self.count)
            .find(|&i| self.from[i] == code)
            .map(|i| self.to[i])
            .unwrap_or(code)
    }
}

impl InputFilter for KeyRemap {
    fn filter(&mut self, event: InputEvent, _: ModifierState) -> Option<InputEvent> {
        Some(match event {
            InputEvent::Key { code, pressed } => InputEvent::Key {
                code: self.remap(code),
                pressed,
            },
            other => other,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PointerAbsIntegrator {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub buttons: u8,
    pub replace_motion: bool,
}

impl PointerAbsIntegrator {
    pub const fn new(width: i32, height: i32) -> Self {
        Self {
            x: width / 2,
            y: height / 2,
            width,
            height,
            buttons: 0,
            replace_motion: true,
        }
    }

    pub fn warp(&mut self, x: i32, y: i32) {
        self.x = x.clamp(0, self.width - 1);
        self.y = y.clamp(0, self.height - 1);
    }
}

impl InputFilter for PointerAbsIntegrator {
    fn filter(&mut self, event: InputEvent, _: ModifierState) -> Option<InputEvent> {
        match event {
            InputEvent::PointerMotion { delta_x, delta_y } => {
                self.x = (self.x + delta_x).clamp(0, self.width - 1);
                self.y = (self.y + delta_y).clamp(0, self.height - 1);
                if self.replace_motion {
                    Some(InputEvent::PointerAbs {
                        x: self.x,
                        y: self.y,
                        buttons: self.buttons,
                    })
                } else {
                    Some(event)
                }
            }
            InputEvent::PointerButton { button, pressed } => {
                let bit = 1u8 << (button.min(7) as u8);
                if pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                Some(InputEvent::PointerAbs {
                    x: self.x,
                    y: self.y,
                    buttons: self.buttons,
                })
            }
            other => Some(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModifierCapture {
    pub state: ModifierState,
}

impl InputFilter for ModifierCapture {
    fn filter(&mut self, event: InputEvent, _: ModifierState) -> Option<InputEvent> {
        if let InputEvent::Key { code, pressed } = event {
            self.state = self.state.update(code, pressed);
        }
        Some(event)
    }
}

pub struct InputIntegrator<'f> {
    modifiers: ModifierState,
    pointer_x: i32,
    pointer_y: i32,
    filters: [Option<&'f mut dyn InputFilter>; MAX_FILTERS],
    filter_count: usize,
}

impl<'f> InputIntegrator<'f> {
    pub fn new() -> Self {
        Self {
            modifiers: ModifierState::empty(),
            pointer_x: 0,
            pointer_y: 0,
            filters: [const { None }; MAX_FILTERS],
            filter_count: 0,
        }
    }

    pub fn add_filter(&mut self, f: &'f mut dyn InputFilter) -> bool {
        if self.filter_count >= MAX_FILTERS {
            return false;
        }
        self.filters[self.filter_count] = Some(f);
        self.filter_count += 1;
        true
    }

    pub fn integrate(&mut self, mut event: InputEvent) -> Option<IntegratedEvent> {
        if let InputEvent::Key { code, pressed } = event {
            self.modifiers = self.modifiers.update(code, pressed);
        }
        for i in 0..self.filter_count {
            if let Some(f) = self.filters[i].as_mut() {
                match f.filter(event, self.modifiers) {
                    Some(ev) => event = ev,
                    None => return None,
                }
            }
        }
        if let InputEvent::PointerAbs { x, y, .. } = event {
            self.pointer_x = x;
            self.pointer_y = y;
        }
        Some(IntegratedEvent {
            raw: event,
            modifiers: self.modifiers,
            pointer_x: self.pointer_x,
            pointer_y: self.pointer_y,
        })
    }

    pub const fn modifiers(&self) -> ModifierState {
        self.modifiers
    }
    pub const fn pointer_x(&self) -> i32 {
        self.pointer_x
    }
    pub const fn pointer_y(&self) -> i32 {
        self.pointer_y
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Keybind {
    pub code: u32,
    pub modifiers: u16,
}

impl Keybind {
    pub const fn new(code: u32, modifiers: u16) -> Self {
        Self { code, modifiers }
    }
    pub const fn plain(code: u32) -> Self {
        Self { code, modifiers: 0 }
    }

    pub fn matches(&self, event: &IntegratedEvent) -> bool {
        matches!(event.raw, InputEvent::Key { code, pressed: true } if
            code == self.code && event.modifiers.0 & self.modifiers == self.modifiers
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_state_tracks_shift() {
        let s = ModifierState::empty().update(42, true);
        assert!(s.has(modifier::SHIFT));
        assert!(!s.update(42, false).has(modifier::SHIFT));
    }

    #[test]
    fn caps_toggles_on_press_not_release() {
        let s = ModifierState::empty().update(58, true);
        assert!(s.has(modifier::CAPS));
        let s = s.update(58, false);
        assert!(s.has(modifier::CAPS)); // stays on after release
        let s = s.update(58, true);
        assert!(!s.has(modifier::CAPS)); // toggles off
    }

    #[test]
    fn remap_filter_remaps_caps_to_ctrl() {
        let mut r = KeyRemap::new();
        r.add(58, 29);
        assert_eq!(
            r.filter(
                InputEvent::Key {
                    code: 58,
                    pressed: true
                },
                ModifierState::empty()
            ),
            Some(InputEvent::Key {
                code: 29,
                pressed: true
            })
        );
    }

    #[test]
    fn abs_integrator_clamps_to_bounds() {
        let mut p = PointerAbsIntegrator::new(1920, 1080);
        p.warp(1900, 540);
        let r = p.filter(
            InputEvent::PointerMotion {
                delta_x: 9999,
                delta_y: 0,
            },
            ModifierState::empty(),
        );
        assert_eq!(
            r,
            Some(InputEvent::PointerAbs {
                x: 1919,
                y: 540,
                buttons: 0
            })
        );
    }

    #[test]
    fn keybind_matches_chord() {
        let bind = Keybind::new(28, modifier::SUPER);
        let ev = IntegratedEvent {
            raw: InputEvent::Key {
                code: 28,
                pressed: true,
            },
            modifiers: ModifierState(modifier::SUPER),
            pointer_x: 0,
            pointer_y: 0,
        };
        assert!(bind.matches(&ev));
    }
}
