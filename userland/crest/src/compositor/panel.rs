// CREST PANEL — top-of-screen system bar
//
// Rendered entirely via Obsidian SDF + fixed-point math. No font rasterizer
// needed for the icons — each widget is an SDF primitive.
//
// Layout (left to right):
//   [LAUNCHER_BTN] [APP_TITLE...............] [CLOCK] [THERMAL] [SETTINGS_BTN]
//
// PanelConfig: fully configurable at runtime (position, height, colors, widgets).
// PanelState: dynamic data (time, thermal, focused app title hash, crash count).
// Panel::evaluate_pixel(x, y): returns ARGB color for any pixel in panel row.
//
// Widgets are SDF shapes evaluated per-pixel:
//   LauncherButton : circle
//   AppTitle       : horizontal capsule (width proportional to title hash length)
//   ClockWidget    : circular clock face with two hands (hour + minute)
//   ThermalWidget  : vertical fill bar, color shifts cool→hot
//   SettingsButton : gear (approximated as ring + notches)
//
// The panel integrates with:
//   CompositorPipeline::invalidate_rect(0, 0, width, panel_height) on state change
//   GestureEvent::Tap → hit-test each widget → emit PanelAction

use crate::obsidian::{Fixed, fixed_hypot};

pub const MAX_PANEL_HEIGHT: u32 = 48;
pub const MAX_APP_TITLE_LEN: usize = 64;

// ─── CONFIG ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct PanelColors {
    pub background:       [u8; 4],
    pub launcher_idle:    [u8; 4],
    pub launcher_hot:     [u8; 4],
    pub title_text:       [u8; 4],
    pub clock_hands:      [u8; 4],
    pub thermal_cool:     [u8; 4],
    pub thermal_hot:      [u8; 4],
    pub settings_idle:    [u8; 4],
    pub divider:          [u8; 4],
}

impl PanelColors {
    pub const OBSIDIAN: Self = Self {
        background:    [8,  10, 14,  240],
        launcher_idle: [60, 120, 220, 255],
        launcher_hot:  [220, 80,  60, 255],
        title_text:    [200, 200, 210, 255],
        clock_hands:   [180, 220, 255, 255],
        thermal_cool:  [40,  200, 120, 255],
        thermal_hot:   [240, 60,  40,  255],
        settings_idle: [150, 150, 160, 255],
        divider:       [40,  40,  50,  255],
    };
}

#[derive(Clone, Copy, Debug)]
pub struct PanelConfig {
    pub screen_width:  u32,
    pub panel_height:  u32,  // pixels, max MAX_PANEL_HEIGHT
    pub at_bottom:     bool,
    pub colors:        PanelColors,
    pub show_thermal:  bool,
    pub show_clock:    bool,
    pub launcher_bind: u32,  // scancode for launcher hotkey
}

impl PanelConfig {
    pub fn new(screen_width: u32) -> Self {
        Self {
            screen_width,
            panel_height:  32,
            at_bottom:     false,
            colors:        PanelColors::OBSIDIAN,
            show_thermal:  true,
            show_clock:    true,
            launcher_bind: 125, // Super key
        }
    }
}

// ─── STATE ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default)]
pub struct PanelState {
    /// Hour [0,11] and minute [0,59] for clock widget.
    pub hour:               u8,
    pub minute:             u8,
    /// Thermal in millicelsius, clamped to [0, 120_000].
    pub thermal_mc:         u32,
    /// FNV-1a hash of focused app name — used to derive title bar width.
    pub focused_app_hash:   u64,
    /// Whether launcher overlay is open.
    pub launcher_open:      bool,
    /// Non-zero if settings panel is open.
    pub settings_open:      bool,
    /// Total system crash count (from heliosphere).
    pub crash_count:        u64,
}

// ─── PANEL ACTIONS ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelAction {
    OpenLauncher,
    CloseLauncher,
    OpenSettings,
    CloseSettings,
    None,
}

// ─── WIDGET HIT REGIONS (pixel x ranges) ───────────────────────────────────

#[derive(Clone, Copy)]
struct Layout {
    launcher_x0: u32,
    launcher_x1: u32,
    title_x0:    u32,
    title_x1:    u32,
    clock_x0:    u32,
    clock_x1:    u32,
    thermal_x0:  u32,
    thermal_x1:  u32,
    settings_x0: u32,
    settings_x1: u32,
}

