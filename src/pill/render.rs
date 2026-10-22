// Draws the pill into a pixmap, for the desktops where Sensay draws it itself
// (layer-shell, X11). Shapes, timing and colours are those of the GNOME
// extension (gnome/sensay-pill@sensay.app/extension.js), which follows
// PillView.swift and Theme.swift of the macOS version: a capsule that grows
// for the level meter, grey dots that turn into bars, a stepped spinner of
// eight rays while the text is on its way.
//
// All measurements are in logical points; `draw` scales them to the pixmap.

use std::time::Instant;

use tiny_skia::{Color, FillRule, LineCap, Mask, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

use super::PillState;

const HEIGHT: f32 = 32.0;
const EXPANDED_WIDTH: f32 = HEIGHT * 2.47;
const IDLE_WIDTH: f32 = HEIGHT * 1.36;
const IDLE_HEIGHT: f32 = HEIGHT * 0.29;
/// From the bottom of the work area, in points.
pub const BOTTOM_INSET: f32 = 24.0;

const BAR_COUNT: usize = 10;
const BAR_WIDTH: f32 = HEIGHT * 0.066;
const BAR_GAP: f32 = HEIGHT * 0.070;
const BAR_PITCH: f32 = BAR_WIDTH + BAR_GAP;
const METER_WIDTH: f32 = BAR_COUNT as f32 * BAR_WIDTH + (BAR_COUNT as f32 - 1.0) * BAR_GAP;
const BAR_MIN: f32 = HEIGHT * 0.035;
const BAR_MAX: f32 = HEIGHT * 0.47;
const BAR_REST: f32 = HEIGHT * 0.074;
const BAR_LOUD: f32 = HEIGHT * 0.39;
const BELL_WIDTH: f32 = 6.0;
const WAVE_PERIOD: f64 = 1.0;
const WAVE_FIRST_AMPLITUDE: f64 = 0.14;
const WAVE_FIRST_PHASE: f64 = -3.03;
const WAVE_SECOND_AMPLITUDE: f64 = 0.073;
const WAVE_SECOND_PHASE: f64 = 0.60;
/// Grey dots stand in for the bars until this long after the expansion.
const DOTS_DURATION: f64 = 0.09;
const SHIMMER_RANGE: (f32, f32) = (0.82, 1.0);
const SHIMMER_STEP: (f64, f64) = (0.4, 1.5);
const LEVEL_ATTACK: f64 = 0.040;
const LEVEL_RELEASE: f64 = 0.080;

const SPINNER_RAYS: usize = 8;
const SPINNER_SCALE: f32 = 2.0 / 3.0;
const SPINNER_INNER: f32 = HEIGHT * 0.117 * SPINNER_SCALE;
const SPINNER_OUTER: f32 = HEIGHT * 0.280 * SPINNER_SCALE;
const SPINNER_THICKNESS: f32 = BAR_WIDTH * SPINNER_SCALE;
const SPINNER_TO_END: f32 = HEIGHT / 2.0;
const SPINNER_OFFSET: f32 = METER_WIDTH / 2.0 + HEIGHT / 2.0;
const SPINNER_STEP: f64 = 8.0 / 60.0;
const SPINNER_FADE_PER_STEP: f64 = 0.104;
const SPINNER_FIRST_RAY: usize = 7;

const WAITING_GROWTH: f32 = SPINNER_OFFSET + SPINNER_TO_END - EXPANDED_WIDTH / 2.0;
const WAITING_WIDTH: f32 = EXPANDED_WIDTH + WAITING_GROWTH;
/// The surface never changes size: wide enough for the waiting pill.
pub const CANVAS_WIDTH: f32 = EXPANDED_WIDTH + 2.0 * WAITING_GROWTH;
pub const CANVAS_HEIGHT: f32 = HEIGHT;

// Fill is black, border and bars white. The macOS open pill is 0.85; the
// lighter 0.6 lets the window under it show through while recording too.
const FILL_IDLE: f32 = 0.5;
const FILL_ACTIVE: f32 = 0.6;
const BORDER_IDLE: f32 = 0.42;
const BORDER_ACTIVE: f32 = 0.08;
const DOT_ALPHA: f32 = 0.32;

/// Cubic Bezier control points x1, y1, x2, y2, and the duration.
const EXPAND: ([f64; 4], f64) = ([0.0, 0.5, 0.0, 1.0], 0.10);
const COLLAPSE: ([f64; 4], f64) = ([0.2, 0.85, 0.4, 0.95], 0.12);

/// Points as integer surface sizes.
pub fn canvas_size() -> (u32, u32) {
    (CANVAS_WIDTH.ceil() as u32, CANVAS_HEIGHT.ceil() as u32)
}

#[derive(Clone, Copy)]
struct Capsule {
    left: f32,
    width: f32,
    height: f32,
    fill: f32,
    border: f32,
}

impl Capsule {
    /// The bottom edge never moves, and neither does the centre, except
    /// while waiting: the left edge stays where the listening pill had it.
    fn of(state: PillState) -> Capsule {
        let (width, height) = match state {
            PillState::Idle => (IDLE_WIDTH, IDLE_HEIGHT),
            PillState::Listening => (EXPANDED_WIDTH, HEIGHT),
            PillState::Waiting => (WAITING_WIDTH, HEIGHT),
        };
        let left = if state == PillState::Waiting {
            (CANVAS_WIDTH - EXPANDED_WIDTH) / 2.0
        } else {
            (CANVAS_WIDTH - width) / 2.0
        };
        let idle = state == PillState::Idle;
        Capsule {
            left,
            width,
            height,
            fill: if idle { FILL_IDLE } else { FILL_ACTIVE },
            border: if idle { BORDER_IDLE } else { BORDER_ACTIVE },
        }
    }

    fn mix(self, to: Capsule, p: f32) -> Capsule {
        let mix = |a: f32, b: f32| a + (b - a) * p;
        Capsule {
            left: mix(self.left, to.left),
            width: mix(self.width, to.width),
            height: mix(self.height, to.height),
            fill: mix(self.fill, to.fill),
            border: mix(self.border, to.border),
        }
    }
}

struct Animation {
    from: Capsule,
    start: f64,
    curve: [f64; 4],
    duration: f64,
}

/// One bar's brightness walk: an eased move to a random next brightness
/// over a random time, then the next move.
#[derive(Clone, Copy)]
struct Walk {
    from: f32,
    to: f32,
    start: f64,
    duration: f64,
    value: f32,
}

pub struct Renderer {
    epoch: Instant,
    state: PillState,
    capsule: Capsule,
    animation: Option<Animation>,
    level_target: f64,
    level: f64,
    heights: [f32; BAR_COUNT],
    bars_shown: bool,
    dots: bool,
    dots_until: Option<f64>,
    walks: [Walk; BAR_COUNT],
    spinner_start: Option<f64>,
    last_tick: f64,
    random: u64,
}

impl Renderer {
    pub fn new() -> Renderer {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |since| since.as_nanos() as u64)
            | 1;
        Renderer {
            epoch: Instant::now(),
            state: PillState::Idle,
            capsule: Capsule::of(PillState::Idle),
            animation: None,
            level_target: 0.0,
            level: 0.0,
            heights: [BAR_WIDTH; BAR_COUNT],
            bars_shown: false,
            dots: false,
            dots_until: None,
            walks: [Walk { from: 1.0, to: 1.0, start: 0.0, duration: 0.001, value: 1.0 }; BAR_COUNT],
            spinner_start: None,
            last_tick: 0.0,
            random: seed,
        }
    }

    pub fn state(&self) -> PillState {
        self.state
    }

    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    /// The level on the meter's scale, 0...1, as the app sends it.
    pub fn set_level(&mut self, level: f64) {
        self.level_target = level.clamp(0.0, 1.0);
    }

    pub fn set_state(&mut self, next: PillState) {
        if next == self.state {
            return;
        }
        let previous = self.state;
        self.state = next;
        let now = self.now();

        // Before the capsule starts to move, so nothing is seen sliding with
        // it on the way out.
        if next != PillState::Waiting {
            self.spinner_start = None;
        }
        let spec = if next == PillState::Idle { COLLAPSE } else { EXPAND };
        self.animation = Some(Animation {
            from: self.presented(now),
            start: now,
            curve: spec.0,
            duration: spec.1,
        });
        self.capsule = Capsule::of(next);

        match next {
            PillState::Idle => {
                self.dots_until = None;
                self.level = 0.0;
                // Bars go at once; the capsule closes around an empty inside.
                self.set_bars(false, false, now);
            }
            PillState::Waiting => {
                // The bars carry on; the level is zero from here, so they
                // settle to their silence shape with the ripple going.
                if previous == PillState::Idle || self.dots_until.is_some() {
                    self.dots_until = None;
                    self.set_bars(false, true, now);
                }
                self.spinner_start = Some(now);
            }
            PillState::Listening => {
                self.level_target = 0.0;
                self.dots_until = Some(now + EXPAND.1 + DOTS_DURATION);
                self.heights = [BAR_WIDTH; BAR_COUNT];
                self.set_bars(true, true, now);
            }
        }
        self.last_tick = now;
    }

    fn set_bars(&mut self, dots: bool, shown: bool, now: f64) {
        self.dots = dots;
        self.bars_shown = shown;
        self.walks = [Walk { from: 1.0, to: 1.0, start: now, duration: 0.001, value: 1.0 }; BAR_COUNT];
    }

    /// Advances everything to this moment. True while anything still moves,
    /// so the caller keeps drawing frames; false once the pill is at rest.
    pub fn tick(&mut self) -> bool {
        let now = self.now();
        let dt = (now - self.last_tick).clamp(0.001, 0.1);
        self.last_tick = now;

        let target = if self.state == PillState::Listening { self.level_target } else { 0.0 };
        let tau = if target > self.level { LEVEL_ATTACK } else { LEVEL_RELEASE };
        self.level += (target - self.level) * (1.0 - (-dt / tau).exp());

        if let Some(until) = self.dots_until {
            if now >= until {
                self.dots_until = None;
                self.set_bars(false, true, now);
            }
        }
        if self.dots_until.is_none() && self.bars_shown {
            for index in 0..BAR_COUNT {
                self.heights[index] = bar_target(index, self.level as f32, now);
            }
            self.update_shimmer(now);
        }
        self.presented(now);
        self.state != PillState::Idle || self.animation.is_some()
    }

    fn update_shimmer(&mut self, now: f64) {
        for index in 0..BAR_COUNT {
            let mut walk = self.walks[index];
            let mut u = (now - walk.start) / walk.duration;
            if u >= 1.0 {
                walk.from = walk.to;
                walk.to = SHIMMER_RANGE.0 + self.next_random() as f32 * (SHIMMER_RANGE.1 - SHIMMER_RANGE.0);
                walk.start = now;
                walk.duration = SHIMMER_STEP.0 + self.next_random() * (SHIMMER_STEP.1 - SHIMMER_STEP.0);
                u = 0.0;
            }
            let eased = (1.0 - (std::f64::consts::PI * u).cos()) / 2.0;
            walk.value = walk.from + (walk.to - walk.from) * eased as f32;
            self.walks[index] = walk;
        }
    }

    /// 0...1, xorshift: nothing here needs more than that.
    fn next_random(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        (self.random >> 11) as f64 / (1u64 << 53) as f64
    }

    /// The capsule as it is on screen at this moment.
    fn presented(&mut self, now: f64) -> Capsule {
        let Some(animation) = &self.animation else {
            return self.capsule;
        };
        let u = (now - animation.start) / animation.duration;
        if u >= 1.0 {
            self.animation = None;
            return self.capsule;
        }
        animation.from.mix(self.capsule, bezier(animation.curve, u) as f32)
    }

    /// Draws the pill into `pixmap`, `scale` pixels to the point, centred and
    /// standing on the pixmap's bottom edge.
    pub fn draw(&mut self, pixmap: &mut Pixmap, scale: f32) {
        pixmap.fill(Color::TRANSPARENT);
        let now = self.now();
        let capsule = self.presented(now);
        let transform = Transform::from_scale(scale, scale);
        let width = pixmap.width() as f32 / scale;
        let bottom = pixmap.height() as f32 / scale;
        let offset = (width - CANVAS_WIDTH) / 2.0;
        let x = offset + capsule.left;
        let y = bottom - capsule.height;
        let radius = capsule.height / 2.0;

        let Some(shape) = rounded_rect(x, y, capsule.width, capsule.height, radius) else { return };
        pixmap.fill_path(&shape, &paint(0, 0, 0, capsule.fill), FillRule::Winding, transform, None);

        // The capsule cuts off what it contains, so it reveals the bars and
        // the spinner as it grows and hides them as it shrinks.
        if let Some(mut mask) = Mask::new(pixmap.width(), pixmap.height()) {
            mask.fill_path(&shape, FillRule::Winding, true, transform);
            if self.bars_shown {
                self.draw_bars(pixmap, offset + CANVAS_WIDTH / 2.0, bottom - capsule.height / 2.0, transform, &mask);
            }
            if let Some(start) = self.spinner_start {
                draw_spinner(pixmap, x + EXPANDED_WIDTH / 2.0 + SPINNER_OFFSET, bottom - HEIGHT / 2.0, now - start, transform, &mask);
            }
        }

        if let Some(edge) = rounded_rect(x + 0.5, y + 0.5, capsule.width - 1.0, capsule.height - 1.0, radius - 0.5) {
            let stroke = Stroke { width: 1.0, ..Default::default() };
            pixmap.stroke_path(&edge, &paint(255, 255, 255, capsule.border), &stroke, transform, None);
        }
    }

    fn draw_bars(&self, pixmap: &mut Pixmap, centre_x: f32, centre_y: f32, transform: Transform, mask: &Mask) {
        let left = centre_x - METER_WIDTH / 2.0;
        for index in 0..BAR_COUNT {
            let height = self.heights[index];
            let alpha = if self.dots { DOT_ALPHA } else { self.walks[index].value };
            if let Some(bar) = rounded_rect(left + index as f32 * BAR_PITCH, centre_y - height / 2.0, BAR_WIDTH, height, BAR_WIDTH / 2.0) {
                pixmap.fill_path(&bar, &paint(255, 255, 255, alpha), FillRule::Winding, transform, Some(mask));
            }
        }
    }
}

