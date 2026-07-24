// CREST SETTINGS PANEL
//
// A runtime-configurable key-value store for all DE preferences.
// No heap. All strings stored as FNV-1a hashes + fixed-length byte arrays.
// Persisted via a flat binary format written to a slope file descriptor.
//
// Sections:
//   Appearance  — panel height, colors, accent color, opacity
//   Keybinds    — up to MAX_KEYBINDS (action → Keybind)
//   Input       — pointer speed, scroll direction, tap-to-click
//   Display     — refresh rate hint, vsync mode, gamma
//   System      — crash behavior, thermal throttle threshold
//
// SettingsStore: the main type. Access via strongly-typed getters/setters.
// SettingsDirty: bitmask of which sections changed (for targeted re-render).
// Serializer: writes the store to a &mut [u8] in a compact binary format.
// Deserializer: reads it back — validates magic + version before applying.

use crate::input::{Keybind, modifier};

pub const MAX_KEYBINDS: usize = 32;
pub const SETTINGS_MAGIC: u32 = 0xC4E5_7077; // "CREST" + version hint
pub const SETTINGS_VERSION: u8 = 1;
pub const SERIALIZED_MAX_BYTES: usize = 512;

// ─── ACCENT COLOR ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccentColor(pub [u8; 4]);

impl AccentColor {
    pub const CORINTH_BLUE: Self = Self([60, 120, 220, 255]);
    pub const ORBITAL_GREEN: Self = Self([40, 200, 120, 255]);
    pub const OBSIDIAN_PURPLE: Self = Self([120, 60, 200, 255]);
    pub const RETINAL_RED: Self = Self([220, 60, 40, 255]);
    pub const SOLAR_GOLD: Self = Self([220, 180, 40, 255]);
}

// ─── APPEARANCE SETTINGS ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct AppearanceSettings {
    pub panel_height: u8, // pixels [16, 48]
    pub panel_at_bottom: bool,
    pub panel_opacity: u8, // 0=transparent, 255=opaque
    pub accent: AccentColor,
    pub blur_radius: u8, // reserved background-blur radius in pixels
    pub animations_on: bool,
    pub tween_speed: u8, // 1=slow, 10=instant
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            panel_height: 32,
            panel_at_bottom: false,
            panel_opacity: 240,
            accent: AccentColor::CORINTH_BLUE,
            blur_radius: 0,
            animations_on: true,
            tween_speed: 5,
        }
    }
}

// ─── KEYBIND SETTINGS ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ShellAction {
    OpenLauncher = 0,
    CloseLauncher = 1,
    OpenSettings = 2,
    CloseSettings = 3,
    FocusNext = 4,
    FocusPrev = 5,
    CloseApp = 6,
    ToggleFullscreen = 7,
    Screenshot = 8,
    LockScreen = 9,
    ShowDesktop = 10,
    VolumeUp = 11,
    VolumeDown = 12,
    BrightnessUp = 13,
    BrightnessDown = 14,
    WorkspaceNext = 15,
    WorkspacePrev = 16,
    Custom(u8),
}

#[derive(Clone, Copy, Debug)]
pub struct KeybindEntry {
    pub action: ShellAction,
    pub keybind: Keybind,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct KeybindSettings {
    pub entries: [KeybindEntry; MAX_KEYBINDS],
    pub count: usize,
}

impl Default for KeybindSettings {
    fn default() -> Self {
        use modifier::*;
        let mut s = Self {
            entries: [KeybindEntry {
                action: ShellAction::Custom(0),
                keybind: Keybind::plain(0),
                enabled: false,
            }; MAX_KEYBINDS],
            count: 0,
        };
        // Default bindings
        let defaults: &[(ShellAction, u32, u16)] = &[
            (ShellAction::OpenLauncher, 125, SUPER),        // Super
            (ShellAction::OpenSettings, 57, SUPER | SHIFT), // Super+Shift+Space
            (ShellAction::FocusNext, 15, ALT),              // Alt+Tab
            (ShellAction::FocusPrev, 15, ALT | SHIFT),      // Alt+Shift+Tab
            (ShellAction::CloseApp, 16, ALT),               // Alt+Q
            (ShellAction::ToggleFullscreen, 33, 0), // F (no modifier; deterministic default binding)
            (ShellAction::Screenshot, 99, 0),       // PrintScreen
            (ShellAction::WorkspaceNext, 78, SUPER), // Super+Right
            (ShellAction::WorkspacePrev, 75, SUPER), // Super+Left
            (ShellAction::VolumeUp, 115, 0),
            (ShellAction::VolumeDown, 114, 0),
            (ShellAction::BrightnessUp, 232, 0),
            (ShellAction::BrightnessDown, 233, 0),
        ];
        for &(action, code, mods) in defaults {
            if s.count < MAX_KEYBINDS {
                s.entries[s.count] = KeybindEntry {
                    action,
                    keybind: Keybind::new(code, mods),
                    enabled: true,
                };
                s.count += 1;
            }
        }
        s
    }
}

impl KeybindSettings {
    pub fn bind(&mut self, action: ShellAction, keybind: Keybind) -> bool {
        for e in self.entries[..self.count].iter_mut() {
            if e.action == action {
                e.keybind = keybind;
                e.enabled = true;
                return true;
            }
        }
        if self.count >= MAX_KEYBINDS {
            return false;
        }
        self.entries[self.count] = KeybindEntry {
            action,
            keybind,
            enabled: true,
        };
        self.count += 1;
        true
    }