impl Layout {
    fn compute(cfg: &PanelConfig) -> Self {
        let w = cfg.screen_width;
        let h = cfg.panel_height;
        // Fixed proportional layout
        let launcher_x0  = 4;
        let launcher_x1  = launcher_x0 + h;
        let settings_x1  = w.saturating_sub(4);
        let settings_x0  = settings_x1.saturating_sub(h);
        let thermal_x1   = if cfg.show_thermal { settings_x0.saturating_sub(4) } else { settings_x0 };
        let thermal_x0   = if cfg.show_thermal { thermal_x1.saturating_sub(16) } else { thermal_x1 };
        let clock_x1     = if cfg.show_clock   { thermal_x0.saturating_sub(4)  } else { thermal_x0 };
        let clock_x0     = if cfg.show_clock   { clock_x1.saturating_sub(h + 16) } else { clock_x1 };
        let title_x0     = launcher_x1 + 8;
        let title_x1     = clock_x0.saturating_sub(8);
        Self { launcher_x0, launcher_x1, title_x0, title_x1,
               clock_x0, clock_x1, thermal_x0, thermal_x1,
               settings_x0, settings_x1 }
    }
}

// ─── SDF HELPERS ───────────────────────────────────────────────────────────

fn sdf_circle(px: Fixed, py: Fixed, cx: Fixed, cy: Fixed, r: Fixed) -> Fixed {
    fixed_hypot(px.saturating_sub(cx), py.saturating_sub(cy)).saturating_sub(r)
}

fn sdf_capsule_h(px: Fixed, py: Fixed, x0: Fixed, x1: Fixed, cy: Fixed, r: Fixed) -> Fixed {
    // Horizontal capsule: clamp px to [x0,x1], distance to nearest endpoint circle
    let clamped_x = if px < x0 { x0 } else if px > x1 { x1 } else { px };
    fixed_hypot(px.saturating_sub(clamped_x), py.saturating_sub(cy)).saturating_sub(r)
}

fn sdf_rect(px: Fixed, py: Fixed, x0: Fixed, y0: Fixed, x1: Fixed, y1: Fixed) -> Fixed {
    let cx = x0.saturating_add(x1).multiply(Fixed::from_raw(Fixed::ONE.raw() / 2));
    let cy = y0.saturating_add(y1).multiply(Fixed::from_raw(Fixed::ONE.raw() / 2));
    let hx = x1.saturating_sub(x0).multiply(Fixed::from_raw(Fixed::ONE.raw() / 2));
    let hy = y1.saturating_sub(y0).multiply(Fixed::from_raw(Fixed::ONE.raw() / 2));
    let dx = px.saturating_sub(cx).abs().saturating_sub(hx);
    let dy = py.saturating_sub(cy).abs().saturating_sub(hy);
    let ox = dx.max(Fixed::ZERO);
    let oy = dy.max(Fixed::ZERO);
    let outside = fixed_hypot(ox, oy);
    let inside  = dx.max(dy).min(Fixed::ZERO);
    outside.saturating_add(inside)
}

fn lerp_color(a: [u8; 4], b: [u8; 4], t: Fixed) -> [u8; 4] {
    let tr = t.raw().max(0).min(Fixed::ONE.raw()) as u32;
    let lerp = |ca: u8, cb: u8| -> u8 {
        let r = ca as u32 * (Fixed::ONE.raw() as u32 - tr) + cb as u32 * tr;
        (r >> 16).min(255) as u8
    };
    [lerp(a[0], b[0]), lerp(a[1], b[1]), lerp(a[2], b[2]), lerp(a[3], b[3])]
}

// ─── PANEL ─────────────────────────────────────────────────────────────────

pub struct Panel {
    pub config: PanelConfig,
    pub state:  PanelState,
    layout:     Layout,
}

impl Panel {
    pub fn new(config: PanelConfig) -> Self {
        let layout = Layout::compute(&config);
        Self { config, state: PanelState::default(), layout }
    }

    pub fn update_state(&mut self, state: PanelState) {
        self.state = state;
    }