/// Stepped: a new ray lights up at full brightness every step, going
/// clockwise, and each ray dims evenly until its turn comes again.
fn draw_spinner(pixmap: &mut Pixmap, centre_x: f32, centre_y: f32, age: f64, transform: Transform, mask: &Mask) {
    let elapsed = age.max(0.0) / SPINNER_STEP;
    let steps = elapsed.floor() as usize;
    let within = elapsed - steps as f64;
    let from = SPINNER_INNER + SPINNER_THICKNESS / 2.0;
    let to = SPINNER_OUTER - SPINNER_THICKNESS / 2.0;
    let stroke = Stroke { width: SPINNER_THICKNESS, line_cap: LineCap::Round, ..Default::default() };
    for index in 0..SPINNER_RAYS {
        let since = (SPINNER_FIRST_RAY + steps + SPINNER_RAYS - index % SPINNER_RAYS) % SPINNER_RAYS;
        let alpha = (1.0 - SPINNER_FADE_PER_STEP * (since as f64 + within)).max(0.0) as f32;
        // Counted clockwise from straight up.
        let angle = index as f32 * std::f32::consts::TAU / SPINNER_RAYS as f32;
        let (dx, dy) = (angle.sin(), -angle.cos());
        let mut builder = PathBuilder::new();
        builder.move_to(centre_x + dx * from, centre_y + dy * from);
        builder.line_to(centre_x + dx * to, centre_y + dy * to);
        if let Some(ray) = builder.finish() {
            pixmap.stroke_path(&ray, &paint(255, 255, 255, alpha), &stroke, transform, Some(mask));
        }
    }
}

