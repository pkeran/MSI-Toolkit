//! Local, side-effect free editors for the two high-touch surfaces.
//!
//! Everything here is pure Rust so it can be unit tested without a window or a
//! daemon: `cargo test -p msilt-ui` covers the clamping rules, the safety floor,
//! the fixed anchor, presets and the 24-zone selection model.
//!
//! Contract notes (see `docs/HARDWARE_PROTOCOLS.md` §2 and
//! `docs/UI_DESIGN_SPEC.md` §6):
//!   * temperatures are fixed knots, never editable;
//!   * duty must be `>= floor`, non-decreasing and `<= max_duty_percent`;
//!   * the anchor knot (`fixed_anchor`) is not draggable;
//!   * dragging edits a *preview*; only `apply` turns it into one IPC command.

use msilt_core::{
    FanChannel, FanCurve, FanPoint, FanState, Rgb8, FAN_POINT_COUNT, KEYBOARD_ZONE_COUNT,
    MAX_FAN_DUTY_PERCENT,
};

/// One fixed temperature knot with the verified factory duty floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knot {
    pub temperature_c: u8,
    pub duty_percent: u8,
    pub floor_percent: u8,
}

impl Knot {
    fn as_point(self) -> FanPoint {
        FanPoint { temperature_c: self.temperature_c, duty_percent: self.duty_percent }
    }
}

/// Why an edit was refused or clamped. Rendered inline under the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClampReason {
    Anchor,
    Floor,
    Ceiling,
    MonotonicLow,
    MonotonicHigh,
}

impl ClampReason {
    pub fn text(self, index: usize, value: u8) -> String {
        match self {
            Self::Anchor => "首点是固定锚点，保持设备给定的下限值".into(),
            Self::Floor => format!("点 {} 不能低于 {}%（出厂安全下限）", index + 1, value),
            Self::Ceiling => format!("点 {} 已限制到本机上限 {}%", index + 1, value),
            Self::MonotonicLow => format!("占空比不能下降：点 {} 已限制到 {}%", index + 1, value),
            Self::MonotonicHigh => format!("占空比不能上升：点 {} 已限制到 {}%", index + 1, value),
        }
    }
}

/// Local preview of one fan channel.
#[derive(Debug, Clone)]
pub struct CurveEditor {
    knots: [Knot; FAN_POINT_COUNT],
    committed: [u8; FAN_POINT_COUNT],
    max_duty_percent: u8,
    fixed_anchor: bool,
    editable: bool,
    hint: Option<String>,
}

impl CurveEditor {
    /// Builds an editor from a published `FanState`.
    ///
    /// When the daemon has not published `limits` the editor is read-only: the
    /// curve is shown but no edit can be committed (no fabricated floor).
    pub fn new(_channel: FanChannel, state: &FanState) -> Self {
        let mut knots = [Knot { temperature_c: 0, duty_percent: 0, floor_percent: 0 }; FAN_POINT_COUNT];
        for (index, point) in state.curve.points.iter().enumerate() {
            knots[index] = Knot {
                temperature_c: point.temperature_c,
                duty_percent: point.duty_percent,
                floor_percent: point.duty_percent,
            };
        }
        let mut committed = [0u8; FAN_POINT_COUNT];
        for (slot, point) in committed.iter_mut().zip(state.curve.points) {
            *slot = point.duty_percent;
        }
        match state.limits {
            Some(limits) => {
                for (index, floor) in limits.floor.points.iter().enumerate() {
                    knots[index].floor_percent = floor.duty_percent;
                }
                Self {
                    knots,
                    committed,
                    max_duty_percent: limits.max_duty_percent,
                    fixed_anchor: limits.fixed_anchor,
                    editable: true,
                    hint: None,
                }
            }
            None => Self {
                knots,
                committed,
                max_duty_percent: 100,
                fixed_anchor: true,
                editable: false,
                hint: None,
            },
        }
    }

    /// Placeholder used before the first snapshot arrives (and for a channel the
    /// device does not expose). Never presented as a real curve.
    pub fn placeholder(_channel: FanChannel) -> Self {
        Self {
            knots: [Knot { temperature_c: 0, duty_percent: 0, floor_percent: 0 }; FAN_POINT_COUNT],
            committed: [0; FAN_POINT_COUNT],
            max_duty_percent: 100,
            fixed_anchor: true,
            editable: false,
            hint: None,
        }
    }

    pub fn knots(&self) -> &[Knot; FAN_POINT_COUNT] {
        &self.knots
    }

    pub fn max_duty_percent(&self) -> u8 {
        self.max_duty_percent
    }