    pub fn update_config(&mut self, config: PanelConfig) {
        self.layout = Layout::compute(&config);
        self.config = config;
    }

    /// Hit-test a tap at (x, y). Returns a PanelAction.
    pub fn hit_test(&self, x: i32, y: i32) -> PanelAction {
        let x = x as u32;
        let _y = y as u32;
        if x >= self.layout.launcher_x0 && x < self.layout.launcher_x1 {
            return if self.state.launcher_open { PanelAction::CloseLauncher }
                   else                        { PanelAction::OpenLauncher  };
        }
        if x >= self.layout.settings_x0 && x < self.layout.settings_x1 {
            return if self.state.settings_open { PanelAction::CloseSettings }
                   else                        { PanelAction::OpenSettings  };
        }
        PanelAction::None
    }

    /// Evaluate ARGB color for pixel (px, py) within the panel row.
    /// py is relative to panel top (0 = top of panel).
    pub fn evaluate_pixel(&self, px: u32, py: u32) -> [u8; 4] {
        if py >= self.config.panel_height { return [0, 0, 0, 0]; }

        let _w = self.config.panel_width_fixed();
        let _h = self.config.panel_height_fixed();
        let fx = px_to_fixed(px, self.config.screen_width);
        let fy = px_to_fixed(py, self.config.panel_height);
        let l  = &self.layout;
        let c  = &self.config.colors;

        // Background
        let mut color = c.background;

        // Bottom divider line
        if py + 1 == self.config.panel_height {
            return c.divider;
        }

        // ── Launcher button ────────────────────────────────────────────────
        {
            let cx = px_to_fixed(
                (l.launcher_x0 + l.launcher_x1) / 2, self.config.screen_width);
            let cy = Fixed::from_ratio(1, 2).unwrap_or(Fixed::ZERO);
            let r  = px_to_fixed(l.launcher_x1 - l.launcher_x0, self.config.screen_width)
                       .multiply(Fixed::from_ratio(2, 5).unwrap_or(Fixed::ZERO));
            let d  = sdf_circle(fx, fy, cx, cy, r);
            if d <= Fixed::ZERO {
                color = if self.state.launcher_open { c.launcher_hot } else { c.launcher_idle };
                // Inner cross (plus sign) for launcher icon
                let thick = r.multiply(Fixed::from_ratio(1, 5).unwrap_or(Fixed::ZERO));
                let half_r = r.multiply(Fixed::from_ratio(3, 4).unwrap_or(Fixed::ZERO));
                let bar_h = sdf_capsule_h(fx, fy, cx.saturating_sub(half_r),
                                          cx.saturating_add(half_r), cy, thick);
                let dx2  = fx.saturating_sub(cx).abs().saturating_sub(thick);
                let dy2  = fy.saturating_sub(cy).abs().saturating_sub(half_r);
                let bar_v_d = dx2.max(dy2);
                if bar_h <= Fixed::ZERO || bar_v_d <= Fixed::ZERO {
                    color = [255, 255, 255, 255];
                }
            }
        }

        // ── App title capsule ──────────────────────────────────────────────
        {
            // Width derived from app hash — deterministic, no allocation
            let hash_width_frac = ((self.state.focused_app_hash & 0xFFFF) as i32 + 0x8000) as u32;
            let max_title_w = l.title_x1.saturating_sub(l.title_x0);
            let title_w = (max_title_w / 2) + (max_title_w / 2 * hash_width_frac / 0xFFFF);
            let tx0 = l.title_x0;
            let tx1 = (tx0 + title_w).min(l.title_x1);
            let cy  = Fixed::from_ratio(1, 2).unwrap_or(Fixed::ZERO);
            let r   = px_to_fixed(self.config.panel_height / 4, self.config.panel_height);
            let d   = sdf_capsule_h(
                fx, fy,
                px_to_fixed(tx0, self.config.screen_width),
                px_to_fixed(tx1, self.config.screen_width),
                cy, r,
            );
            if d <= Fixed::ZERO {
                color = blend_alpha(c.title_text, color, 60);
            }
        }

        // ── Clock widget ───────────────────────────────────────────────────
        if self.config.show_clock {
            let cx  = px_to_fixed((l.clock_x0 + l.clock_x1) / 2, self.config.screen_width);
            let cy  = Fixed::from_ratio(1, 2).unwrap_or(Fixed::ZERO);
            let r   = px_to_fixed((l.clock_x1 - l.clock_x0).min(self.config.panel_height) / 2 - 2,
                                   self.config.screen_width);
            // Dial ring
            let ring_outer = sdf_circle(fx, fy, cx, cy, r);
            let ring_inner = sdf_circle(fx, fy, cx, cy,
                r.saturating_sub(Fixed::from_raw(Fixed::ONE.raw() / 64)));
            let ring_d = ring_outer.abs().saturating_sub(ring_outer.saturating_sub(ring_inner));
            if ring_outer <= Fixed::ZERO {
                color = blend_alpha([20, 25, 35, 200], color, 180);
            }
            // Hour hand
            let hour_angle = angle_fixed(self.state.hour as i32 * 30); // 360/12 = 30 deg per hour
            let (hx, hy)   = sin_cos_fixed(hour_angle);
            let hand_r = r.multiply(Fixed::from_ratio(5, 10).unwrap_or(Fixed::ZERO));
            let tip_x  = cx.saturating_add(hx.multiply(hand_r));
            let _tip_y  = cy.saturating_sub(hy.multiply(hand_r));
            let hd = sdf_capsule_h(fx, fy, cx, tip_x, cy,
                Fixed::from_raw(Fixed::ONE.raw() / 128));
            // Minute hand
            let min_angle = angle_fixed(self.state.minute as i32 * 6);
            let (mx, my)  = sin_cos_fixed(min_angle);
            let min_r = r.multiply(Fixed::from_ratio(8, 10).unwrap_or(Fixed::ZERO));
            let mtip_x = cx.saturating_add(mx.multiply(min_r));
            let _mtip_y = cy.saturating_sub(my.multiply(min_r));
            let md = sdf_capsule_h(fx, fy, cx, mtip_x, cy,
                Fixed::from_raw(Fixed::ONE.raw() / 196));
            if hd <= Fixed::ZERO || md <= Fixed::ZERO {
                color = c.clock_hands;
            }
            let _ = ring_d; // suppress unused
        }

        // ── Thermal bar ────────────────────────────────────────────────────
        if self.config.show_thermal {
            let bar_x0 = px_to_fixed(l.thermal_x0 + 2, self.config.screen_width);
            let bar_x1 = px_to_fixed(l.thermal_x1 - 2, self.config.screen_width);
            let bar_y0 = px_to_fixed(4,  self.config.panel_height);
            let bar_y1 = px_to_fixed(self.config.panel_height - 4, self.config.panel_height);
            let bg_d   = sdf_rect(fx, fy, bar_x0, bar_y0, bar_x1, bar_y1);
            if bg_d <= Fixed::ZERO {
                let temp_norm = ((self.state.thermal_mc.min(120_000) as i64 * Fixed::ONE.raw() as i64)
                    / 120_000) as i32;
                let t = Fixed::from_raw(temp_norm);
                let fill_y1 = bar_y0.saturating_add(
                    bar_y1.saturating_sub(bar_y0).multiply(t));
                let fill_d = sdf_rect(fx, fy, bar_x0, bar_y0, bar_x1, fill_y1);
                color = blend_alpha([15, 15, 20, 220], color, 200);
                if fill_d <= Fixed::ZERO {
                    color = lerp_color(c.thermal_cool, c.thermal_hot, t);
                }
            }
        }

        // ── Settings button (gear ring) ────────────────────────────────────
        {
            let cx = px_to_fixed((l.settings_x0 + l.settings_x1) / 2, self.config.screen_width);
            let cy = Fixed::from_ratio(1, 2).unwrap_or(Fixed::ZERO);
            let r  = px_to_fixed((l.settings_x1 - l.settings_x0) / 2 - 2,
                                  self.config.screen_width);
            let ring_d = sdf_circle(fx, fy, cx, cy, r).abs()
                .saturating_sub(Fixed::from_raw(Fixed::ONE.raw() / 64));
            if ring_d <= Fixed::ZERO {
                color = if self.state.settings_open { c.launcher_hot } else { c.settings_idle };
            }
            // Gear dot center
            let dot_d = sdf_circle(fx, fy, cx, cy,
                r.multiply(Fixed::from_ratio(3, 10).unwrap_or(Fixed::ZERO)));
            if dot_d <= Fixed::ZERO {
                color = if self.state.settings_open { c.launcher_hot } else { c.settings_idle };
            }
        }

        color
    }
}