    pub fn lookup(&self, action: ShellAction) -> Option<Keybind> {
        self.entries[..self.count]
            .iter()
            .find(|e| e.action == action && e.enabled)
            .map(|e| e.keybind)
    }
}

// ─── INPUT SETTINGS ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct InputSettings {
    pub pointer_speed: u8, // 1–10
    pub natural_scroll: bool,
    pub tap_to_click: bool,
    pub pointer_accel: bool,
    pub double_tap_ms: u16, // tap interval threshold in ms
    pub hold_threshold_ms: u16,
    pub drag_threshold_px: u8,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            pointer_speed: 5,
            natural_scroll: false,
            tap_to_click: true,
            pointer_accel: true,
            double_tap_ms: 300,
            hold_threshold_ms: 500,
            drag_threshold_px: 6,
        }
    }
}

// ─── DISPLAY SETTINGS ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VsyncMode {
    Off,
    Adaptive,
    On,
}

#[derive(Clone, Copy, Debug)]
pub struct DisplaySettings {
    pub refresh_hz: u8, // hint for kernel display broker
    pub vsync: VsyncMode,
    pub gamma: u8, // 100 = 1.0, 220 = 2.2
    pub hdr: bool,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            refresh_hz: 60,
            vsync: VsyncMode::Adaptive,
            gamma: 220,
            hdr: false,
        }
    }
}

// ─── SYSTEM SETTINGS ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashBehavior {
    Ignore,
    ShowNotification,
    RestartShell,
}

#[derive(Clone, Copy, Debug)]
pub struct SystemSettings {
    pub thermal_throttle_mc: u32, // millicelsius threshold to begin throttling
    pub crash_behavior: CrashBehavior,
    pub auto_lock_minutes: u8, // 0 = disabled
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self {
            thermal_throttle_mc: 85_000,
            crash_behavior: CrashBehavior::ShowNotification,
            auto_lock_minutes: 0,
        }
    }
}

// ─── DIRTY BITMASK ─────────────────────────────────────────────────────────

pub mod dirty {
    pub const APPEARANCE: u8 = 1 << 0;
    pub const KEYBINDS: u8 = 1 << 1;
    pub const INPUT: u8 = 1 << 2;
    pub const DISPLAY: u8 = 1 << 3;
    pub const SYSTEM: u8 = 1 << 4;
    pub const ALL: u8 = 0x1F;
}

// ─── SETTINGS STORE ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct SettingsStore {
    pub appearance: AppearanceSettings,
    pub keybinds: KeybindSettings,
    pub input: InputSettings,
    pub display: DisplaySettings,
    pub system: SystemSettings,
    pub dirty: u8,
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self {
            appearance: AppearanceSettings::default(),
            keybinds: KeybindSettings::default(),
            input: InputSettings::default(),
            display: DisplaySettings::default(),
            system: SystemSettings::default(),
            dirty: dirty::ALL,
        }
    }
}

