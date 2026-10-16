// Where the samples of one recording go: the WAV file, the level for the
// pill, and the numbers for the log. RecordingSink in MicrophoneRecorder.swift.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufWriter};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use hound::{SampleFormat, WavSpec, WavWriter};

use super::{AudioError, Recording};
use crate::log;

pub const SAMPLE_RATE: u32 = 16_000;

const WAV_SPEC: WavSpec = WavSpec {
    channels: 1,
    sample_rate: SAMPLE_RATE,
    bits_per_sample: 16,
    sample_format: SampleFormat::Int,
};

// Only enough to take the jitter off single buffers. The pace of the meter is
// set where it is drawn, as on macOS (attack and release in
// MicrophoneRecorder.swift).
const ATTACK: f32 = 0.010;
const RELEASE: f32 = 0.010;

pub struct Sink {
    tag: String,
    path: PathBuf,
    writer: Option<WavWriter<BufWriter<File>>>,
    /// Samples written so far.
    frames: u64,
    smoothed: f32,
    peak: f32,
    /// Every raw reading, one per buffer, so the spread can be reported at
    /// the end.
    levels: Vec<f32>,
    /// Set once any sample is not zero.
    heard_sound: bool,
    /// Half a sample left over from the last buffer, if one ended mid-sample.
    carry: Option<u8>,
    /// When the stream was asked for, for the first-buffer timing.
    requested_at: Instant,
    first_buffer_logged: bool,
    write_failed: bool,
}

impl Sink {
    /// Creates the file under a name no other recording has.
    pub fn create(tag: &str, requested_at: Instant) -> Result<Sink, AudioError> {
        let folder = folder().ok_or_else(|| {
            AudioError::CouldNotStart("neither XDG_DATA_HOME nor HOME is set".to_string())
        })?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)
            .map_err(|error| {
                AudioError::CouldNotStart(format!("cannot create {}: {error}", folder.display()))
            })?;

        let (path, file) = create_file(&folder).map_err(|error| {
            AudioError::CouldNotStart(format!("cannot create a file in {}: {error}", folder.display()))
        })?;
        let writer = WavWriter::new(BufWriter::new(file), WAV_SPEC).map_err(|error| {
            let _ = fs::remove_file(&path);
            AudioError::CouldNotStart(format!("cannot write {}: {error}", path.display()))
        })?;