    pub fn fixed_anchor(&self) -> bool { self.fixed_anchor }

    pub fn is_editable(&self) -> bool {
        self.editable
    }

    pub fn is_dirty(&self) -> bool {
        self.knots.iter().map(|k| k.duty_percent).ne(self.committed.iter().copied())
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    pub fn clear_hint(&mut self) {
        self.hint = None;
    }

    /// Applies one vertical edit. Returns `true` when the preview changed.
    ///
    /// The edit is always clamped into the valid region, so the canvas can never
    /// hold an illegal curve; `hint` explains every clamp.
    pub fn set_duty(&mut self, index: usize, duty: i32) -> bool {
        self.hint = None;
        if !self.editable || index >= FAN_POINT_COUNT {
            self.hint = Some("当前通道只读：映射尚未通过写入验收".into());
            return false;
        }
        if index == 0 && self.fixed_anchor {
            self.hint = Some(ClampReason::Anchor.text(index, 0));
            return false;
        }

        let requested = duty.clamp(0, MAX_FAN_DUTY_PERCENT as i32) as u8;
        let floor = self.knots[index].floor_percent;
        let previous = if index == 0 { 0 } else { self.knots[index - 1].duty_percent };
        let next = if index + 1 < FAN_POINT_COUNT {
            self.knots[index + 1].duty_percent
        } else {
            self.max_duty_percent
        };

        let lower = floor.max(previous);
        let upper = self.max_duty_percent.min(next).max(lower);
        let clamped = requested.clamp(lower, upper);

        if clamped != requested {
            self.hint = Some(if requested < lower {
                if floor >= previous {
                    ClampReason::Floor.text(index, lower)
                } else {
                    ClampReason::MonotonicLow.text(index, lower)
                }
            } else if requested > self.max_duty_percent {
                ClampReason::Ceiling.text(index, self.max_duty_percent)
            } else {
                ClampReason::MonotonicHigh.text(index, upper)
            });
        }

        let changed = self.knots[index].duty_percent != clamped;
        self.knots[index].duty_percent = clamped;
        changed
    }

    /// Discards the preview and returns to the published snapshot.
    pub fn reset(&mut self) {
        for (knot, committed) in self.knots.iter_mut().zip(self.committed) {
            knot.duty_percent = committed;
        }
        self.hint = None;
    }

    /// Named templates. `0` = factory floor, `1` = quiet, `2` = performance.
    ///
    /// Templates are *local previews* only — they are never claimed to be
    /// already applied. The result is monotonic by construction.
    pub fn preset(&mut self, kind: u8) {
        if !self.editable {
            self.hint = Some("当前通道只读".into());
            return;
        }
        self.hint = None;
        let offset: u8 = match kind {
            0 => 0,
            1 => 6,
            _ => 12,
        };
        let mut next = [0u8; FAN_POINT_COUNT];
        for index in 0..FAN_POINT_COUNT {
            let floor = self.knots[index].floor_percent;
            let target = if index == 0 { floor } else { floor.saturating_add(offset) };
            let previous = if index == 0 { 0 } else { next[index - 1] };
            next[index] = target.max(floor).max(previous).min(self.max_duty_percent);
        }
        for (knot, value) in self.knots.iter_mut().zip(next) {
            knot.duty_percent = value;
        }
    }

    pub fn curve(&self) -> FanCurve {
        FanCurve { points: self.knots.map(Knot::as_point) }
    }
}

// ── keyboard zones ──────────────────────────────────────────────────────────

/// 24-zone local selection + colour model (with continuous HSV picker state).
#[derive(Debug, Clone)]
pub struct ZoneEditor {
    colors: [Rgb8; KEYBOARD_ZONE_COUNT],
    selected: [bool; KEYBOARD_ZONE_COUNT],
    brightness: u8,
    hue: f32,
    sat: f32,
    val: f32,
}

/// Preset order matches the page buttons: MSI red, ice blue, white, off.
pub const ZONE_PRESETS: [[Rgb8; KEYBOARD_ZONE_COUNT]; 4] = [
    [Rgb8 { r: 0xFF, g: 0x1A, b: 0x1A }; KEYBOARD_ZONE_COUNT],
    [Rgb8 { r: 0x35, g: 0xC8, b: 0xFF }; KEYBOARD_ZONE_COUNT],
    [Rgb8 { r: 0xF2, g: 0xF2, b: 0xF2 }; KEYBOARD_ZONE_COUNT],
    [Rgb8 { r: 0, g: 0, b: 0 }; KEYBOARD_ZONE_COUNT],
];

impl Default for ZoneEditor {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ZoneEditor {
    pub fn new(zones: Option<&[Rgb8; KEYBOARD_ZONE_COUNT]>) -> Self {
        let mut selected = [false; KEYBOARD_ZONE_COUNT];
        selected[0] = true;
        let colors = zones.copied().unwrap_or(ZONE_PRESETS[0]);
        let (hue, sat, val) = rgb_to_hsv(colors[0]);
        Self {
            colors,
            selected,
            brightness: 3,
            hue,
            sat,
            val,
        }
    }