fn paint(red: u8, green: u8, blue: u8, alpha: f32) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(red, green, blue, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8);
    paint.anti_alias = true;
    paint
}

fn rounded_rect(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<Path> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let r = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    // Control point distance for a quarter circle made of one cubic.
    let k = 0.552_284_8 * r;
    let (right, bottom) = (x + width, y + height);
    let mut builder = PathBuilder::new();
    builder.move_to(x + r, y);
    builder.line_to(right - r, y);
    builder.cubic_to(right - r + k, y, right, y + r - k, right, y + r);
    builder.line_to(right, bottom - r);
    builder.cubic_to(right, bottom - r + k, right - r + k, bottom, right - r, bottom);
    builder.line_to(x + r, bottom);
    builder.cubic_to(x + r - k, bottom, x, bottom - r + k, x, bottom - r);
    builder.line_to(x, y + r);
    builder.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    builder.close();
    builder.finish()
}

/// The CSS cubic-bezier timing function: progress for time u in 0...1.
fn bezier([x1, y1, x2, y2]: [f64; 4], u: f64) -> f64 {
    if u <= 0.0 {
        return 0.0;
    }
    if u >= 1.0 {
        return 1.0;
    }
    let (cx, cy) = (3.0 * x1, 3.0 * y1);
    let (bx, by) = (3.0 * (x2 - x1) - cx, 3.0 * (y2 - y1) - cy);
    let (ax, ay) = (1.0 - cx - bx, 1.0 - cy - by);
    let curve_x = |t: f64| ((ax * t + bx) * t + cx) * t;
    let curve_y = |t: f64| ((ay * t + by) * t + cy) * t;
    let (mut low, mut high, mut t) = (0.0, 1.0, u);
    for _ in 0..40 {
        let x = curve_x(t);
        if (x - u).abs() < 1e-6 {
            break;
        }
        if x < u {
            low = t;
        } else {
            high = t;
        }
        t = (low + high) / 2.0;
    }
    curve_y(t)
}