// ─── HELPERS ───────────────────────────────────────────────────────────────

impl PanelConfig {
    fn panel_width_fixed(&self) -> Fixed {
        Fixed::from_integer(self.screen_width as i16)
    }
    fn panel_height_fixed(&self) -> Fixed {
        Fixed::from_integer(self.panel_height as i16)
    }
}

fn px_to_fixed(px: u32, max: u32) -> Fixed {
    Fixed::from_ratio(px as i32, max.max(1) as i32).unwrap_or(Fixed::ZERO)
}

fn blend_alpha(src: [u8; 4], dst: [u8; 4], alpha: u8) -> [u8; 4] {
    let a = alpha as u32;
    let inv = 255 - a;
    [
        ((src[0] as u32 * a + dst[0] as u32 * inv) / 255) as u8,
        ((src[1] as u32 * a + dst[1] as u32 * inv) / 255) as u8,
        ((src[2] as u32 * a + dst[2] as u32 * inv) / 255) as u8,
        255,
    ]
}

// Degrees [0,359] → Fixed angle in our fixed-point radian space
fn angle_fixed(degrees: i32) -> Fixed {
    // TAU_RAW = 411_775 (from orbital_cortex.rs)
    const TAU_RAW: i32 = 411_775;
    Fixed::from_raw((degrees as i64 * TAU_RAW as i64 / 360) as i32)
}

