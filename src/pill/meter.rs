// The level scale of the meter, as `scale` in PillView.swift: turns the
// microphone RMS into the 0...1 the bars are drawn from.
//
// In decibels, on a scale that floats: its top sits just under the running
// peak of the voice, its bottom on the noise floor of the room. Normal speech
// therefore fills the row whatever the microphone gain, and room noise never
// lifts the row off its resting shape. The numbers are Theme.swift's.

/// Width of the scale under its top, in decibels.
const WINDOW_DB: f64 = 24.0;
/// The top of the scale sits this far under the running peak.
const TOP_MARGIN_DB: f64 = 2.0;
/// Readings closer than this to the noise floor draw the resting row.
const NOISE_MARGIN_DB: f64 = 4.0;
/// The running peak lets go toward the noise floor plus this.
const PEAK_MIN_ABOVE_NOISE_DB: f64 = 15.0;
/// How fast the running peak lets go, in seconds.
const PEAK_RELEASE: f64 = 2.5;
/// The noise floor follows the level down at this pace...
const NOISE_FALL: f64 = 0.3;
/// ...and creeps up at this one, so that speech does not lift it.
const NOISE_RISE: f64 = 20.0;
/// For this long after a session starts the floor also rises at the fast
/// pace, to pick up the room before the first word.
const NOISE_SETTLE: f64 = 0.3;
/// Where the noise floor starts before anything has been heard.
const INITIAL_NOISE_DB: f64 = -50.0;

pub struct LevelScale {
    noise_db: f64,
    peak_db: f64,
}

impl LevelScale {
    pub fn new() -> LevelScale {
        LevelScale {
            noise_db: INITIAL_NOISE_DB,
            peak_db: INITIAL_NOISE_DB + PEAK_MIN_ABOVE_NOISE_DB,
        }
    }

    /// The noise floor is kept across sessions, the room does not change
    /// between them; the peak of the last session is stale.
    pub fn start_session(&mut self) {
        self.peak_db = self.noise_db + PEAK_MIN_ABOVE_NOISE_DB;
    }

    /// `rms` 0...1, `dt` seconds since the previous reading, `session_age`
    /// seconds since the key went down.
    pub fn scale(&mut self, rms: f64, dt: f64, session_age: f64) -> f64 {
        // Zero means no buffer has arrived yet; nothing to learn from it.
        if rms <= 0.0 {
            return 0.0;
        }
        let db = 20.0 * rms.log10();

        let rising = db > self.noise_db;
        let settling = session_age < NOISE_SETTLE;
        let noise_tau = if rising && !settling { NOISE_RISE } else { NOISE_FALL };
        self.noise_db += (db - self.noise_db) * step(dt, noise_tau);

        let peak_floor = self.noise_db + PEAK_MIN_ABOVE_NOISE_DB;
        if db > self.peak_db {
            self.peak_db = db;
        } else {
            self.peak_db += (peak_floor - self.peak_db) * step(dt, PEAK_RELEASE);
        }
        self.peak_db = self.peak_db.max(peak_floor);

        let top = self.peak_db - TOP_MARGIN_DB;
        let bottom = (top - WINDOW_DB).max(self.noise_db + NOISE_MARGIN_DB);
        if db <= bottom || top <= bottom {
            return 0.0;
        }
        ((db - bottom) / (top - bottom)).min(1.0)
    }
}

/// Fraction of the way to a target covered in `dt` with time constant `tau`.
fn step(dt: f64, tau: f64) -> f64 {
    1.0 - (-dt / tau).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: f64 = 1.0 / 60.0;

    fn rms(db: f64) -> f64 {
        10f64.powf(db / 20.0)
    }

    /// Feeds one level for `seconds`, starting at `age`; returns the last
    /// value and the age after it.
    fn feed(scale: &mut LevelScale, db: f64, seconds: f64, mut age: f64) -> (f64, f64) {
        let mut value = 0.0;
        let frames = (seconds / FRAME).round() as usize;
        for _ in 0..frames {
            value = scale.scale(rms(db), FRAME, age);
            age += FRAME;
        }
        (value, age)
    }

    #[test]
    fn no_buffer_yet_draws_rest() {
        let mut scale = LevelScale::new();
        assert_eq!(scale.scale(0.0, FRAME, 0.0), 0.0);
    }

    #[test]
    fn room_noise_draws_rest() {
        let mut scale = LevelScale::new();
        scale.start_session();
        let (value, _) = feed(&mut scale, -60.0, 2.0, 0.0);
        assert_eq!(value, 0.0);
    }

    #[test]
    fn speech_over_room_noise_fills_the_row() {
        let mut scale = LevelScale::new();
        scale.start_session();
        let (_, age) = feed(&mut scale, -60.0, 1.0, 0.0);
        let value = scale.scale(rms(-20.0), FRAME, age);
        assert_eq!(value, 1.0);
    }
}