        Ok(Sink {
            tag: tag.to_string(),
            path,
            writer: Some(writer),
            frames: 0,
            smoothed: 0.0,
            peak: 0.0,
            levels: Vec::new(),
            heard_sound: false,
            carry: None,
            requested_at,
            first_buffer_logged: false,
            write_failed: false,
        })
    }

    /// Takes one buffer of S16LE mono bytes. Returns the smoothed level.
    pub fn consume(&mut self, bytes: &[u8]) -> f32 {
        if !self.first_buffer_logged {
            self.first_buffer_logged = true;
            log::info!(
                Audio,
                "[{}] first audio buffer after {} ms",
                self.tag,
                self.requested_at.elapsed().as_millis()
            );
        }

        let samples = self.samples(bytes);
        if samples.is_empty() {
            return self.smoothed;
        }

        let mut sum = 0.0f64;
        for &sample in &samples {
            sum += f64::from(sample) * f64::from(sample);
            if sample != 0 {
                self.heard_sound = true;
            }
        }
        let raw = ((sum / samples.len() as f64).sqrt() / 32768.0).min(1.0) as f32;

        // Rising and falling may differ, as on macOS; both are 10 ms now.
        let seconds = samples.len() as f32 / SAMPLE_RATE as f32;
        let time_constant = if raw > self.smoothed { ATTACK } else { RELEASE };
        let step = (seconds / time_constant).min(1.0);
        self.smoothed += (raw - self.smoothed) * step;

        self.peak = self.peak.max(raw);
        self.levels.push(raw);

        self.write(&samples);
        self.smoothed
    }

    /// Splits bytes into little-endian samples, keeping an odd byte for the
    /// next buffer.
    fn samples(&mut self, bytes: &[u8]) -> Vec<i16> {
        let mut samples = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut rest = bytes;
        if let Some(low) = self.carry.take() {
            match rest.split_first() {
                Some((&high, tail)) => {
                    samples.push(i16::from_le_bytes([low, high]));
                    rest = tail;
                }
                None => {
                    self.carry = Some(low);
                    return samples;
                }
            }
        }
        let mut pairs = rest.chunks_exact(2);
        for pair in &mut pairs {
            samples.push(i16::from_le_bytes([pair[0], pair[1]]));
        }
        if let [odd] = pairs.remainder() {
            self.carry = Some(*odd);
        }
        samples
    }

    fn write(&mut self, samples: &[i16]) {
        let Some(writer) = self.writer.as_mut() else { return };
        for &sample in samples {
            if let Err(error) = writer.write_sample(sample) {
                if !self.write_failed {
                    self.write_failed = true;
                    log::error!(Audio, "[{}] write to {} failed: {error}", self.tag, self.path.display());
                }
                return;
            }
            self.frames += 1;
        }
    }

    /// Closes the file and reports what was recorded.
    pub fn finish(mut self) -> Result<Recording, AudioError> {
        self.log_spread();

        let writer = self.writer.take();
        if let Some(writer) = writer {
            if let Err(error) = writer.finalize() {
                log::error!(Audio, "[{}] could not finish {}: {error}", self.tag, self.path.display());
                return Err(AudioError::CouldNotWrite(error.to_string()));
            }
        }

        let duration = self.frames as f64 / f64::from(SAMPLE_RATE);
        log::info!(
            Audio,
            "[{}] recorded {:.2} s, peak {:.3}, file {}",
            self.tag,
            duration,
            self.peak,
            self.path.display()
        );
        if self.frames == 0 {
            log::warning!(Audio, "[{}] no audio arrived before the key was released", self.tag);
        } else if !self.heard_sound {
            log::warning!(Audio, "[{}] silent recording: every sample is zero", self.tag);
        }

        Ok(Recording {
            path: self.path,
            duration,
            peak: self.peak,
        })
    }

    /// Drops the file without finishing it.
    pub fn discard(mut self) {
        self.writer = None;
        if let Err(error) = fs::remove_file(&self.path) {
            log::warning!(Audio, "[{}] could not remove {}: {error}", self.tag, self.path.display());
        }
    }

    /// Where the level actually sat, in decibels, as on macOS.
    fn log_spread(&self) {
        if self.levels.len() <= 3 {
            return;
        }
        let mut sorted = self.levels.clone();
        sorted.sort_by(f32::total_cmp);
        let decibels = |share: f64| -> String {
            let last = sorted.len() - 1;
            let index = ((share * last as f64) as usize).min(last);
            format!("{:.0}", 20.0 * f64::from(sorted[index]).max(1e-6).log10())
        };
        log::info!(
            Audio,
            "[{}] level dB: p10 {} p50 {} p90 {} max {} over {} buffers",
            self.tag,
            decibels(0.10),
            decibels(0.50),
            decibels(0.90),
            decibels(1.0),
            sorted.len()
        );
    }
}

/// `$XDG_DATA_HOME/sensay/audio`, or `~/.local/share/sensay/audio` when the
/// variable is unset, empty or relative. None without HOME.
fn folder() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local").join("share"),
    };
    Some(base.join("sensay").join("audio"))
}

/// `dictation-YYYY-MM-DD-HHMMSS-mmm.wav` in local time. Milliseconds, because
/// two presses inside one second must not share a file. A name that is taken
/// anyway gets `-1`, `-2`: losing a recording is worse than an odd name.
fn create_file(folder: &Path) -> io::Result<(PathBuf, File)> {
    let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S-%3f").to_string();
    let mut attempt = 0u32;
    loop {
        let name = if attempt == 0 {
            format!("dictation-{stamp}.wav")
        } else {
            format!("dictation-{stamp}-{attempt}.wav")
        };
        let path = folder.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 1000 => {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}