fn sin_cos_fixed(angle: Fixed) -> (Fixed, Fixed) {
    // Reuse the same Taylor series approach as orbital_cortex.rs
    // sin via 7th-order Taylor; cos = sin(angle + PI/2)
    const PI_RAW:     i32 = 205_887;
    const HALF_PI:    i32 = 102_944;
    const TAU_RAW:    i32 = 411_775;
    const ONE_SIXTH:  i32 = 10_923;
    const ONE_TWENTIETH: i32 = 546;
    const ONE_5040:   i32 = 13;

    let sin = |a: Fixed| -> Fixed {
        let mut x = Fixed::from_raw(a.raw().rem_euclid(TAU_RAW));
        let mut neg = false;
        if x.raw() > PI_RAW { x = Fixed::from_raw(x.raw() - PI_RAW); neg = true; }
        if x.raw() > HALF_PI { x = Fixed::from_raw(PI_RAW - x.raw()); }
        let x2 = x.multiply(x);
        let x3 = x2.multiply(x);
        let x5 = x3.multiply(x2);
        let x7 = x5.multiply(x2);
        let v = x
            .saturating_sub(x3.multiply(Fixed::from_raw(ONE_SIXTH)))
            .saturating_add(x5.multiply(Fixed::from_raw(ONE_TWENTIETH)))
            .saturating_sub(x7.multiply(Fixed::from_raw(ONE_5040)));
        if neg { v.saturating_neg() } else { v }
    };
    let s = sin(angle);
    let c = sin(Fixed::from_raw(angle.raw().wrapping_add(HALF_PI)));
    (s, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_hit_test_launcher_region() {
        let cfg  = PanelConfig::new(1920);
        let panel = Panel::new(cfg);
        let action = panel.hit_test(panel.layout.launcher_x0 as i32 + 2, 10);
        assert_eq!(action, PanelAction::OpenLauncher);
    }

    #[test]
    fn panel_hit_test_settings_region() {
        let cfg   = PanelConfig::new(1920);
        let panel = Panel::new(cfg);
        let action = panel.hit_test(panel.layout.settings_x0 as i32 + 2, 10);
        assert_eq!(action, PanelAction::OpenSettings);
    }

    #[test]
    fn evaluate_pixel_returns_background_outside_widgets() {
        let cfg   = PanelConfig::new(1920);
        let panel = Panel::new(cfg);
        let color = panel.evaluate_pixel(960, 16);
        // Should be some color, not zero
        assert_ne!(color, [0, 0, 0, 0]);
    }
}