    pub fn colors(&self) -> &[Rgb8; KEYBOARD_ZONE_COUNT] {
        &self.colors
    }

    pub fn selected(&self) -> &[bool; KEYBOARD_ZONE_COUNT] {
        &self.selected
    }

    pub fn brightness(&self) -> u8 {
        self.brightness
    }

    pub fn set_brightness(&mut self, level: i32) {
        self.brightness = level.clamp(0, 3) as u8;
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| **s).count()
    }

    fn sync_hsv_from_active(&mut self) {
        let (h, s, v) = rgb_to_hsv(self.active_color());
        if s > 0.01 && v > 0.01 {
            self.hue = h;
        }
        self.sat = s;
        self.val = v;
    }

    /// Exclusive select, or additive toggle when `additive`.
    pub fn toggle(&mut self, index: usize, additive: bool) {
        if index >= KEYBOARD_ZONE_COUNT {
            return;
        }
        if additive {
            self.selected[index] = !self.selected[index];
            self.sync_hsv_from_active();
            return;
        }
        let was_only_this = self.selected[index] && self.selected_count() == 1;
        self.selected = [false; KEYBOARD_ZONE_COUNT];
        self.selected[index] = !was_only_this;
        self.sync_hsv_from_active();
    }

    pub fn select_all(&mut self) {
        self.selected = [true; KEYBOARD_ZONE_COUNT];
        self.sync_hsv_from_active();
    }

    pub fn select_none(&mut self) {
        self.selected = [false; KEYBOARD_ZONE_COUNT];
    }

    pub fn apply_preset(&mut self, kind: usize) {
        if let Some(preset) = ZONE_PRESETS.get(kind) {
            self.colors = *preset;
            self.sync_hsv_from_active();
        }
    }

    /// Paints the selected zones; when nothing is selected the whole board is
    /// painted so the control is never a no-op.
    pub fn paint(&mut self, color: Rgb8) {
        let count = self.selected_count();
        for index in 0..KEYBOARD_ZONE_COUNT {
            if count == 0 || self.selected[index] {
                self.colors[index] = color;
            }
        }
        self.sync_hsv_from_active();
    }

    /// Interactive color-pad / hue-bar drag paint preserving exact HSV cursor position.
    pub fn paint_hsv(&mut self, h: f32, s: f32, v: f32) {
        self.hue = h.rem_euclid(360.0);
        self.sat = s.clamp(0.0, 1.0);
        self.val = v.clamp(0.0, 1.0);
        let color = hsv_to_rgb(self.hue, self.sat, self.val);
        let count = self.selected_count();
        for index in 0..KEYBOARD_ZONE_COUNT {
            if count == 0 || self.selected[index] {
                self.colors[index] = color;
            }
        }
    }

    pub fn hsv(&self) -> (f32, f32, f32) {
        (self.hue, self.sat, self.val)
    }

    pub fn pure_hue_rgb(&self) -> Rgb8 {
        hsv_to_rgb(self.hue, 1.0, 1.0)
    }

    /// Returns the unscaled color of the first selected zone (or zone 0 when none selected).
    pub fn active_color(&self) -> Rgb8 {
        self.selected
            .iter()
            .position(|&s| s)
            .map_or(self.colors[0], |index| self.colors[index])
    }