/// Constant per-bar factor: 1.0 in the middle, 0.57 at the outer bars.
fn bell(index: usize) -> f32 {
    let distance = (index as f32 - (BAR_COUNT as f32 - 1.0) / 2.0) / BELL_WIDTH;
    (-distance * distance).exp()
}

/// The ripple as a fraction of the bar's own height, travelling left to
/// right along the whole row once per period.
fn wave(time: f64, index: usize) -> f32 {
    let phase = time / WAVE_PERIOD - index as f64 / BAR_COUNT as f64;
    (WAVE_FIRST_AMPLITUDE * (std::f64::consts::TAU * phase + WAVE_FIRST_PHASE).cos()
        + WAVE_SECOND_AMPLITUDE * (2.0 * std::f64::consts::TAU * phase + WAVE_SECOND_PHASE).cos()) as f32
}

fn bar_target(index: usize, level: f32, time: f64) -> f32 {
    let middle = BAR_REST + (BAR_LOUD - BAR_REST) * level;
    (middle * bell(index) * (1.0 + wave(time, index))).clamp(BAR_MIN, BAR_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_start_at_zero_and_end_at_one() {
        assert_eq!(bezier(EXPAND.0, 0.0), 0.0);
        assert_eq!(bezier(EXPAND.0, 1.0), 1.0);
        let half = bezier(COLLAPSE.0, 0.5);
        assert!(half > 0.5 && half < 1.0);
    }

    #[test]
    fn the_resting_pill_draws_and_then_rests() {
        let (width, height) = canvas_size();
        let mut pixmap = Pixmap::new(width * 2, height * 2).unwrap();
        let mut renderer = Renderer::new();
        renderer.draw(&mut pixmap, 2.0);
        assert!(pixmap.pixels().iter().any(|pixel| pixel.alpha() > 0));
        assert!(!renderer.tick());
    }
}