impl SettingsStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_accent(&mut self, color: AccentColor) {
        self.appearance.accent = color;
        self.dirty |= dirty::APPEARANCE;
    }

    pub fn set_panel_height(&mut self, h: u8) {
        self.appearance.panel_height = h.clamp(16, 48);
        self.dirty |= dirty::APPEARANCE;
    }

    pub fn set_animations(&mut self, on: bool) {
        self.appearance.animations_on = on;
        self.dirty |= dirty::APPEARANCE;
    }

    pub fn bind_action(&mut self, action: ShellAction, keybind: Keybind) -> bool {
        let ok = self.keybinds.bind(action, keybind);
        if ok {
            self.dirty |= dirty::KEYBINDS;
        }
        ok
    }

    pub fn set_pointer_speed(&mut self, speed: u8) {
        self.input.pointer_speed = speed.clamp(1, 10);
        self.dirty |= dirty::INPUT;
    }

    pub fn set_natural_scroll(&mut self, on: bool) {
        self.input.natural_scroll = on;
        self.dirty |= dirty::INPUT;
    }

    pub fn set_vsync(&mut self, mode: VsyncMode) {
        self.display.vsync = mode;
        self.dirty |= dirty::DISPLAY;
    }

    pub fn set_thermal_throttle(&mut self, mc: u32) {
        self.system.thermal_throttle_mc = mc;
        self.dirty |= dirty::SYSTEM;
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = 0;
    }
    pub fn is_dirty(&self, section: u8) -> bool {
        self.dirty & section != 0
    }

    // ─── Serializer (flat binary, no alloc) ───────────────────────────────
    // Format:
    //   [0..4]   magic  (u32 LE)
    //   [4]      version (u8)
    //   [5]      panel_height
    //   [6]      panel_at_bottom
    //   [7]      panel_opacity
    //   [8..12]  accent color [r,g,b,a]
    //   [12]     animations_on
    //   [13]     tween_speed
    //   [14]     pointer_speed
    //   [15]     natural_scroll
    //   [16]     tap_to_click
    //   [17]     pointer_accel
    //   [18..20] double_tap_ms (u16 LE)
    //   [20..22] hold_threshold_ms (u16 LE)
    //   [22]     drag_threshold_px
    //   [23]     refresh_hz
    //   [24]     vsync (0=off,1=adaptive,2=on)
    //   [25]     gamma
    //   [26]     hdr
    //   [27..31] thermal_throttle_mc (u32 LE)
    //   [31]     crash_behavior (0/1/2)
    //   [32]     auto_lock_minutes
    //   [33]     keybind_count
    //   [34..]   keybind entries: [action:u8, code:u32 LE, mods:u16 LE, enabled:u8] × N

    pub fn serialize(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < 34 {
            return None;
        }
        // magic bytes replaced below
        const MAGIC: u32 = 0xC4E57077;
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = SETTINGS_VERSION;
        buf[5] = self.appearance.panel_height;
        buf[6] = self.appearance.panel_at_bottom as u8;
        buf[7] = self.appearance.panel_opacity;
        buf[8..12].copy_from_slice(&self.appearance.accent.0);
        buf[12] = self.appearance.animations_on as u8;
        buf[13] = self.appearance.tween_speed;
        buf[14] = self.input.pointer_speed;
        buf[15] = self.input.natural_scroll as u8;
        buf[16] = self.input.tap_to_click as u8;
        buf[17] = self.input.pointer_accel as u8;
        buf[18..20].copy_from_slice(&self.input.double_tap_ms.to_le_bytes());
        buf[20..22].copy_from_slice(&self.input.hold_threshold_ms.to_le_bytes());
        buf[22] = self.input.drag_threshold_px;
        buf[23] = self.display.refresh_hz;
        buf[24] = match self.display.vsync {
            VsyncMode::Off => 0,
            VsyncMode::Adaptive => 1,
            VsyncMode::On => 2,
        };
        buf[25] = self.display.gamma;
        buf[26] = self.display.hdr as u8;
        buf[27..31].copy_from_slice(&self.system.thermal_throttle_mc.to_le_bytes());
        buf[31] = match self.system.crash_behavior {
            CrashBehavior::Ignore => 0,
            CrashBehavior::ShowNotification => 1,
            CrashBehavior::RestartShell => 2,
        };
        buf[32] = self.system.auto_lock_minutes;
        buf[33] = self.keybinds.count as u8;
        let mut pos = 34;
        for e in self.keybinds.entries[..self.keybinds.count].iter() {
            if pos + 8 > buf.len() {
                return None;
            }
            buf[pos] = match e.action {
                ShellAction::Custom(n) => n,
                ShellAction::OpenLauncher => 0,
                ShellAction::CloseLauncher => 1,
                ShellAction::OpenSettings => 2,
                ShellAction::CloseSettings => 3,
                ShellAction::FocusNext => 4,
                ShellAction::FocusPrev => 5,
                ShellAction::CloseApp => 6,
                ShellAction::ToggleFullscreen => 7,
                ShellAction::Screenshot => 8,
                ShellAction::LockScreen => 9,
                ShellAction::ShowDesktop => 10,
                ShellAction::VolumeUp => 11,
                ShellAction::VolumeDown => 12,
                ShellAction::BrightnessUp => 13,
                ShellAction::BrightnessDown => 14,
                ShellAction::WorkspaceNext => 15,
                ShellAction::WorkspacePrev => 16,
            };
            buf[pos + 1..pos + 5].copy_from_slice(&e.keybind.code.to_le_bytes());
            buf[pos + 5..pos + 7].copy_from_slice(&e.keybind.modifiers.to_le_bytes());
            buf[pos + 7] = e.enabled as u8;
            pos += 8;
        }
        Some(pos)
    }

    pub fn deserialize(&mut self, buf: &[u8]) -> bool {
        if buf.len() < 34 {
            return false;
        }
        const MAGIC: u32 = 0xC4E57077;
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC || buf[4] != SETTINGS_VERSION {
            return false;
        }
        self.appearance.panel_height = buf[5].clamp(16, 48);
        self.appearance.panel_at_bottom = buf[6] != 0;
        self.appearance.panel_opacity = buf[7];
        self.appearance.accent = AccentColor([buf[8], buf[9], buf[10], buf[11]]);
        self.appearance.animations_on = buf[12] != 0;
        self.appearance.tween_speed = buf[13].clamp(1, 10);
        self.input.pointer_speed = buf[14].clamp(1, 10);
        self.input.natural_scroll = buf[15] != 0;
        self.input.tap_to_click = buf[16] != 0;
        self.input.pointer_accel = buf[17] != 0;
        self.input.double_tap_ms = u16::from_le_bytes([buf[18], buf[19]]);
        self.input.hold_threshold_ms = u16::from_le_bytes([buf[20], buf[21]]);
        self.input.drag_threshold_px = buf[22];
        self.display.refresh_hz = buf[23];
        self.display.vsync = match buf[24] {
            0 => VsyncMode::Off,
            2 => VsyncMode::On,
            _ => VsyncMode::Adaptive,
        };
        self.display.gamma = buf[25];
        self.display.hdr = buf[26] != 0;
        self.system.thermal_throttle_mc = u32::from_le_bytes([buf[27], buf[28], buf[29], buf[30]]);
        self.system.crash_behavior = match buf[31] {
            0 => CrashBehavior::Ignore,
            2 => CrashBehavior::RestartShell,
            _ => CrashBehavior::ShowNotification,
        };
        self.system.auto_lock_minutes = buf[32];
        let kcount = buf[33] as usize;
        self.keybinds.count = 0;
        let mut pos = 34usize;
        for _ in 0..kcount.min(MAX_KEYBINDS) {
            if pos + 8 > buf.len() {
                break;
            }
            let action_byte = buf[pos];
            let code = u32::from_le_bytes([buf[pos + 1], buf[pos + 2], buf[pos + 3], buf[pos + 4]]);
            let mods = u16::from_le_bytes([buf[pos + 5], buf[pos + 6]]);
            let enabled = buf[pos + 7] != 0;
            let action = match action_byte {
                0 => ShellAction::OpenLauncher,
                1 => ShellAction::CloseLauncher,
                2 => ShellAction::OpenSettings,
                3 => ShellAction::CloseSettings,
                4 => ShellAction::FocusNext,
                5 => ShellAction::FocusPrev,
                6 => ShellAction::CloseApp,
                7 => ShellAction::ToggleFullscreen,
                8 => ShellAction::Screenshot,
                9 => ShellAction::LockScreen,
                10 => ShellAction::ShowDesktop,
                11 => ShellAction::VolumeUp,
                12 => ShellAction::VolumeDown,
                13 => ShellAction::BrightnessUp,
                14 => ShellAction::BrightnessDown,
                15 => ShellAction::WorkspaceNext,
                16 => ShellAction::WorkspacePrev,
                n => ShellAction::Custom(n),
            };
            let i = self.keybinds.count;
            self.keybinds.entries[i] = crate::compositor::settings::KeybindEntry {
                action,
                keybind: Keybind::new(code, mods),
                enabled,
            };
            self.keybinds.count += 1;
            pos += 8;
        }
        self.dirty = dirty::ALL;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_deserialize_roundtrip() {
        let mut store = SettingsStore::new();
        store.set_accent(AccentColor::ORBITAL_GREEN);
        store.set_panel_height(40);
        store.set_pointer_speed(8);

        let mut buf = [0u8; SERIALIZED_MAX_BYTES];
        let written = store.serialize(&mut buf).unwrap();
        assert!(written > 0);

        let mut restored = SettingsStore::new();
        assert!(restored.deserialize(&buf[..written]));
        assert_eq!(restored.appearance.accent, AccentColor::ORBITAL_GREEN);
        assert_eq!(restored.appearance.panel_height, 40);
        assert_eq!(restored.input.pointer_speed, 8);
    }

    #[test]
    fn bind_action_overwrites_existing() {
        let mut store = SettingsStore::new();
        store.bind_action(ShellAction::OpenLauncher, Keybind::new(57, modifier::CTRL));
        let kb = store.keybinds.lookup(ShellAction::OpenLauncher).unwrap();
        assert_eq!(kb.code, 57);
        assert_eq!(kb.modifiers, modifier::CTRL);
    }

    #[test]
    fn dirty_cleared_after_clear() {
        let mut store = SettingsStore::new();
        store.clear_dirty();
        assert!(!store.is_dirty(dirty::ALL));
    }
}