    /// Applies the software brightness scale (`rgb * level / 3`).
    pub fn scaled(&self) -> [Rgb8; KEYBOARD_ZONE_COUNT] {
        let level = u32::from(self.brightness);
        self.colors.map(|c| Rgb8 {
            r: ((u32::from(c.r) * level) / 3) as u8,
            g: ((u32::from(c.g) * level) / 3) as u8,
            b: ((u32::from(c.b) * level) / 3) as u8,
        })
    }
}

/// Converts HSV (`h` in `[0, 360)`, `s`/`v` in `[0, 1]`) to `Rgb8`.
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Rgb8 {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    Rgb8 {
        r: ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        g: ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        b: ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    }
}

/// Converts `Rgb8` to `(h, s, v)` where `h` in `[0, 360)`, `s`/`v` in `[0, 1]`.
pub fn rgb_to_hsv(rgb: Rgb8) -> (f32, f32, f32) {
    let r = f32::from(rgb.r) / 255.0;
    let g = f32::from(rgb.g) / 255.0;
    let b = f32::from(rgb.b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta < 1e-6 {
        0.0
    } else if (max - r).abs() < 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-6 {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    let s = if max < 1e-6 { 0.0 } else { delta / max };
    (h, s, max)
}

/// Parses `#RRGGBB` or `RRGGBB` (ASCII hex, optional surrounding whitespace).
pub fn parse_hex_rgb(input: &str) -> Option<Rgb8> {
    let raw = input.trim().strip_prefix('#').unwrap_or_else(|| input.trim());
    if raw.len() != 6 || !raw.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&raw[0..2], 16).ok()?;
    let g = u8::from_str_radix(&raw[2..4], 16).ok()?;
    let b = u8::from_str_radix(&raw[4..6], 16).ok()?;
    Some(Rgb8 { r, g, b })
}

#[cfg(test)]
mod tests {
    use super::*;
    use msilt_core::{FanCurveLimits, DEFAULT_MAX_FAN_DUTY_PERCENT};

    fn limits(temps: [u8; 6], duties: [u8; 6]) -> FanCurveLimits {
        FanCurveLimits {
            floor: FanCurve {
                points: std::array::from_fn(|i| FanPoint {
                    temperature_c: temps[i],
                    duty_percent: duties[i],
                }),
            },
            max_duty_percent: DEFAULT_MAX_FAN_DUTY_PERCENT,
            fixed_anchor: true,
        }
    }

    fn cpu_state() -> FanState {
        let floor = limits([0, 48, 56, 62, 70, 78], [0, 36, 42, 52, 76, 84]);
        FanState { curve: floor.floor, limits: Some(floor) }
    }

    #[test]
    fn factory_curve_is_valid_against_its_own_limits() {
        let state = cpu_state();
        state.curve.validate_with_limits(state.limits.as_ref().unwrap()).unwrap();
        let editor = CurveEditor::new(FanChannel::Cpu, &state);
        assert!(!editor.is_dirty());
        assert_eq!(editor.max_duty_percent(), DEFAULT_MAX_FAN_DUTY_PERCENT);
    }

    #[test]
    fn anchor_knot_is_not_draggable() {
        let mut editor = CurveEditor::new(FanChannel::Cpu, &cpu_state());
        assert!(!editor.set_duty(0, 20));
        assert_eq!(editor.knots()[0].duty_percent, 0);
        assert!(editor.hint().is_some_and(|h| h.contains("锚点")));
    }

    #[test]
    fn duty_is_clamped_to_the_factory_floor() {
        let mut editor = CurveEditor::new(FanChannel::Cpu, &cpu_state());
        // knot 5 floor is 84 %
        editor.set_duty(5, 10);
        assert_eq!(editor.knots()[5].duty_percent, 84);
        assert!(editor.hint().is_some_and(|h| h.contains("下限")));
    }

    #[test]
    fn duty_never_decreases_along_the_curve() {
        let mut editor = CurveEditor::new(FanChannel::Cpu, &cpu_state());
        // Raising knot 2 above knot 3 must clamp to knot 3 (52 %).
        editor.set_duty(2, 95);
        assert_eq!(editor.knots()[2].duty_percent, 52);
        assert!(editor.is_dirty());
        editor.curve().validate_shape().unwrap();
    }

    #[test]
    fn ceiling_is_respected_even_when_limits_allow_150() {
        let mut state = cpu_state();
        if let Some(limits) = state.limits.as_mut() {
            limits.max_duty_percent = 150;
        }
        let mut editor = CurveEditor::new(FanChannel::Gpu, &state);
        editor.set_duty(5, 149);
        assert_eq!(editor.knots()[5].duty_percent, 149);
        assert!(editor.curve().validate_shape().is_ok());
    }

    #[test]
    fn presets_stay_valid_and_never_below_the_floor() {
        for kind in 0..3u8 {
            let mut editor = CurveEditor::new(FanChannel::Cpu, &cpu_state());
            editor.preset(kind);
            for (index, knot) in editor.knots().iter().enumerate() {
                assert!(knot.duty_percent >= knot.floor_percent, "kind {kind} knot {index}");
                assert!(knot.duty_percent <= editor.max_duty_percent());
            }
            editor.curve().validate_shape().unwrap();
            let state = cpu_state();
            editor.curve().validate_with_limits(state.limits.as_ref().unwrap()).unwrap();
        }
    }

    #[test]
    fn reset_returns_to_the_published_curve() {
        let mut editor = CurveEditor::new(FanChannel::Cpu, &cpu_state());
        editor.set_duty(4, 90);
        assert!(editor.is_dirty());
        editor.reset();
        assert!(!editor.is_dirty());
        assert_eq!(editor.knots()[4].duty_percent, 76);
    }

    #[test]
    fn missing_limits_make_the_channel_read_only() {
        let state = FanState { curve: cpu_state().curve, limits: None };
        let mut editor = CurveEditor::new(FanChannel::Gpu, &state);
        assert!(!editor.is_editable());
        assert!(!editor.set_duty(3, 50));
        assert!(editor.hint().is_some());
    }

    #[test]
    fn zone_toggle_is_exclusive_without_the_additive_flag() {
        let mut zones = ZoneEditor::default();
        assert_eq!(zones.selected_count(), 1);
        zones.toggle(7, false);
        assert!(zones.selected()[7]);
        assert_eq!(zones.selected_count(), 1);
        zones.toggle(9, true);
        assert_eq!(zones.selected_count(), 2);
        zones.toggle(9, true);
        assert_eq!(zones.selected_count(), 1);
    }

    #[test]
    fn select_all_and_presets_cover_24_zones() {
        let mut zones = ZoneEditor::default();
        zones.select_all();
        assert_eq!(zones.selected_count(), KEYBOARD_ZONE_COUNT);
        zones.apply_preset(3);
        assert!(zones.colors().iter().all(|c| *c == Rgb8 { r: 0, g: 0, b: 0 }));
        zones.set_brightness(0);
        assert!(zones.scaled().iter().all(|c| *c == Rgb8 { r: 0, g: 0, b: 0 }));
        zones.set_brightness(9);
        assert_eq!(zones.brightness(), 3);
    }

    #[test]
    fn paint_without_selection_paints_everything() {
        let mut zones = ZoneEditor::default();
        zones.select_none();
        zones.paint(Rgb8 { r: 1, g: 2, b: 3 });
        assert!(zones.colors().iter().all(|c| *c == Rgb8 { r: 1, g: 2, b: 3 }));
    }

    #[test]
    fn parse_hex_rgb_accepts_valid_and_rejects_malformed() {
        assert_eq!(parse_hex_rgb("#38bdf8"), Some(Rgb8 { r: 0x38, g: 0xBD, b: 0xF8 }));
        assert_eq!(parse_hex_rgb("  E62028 "), Some(Rgb8 { r: 0xE6, g: 0x20, b: 0x28 }));
        assert_eq!(parse_hex_rgb("#FFF"), None);
        assert_eq!(parse_hex_rgb("#GGGGGG"), None);
        assert_eq!(parse_hex_rgb(""), None);
    }

    #[test]
    fn paint_updates_selected_and_active_color_tracks_selection() {
        let mut zones = ZoneEditor::default();
        zones.toggle(5, false);
        let teal = Rgb8 { r: 0x14, g: 0xB8, b: 0xA6 };
        zones.paint(teal);
        assert_eq!(zones.colors()[5], teal);
        assert_eq!(zones.active_color(), teal);
        assert_ne!(zones.colors()[0], teal);
    }

    #[test]
    fn hsv_rgb_round_trip_and_paint_hsv_work_smoothly() {
        for expected in [
            Rgb8 { r: 255, g: 0, b: 0 },
            Rgb8 { r: 0, g: 255, b: 0 },
            Rgb8 { r: 0, g: 0, b: 255 },
            Rgb8 { r: 56, g: 189, b: 248 },
            Rgb8 { r: 255, g: 255, b: 255 },
            Rgb8 { r: 0, g: 0, b: 0 },
        ] {
            let (h, s, v) = rgb_to_hsv(expected);
            let actual = hsv_to_rgb(h, s, v);
            assert!((i16::from(actual.r) - i16::from(expected.r)).abs() <= 1);
            assert!((i16::from(actual.g) - i16::from(expected.g)).abs() <= 1);
            assert!((i16::from(actual.b) - i16::from(expected.b)).abs() <= 1);
        }
        let mut zones = ZoneEditor::default();
        zones.toggle(10, false);
        zones.paint_hsv(210.0, 0.8, 1.0);
        let c = zones.colors()[10];
        assert!(c.b > c.g && c.g > c.r, "expected blue-ish color, got {c:?}");
    }
}
