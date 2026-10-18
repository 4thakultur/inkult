//! Spectrum check: does a "lossless" file really carry lossless audio, or
//! was it made from a lossy file (an MP3 decoded and saved as FLAC)?
//!
//! Lossy encoders throw away the highest frequencies with a steep low-pass
//! filter: MP3 at 128 kbps keeps about 16 kHz, 192 kbps about 19 kHz, Opus
//! and AAC at high bitrates about 20 kHz. Decoding the result to FLAC keeps
//! that hole. Real recordings fade out gradually towards the top, or end at
//! the CD's own anti-aliasing filter just below its 22.05 kHz limit.
//!
//! The check averages the spectrum of up to [`MAX_FRAMES`] windows spread
//! over the whole track (silent passages skipped), and looks for a *cliff*:
//! a drop of at least [`CLIFF_DB`] dB within about half a kilohertz, with
//! nothing but noise above it all the way up. A cliff below
//! [`LOSSY_BELOW_HZ`] means lossy. In a high-resolution file (88.2 kHz and
//! up), a cliff near 22–24 kHz means it was upsampled from CD quality.
//!
//! This is a heuristic, like the spectrograms people check by eye:
//!
//! * MP3 at 320 kbps (cut at about 20.5 kHz) cannot be told from CD audio
//!   this way, and passes.
//! * Music with little high-frequency content (old recordings, some
//!   ambient) gives no verdict ([`SpectrumVerdict::Inconclusive`]) rather
//!   than a guess.
//! * A track deliberately low-passed for its whole length would look lossy.

use std::f64::consts::PI;
use std::path::Path;

use crate::domain::quality::{QualityReport, SpectrumCheck, SpectrumVerdict};
use crate::tools::ToolError;
use crate::tools::ffmpeg::{Ffmpeg, StreamInput};

const WINDOW: usize = 4096;
/// Windows analysed per track at most; spread evenly over it.
const MAX_FRAMES: usize = 400;
/// Fewer sounding windows than this give no verdict.
const MIN_FRAMES: usize = 8;
/// Windows quieter than this (RMS, full scale = 1) are skipped: silence and
/// fades say nothing about the encoding.
const QUIET_RMS: f64 = 0.003;
const BAND_HZ: f64 = 250.0;
/// Bands averaged on each side of a candidate cliff.
const SIDE_BANDS: usize = 4;
/// Minimum level difference across a cliff.
const CLIFF_DB: f64 = 25.0;
/// Above a cliff, the spectrum must stay this far below the music.
const FLOOR_BELOW_MUSIC_DB: f64 = 45.0;
/// Where a cliff starts: the first band this far below the level under it.
const CUTOFF_FALL_DB: f64 = 10.0;
/// Above a cliff, no band may rise more than this over the floor.
const FLOOR_FLATNESS_DB: f64 = 10.0;
/// Cliffs lower than this are lossy encoders' (CD audio reaches ~21 kHz).
pub const LOSSY_BELOW_HZ: f64 = 20_300.0;
/// In high-resolution files, content ending below this means CD-rate
/// source audio (resamplers fade out between 22 and ~25 kHz).
const UPSAMPLED_BELOW_HZ: f64 = 26_000.0;
/// An empty ultrasonic range varies less than this from band to band.
const ULTRASONIC_FLATNESS_DB: f64 = 6.0;
/// A band this far below the music still counts as content.
const CONTENT_DB: f64 = 75.0;
/// Lower cliffs are not looked for: even 64 kbps encoders keep more.
const MIN_CLIFF_HZ: f64 = 10_000.0;

/// Collects the average power spectrum of a track as its samples arrive.
pub struct Analyzer {
    sample_rate: u32,
    window: Vec<f64>,
    buf: Vec<f64>,
    hop: usize,
    skip: usize,
    power: Vec<f64>,
    frames: usize,
}

impl Analyzer {
    /// For mono audio at `sample_rate`; `duration_secs` (if known) spreads
    /// the analysed windows over the whole track.
    pub fn new(sample_rate: u32, duration_secs: Option<f64>) -> Self {
        let window = (0..WINDOW)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / (WINDOW - 1) as f64).cos())
            .collect();
        let hop = duration_secs
            .map(|secs| (secs * f64::from(sample_rate)) as usize / MAX_FRAMES)
            .unwrap_or(0)
            .max(WINDOW);
        Self {
            sample_rate,
            window,
            buf: Vec::with_capacity(WINDOW),
            hop,
            skip: 0,
            power: vec![0.0; WINDOW / 2],
            frames: 0,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        for &sample in samples {
            if self.frames >= MAX_FRAMES {
                return;
            }
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            self.buf.push(f64::from(sample));
            if self.buf.len() == WINDOW {
                self.analyze_window();
                self.buf.clear();
                self.skip = self.hop - WINDOW;
            }
        }
    }

    fn analyze_window(&mut self) {
        let rms = (self.buf.iter().map(|s| s * s).sum::<f64>() / WINDOW as f64).sqrt();
        if rms < QUIET_RMS {
            return;
        }
        let mut re: Vec<f64> = self
            .buf
            .iter()
            .zip(&self.window)
            .map(|(s, w)| s * w)
            .collect();
        let mut im = vec![0.0; WINDOW];
        fft(&mut re, &mut im);
        for (bin, power) in self.power.iter_mut().enumerate() {
            *power += re[bin] * re[bin] + im[bin] * im[bin];
        }
        self.frames += 1;
    }

    pub fn finish(self) -> SpectrumCheck {
        if self.frames < MIN_FRAMES {
            return inconclusive("too little sound to analyse");
        }
        let nyquist = f64::from(self.sample_rate) / 2.0;
        let bin_hz = nyquist / (WINDOW / 2) as f64;
        let per_band = ((BAND_HZ / bin_hz).round() as usize).max(1);
        let levels: Vec<f64> = self
            .power
            .chunks_exact(per_band)
            .map(|band| {
                let mean = band.iter().sum::<f64>() / (band.len() * self.frames) as f64;
                10.0 * (mean + 1e-30).log10()
            })
            .collect();
        let band_hz = per_band as f64 * bin_hz;
        let edge = |band: usize| band as f64 * band_hz;
        let mean = |range: std::ops::Range<usize>| {
            let len = range.len() as f64;
            levels[range].iter().sum::<f64>() / len
        };
        let music_bands: Vec<usize> = (0..levels.len())
            .filter(|&b| (500.0..4000.0).contains(&edge(b)))
            .collect();
        let Some((&first, &last)) = music_bands.first().zip(music_bands.last()) else {
            return inconclusive("the sample rate is too low to analyse");
        };
        let music = mean(first..last + 1);

        // The strongest cliff with only a flat floor above it. The topmost
        // band is left out: resamplers and dither shape it on their own.
        let top = levels.len() - 1;
        let cliff = (SIDE_BANDS..top.saturating_sub(SIDE_BANDS + 2))
            .filter(|&k| edge(k) >= MIN_CLIFF_HZ)
            .filter_map(|k| {
                let below = mean(k - SIDE_BANDS..k);
                let above = mean(k + 2..k + 2 + SIDE_BANDS);
                // Above an encoder's cutoff the level only falls (a filter
                // skirt, then the floor); music would come back up.
                let rest = levels[k + 2..top]
                    .iter()
                    .copied()
                    .fold(f64::NEG_INFINITY, f64::max);
                let flat = rest <= levels[k + 2] + FLOOR_FLATNESS_DB;
                let quiet = above <= music - FLOOR_BELOW_MUSIC_DB;
                (flat && quiet).then_some((k, below, below - above))
            })
            .max_by(|a, b| a.2.total_cmp(&b.2))
            .filter(|(_, _, drop)| *drop >= CLIFF_DB);

        if let Some((k, below, drop)) = cliff {
            // The strongest drop can straddle the cliff's lower bands; the
            // cutoff is where the level first falls clearly below the music.
            let cutoff = (k - SIDE_BANDS..=k + 1)
                .take_while(|&b| levels[b] >= below - CUTOFF_FALL_DB)
                .last()
                .map_or(edge(k + 1), |b| edge(b + 1));
            if cutoff < LOSSY_BELOW_HZ {
                let (estimate, likely) = lossy_source(cutoff);
                return SpectrumCheck {
                    verdict: SpectrumVerdict::Lossy,
                    cutoff_hz: Some(cutoff.round() as u32),
                    estimated_kbps: Some(estimate),
                    summary: format!(
                        "audio stops sharply at {} ({drop:.0} dB cliff): made from a lossy file, {likely}",
                        khz(cutoff)
                    ),
                };
            }
        }

        let content_top = (0..levels.len())
            .rev()
            .find(|&b| levels[b] > music - CONTENT_DB)
            .map(|b| edge(b + 1).min(nyquist));
        // A resampler fades out over a few kHz rather than cutting, so an
        // upsampled file shows as content ending near CD's 22 kHz limit
        // with a flat, empty ultrasonic range above (real high-resolution
        // recordings carry noise and harmonics up there, falling or rising,
        // rarely dead flat).
        if self.sample_rate >= 88_200
            && let Some(top) = content_top.filter(|&top| top < UPSAMPLED_BELOW_HZ)
        {
            let ultrasonic: Vec<f64> = (0..levels.len())
                .filter(|&b| {
                    edge(b) >= UPSAMPLED_BELOW_HZ + 1000.0 && edge(b + 1) < nyquist - 1000.0
                })
                .map(|b| levels[b])
                .collect();
            let (low, high) = ultrasonic
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &l| {
                    (lo.min(l), hi.max(l))
                });
            if ultrasonic.len() >= 8 && high - low <= ULTRASONIC_FLATNESS_DB {
                return SpectrumCheck {
                    verdict: SpectrumVerdict::Upsampled,
                    cutoff_hz: Some(top.round() as u32),
                    estimated_kbps: None,
                    summary: format!(
                        "nothing above {}: upsampled from CD-quality audio, not real high resolution",
                        khz(top)
                    ),
                };
            }
        }
        match content_top {
            Some(top) if top >= LOSSY_BELOW_HZ.min(0.9 * nyquist) => SpectrumCheck {
                verdict: SpectrumVerdict::FullBand,
                cutoff_hz: Some(top.round() as u32),
                estimated_kbps: None,
                summary: format!("full spectrum, content up to {}", khz(top)),
            },
            _ => {
                inconclusive("little high-frequency content to tell by; no sign of lossy encoding")
            }
        }
    }
}

fn inconclusive(reason: &str) -> SpectrumCheck {
    SpectrumCheck {
        verdict: SpectrumVerdict::Inconclusive,
        cutoff_hz: None,
        estimated_kbps: None,
        summary: format!("no verdict: {reason}"),
    }
}

fn khz(hz: f64) -> String {
    format!("{:.1} kHz", hz / 1000.0)
}

/// What lossy encode a cutoff points to (roughly; encoders differ), and the
/// bitrate used to rank the file against real lossy copies.
fn lossy_source(cutoff_hz: f64) -> (u32, &'static str) {
    // LAME's default low-pass per bitrate; older encoders and AAC cut lower.
    match cutoff_hz {
        c if c < 11_500.0 => (64, "likely 64 kbps or less"),
        c if c < 15_000.0 => (96, "likely about 96 kbps"),
        c if c < 17_200.0 => (128, "likely MP3 or AAC at about 128 kbps"),
        c if c < 18_000.0 => (160, "likely MP3 at about 160 kbps"),
        c if c < 19_200.0 => (192, "likely MP3 at about 192 kbps"),
        c if c < 19_800.0 => (256, "likely MP3 V0 or 256 kbps"),
        _ => (256, "likely 256–320 kbps MP3, AAC or Opus"),
    }
}

/// In-place iterative radix-2 FFT; `re.len()` must be a power of two.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = -2.0 * PI / len as f64;
        let (wr, wi) = (angle.cos(), angle.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let next = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next;
            }
        }
        len <<= 1;
    }
}

/// Check a local lossless file. `sample_rate` must be the file's own (the
/// audio is not resampled: that would move or hide a cliff).
pub async fn check_file(
    ffmpeg: &Ffmpeg,
    path: &Path,
    sample_rate: u32,
    duration_secs: Option<f64>,
) -> Result<SpectrumCheck, ToolError> {
    let mut analyzer = Analyzer::new(sample_rate, duration_secs);
    ffmpeg
        .decode_mono_f32(StreamInput::LocalFile(path), sample_rate, |samples| {
            analyzer.push(samples)
        })
        .await?;
    Ok(analyzer.finish())
}

/// Check a received or downloaded file whose codec is lossless; `None`
/// for lossy codecs (nothing to prove) or when the check could not run,
/// which is logged: the file is then reported without a verdict rather
/// than held back.
pub async fn check_lossless(
    ffmpeg: &Ffmpeg,
    path: &Path,
    quality: &QualityReport,
    duration_secs: Option<f64>,
) -> Option<SpectrumCheck> {
    if !quality.codec.is_lossless() {
        return None;
    }
    let rate = quality.sample_rate_hz?;
    match check_file(ffmpeg, path, rate, duration_secs).await {
        Ok(check) => {
            tracing::info!(file = %path.display(), verdict = ?check.verdict, summary = %check.summary, "spectrum checked");
            Some(check)
        }
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "spectrum check failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator, so tests need no dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// `secs` of a mix of sines (random frequencies and phases) whose level
    /// at frequency f is `gain(f)`, plus faint noise like 16-bit dither.
    fn signal(rate: u32, secs: f64, top_hz: f64, gain: impl Fn(f64) -> f64) -> Vec<f32> {
        signal_with_dither(rate, secs, top_hz, 2.0 / 32768.0, gain)
    }

    fn signal_with_dither(
        rate: u32,
        secs: f64,
        top_hz: f64,
        dither: f64,
        gain: impl Fn(f64) -> f64,
    ) -> Vec<f32> {
        let mut rng = Lcg(7);
        let tones: Vec<(f64, f64, f64)> = (0..120)
            .map(|_| {
                let f = 60.0 + rng.next() * (top_hz - 60.0);
                (f, rng.next() * 2.0 * PI, gain(f))
            })
            .collect();
        let total: f64 = tones.iter().map(|t| t.2).sum();
        let n = (secs * f64::from(rate)) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let x: f64 = tones
                    .iter()
                    .map(|(f, phase, g)| g * (2.0 * PI * f * t + phase).sin())
                    .sum::<f64>()
                    / total
                    * 0.5;
                (x + (rng.next() - 0.5) * dither) as f32
            })
            .collect()
    }

    fn check(rate: u32, samples: &[f32]) -> SpectrumCheck {
        let mut analyzer = Analyzer::new(rate, None);
        for chunk in samples.chunks(10_000) {
            analyzer.push(chunk);
        }
        analyzer.finish()
    }

    #[test]
    fn fft_finds_a_tone() {
        let mut re: Vec<f64> = (0..64)
            .map(|i| (2.0 * PI * 5.0 * i as f64 / 64.0).cos())
            .collect();
        let mut im = vec![0.0; 64];
        fft(&mut re, &mut im);
        let peak = (0..32)
            .max_by(|&a, &b| (re[a].hypot(im[a])).total_cmp(&re[b].hypot(im[b])))
            .unwrap();
        assert_eq!(peak, 5);
    }

    #[test]
    fn a_lossy_cutoff_is_caught() {
        let mp3_128 = signal(44_100, 3.0, 16_000.0, |_| 1.0);
        let result = check(44_100, &mp3_128);
        assert_eq!(result.verdict, SpectrumVerdict::Lossy, "{result:?}");
        let cutoff = result.cutoff_hz.unwrap();
        assert!((15_500..=16_500).contains(&cutoff), "{cutoff}");
        assert_eq!(result.estimated_kbps, Some(128));

        let opus = signal(48_000, 3.0, 20_000.0, |_| 1.0);
        assert_eq!(check(48_000, &opus).verdict, SpectrumVerdict::Lossy);
    }

    #[test]
    fn full_band_and_gradual_rolloff_are_not_lossy() {
        let cd = signal(44_100, 3.0, 21_800.0, |_| 1.0);
        let result = check(44_100, &cd);
        assert_eq!(result.verdict, SpectrumVerdict::FullBand, "{result:?}");

        // Fading by 6 dB per kHz above 8 kHz: a dull master, not an encoder.
        let dull = signal(44_100, 3.0, 21_800.0, |f| {
            10f64.powf(-6.0 * ((f - 8000.0).max(0.0) / 1000.0) / 20.0)
        });
        let result = check(44_100, &dull);
        assert_ne!(result.verdict, SpectrumVerdict::Lossy, "{result:?}");
    }

    #[test]
    fn upsampled_high_resolution_is_caught() {
        // CD audio resampled into a 24-bit file: nothing but the 24-bit
        // floor above the original range.
        let fake = signal_with_dither(96_000, 2.0, 21_500.0, 2.0 / 8_388_608.0, |_| 1.0);
        let result = check(96_000, &fake);
        assert_eq!(result.verdict, SpectrumVerdict::Upsampled, "{result:?}");
    }

    #[test]
    fn real_high_resolution_is_not_upsampled() {
        let hires = signal_with_dither(96_000, 2.0, 40_000.0, 2.0 / 8_388_608.0, |f| {
            if f > 20_000.0 { 0.05 } else { 1.0 }
        });
        let result = check(96_000, &hires);
        assert_eq!(result.verdict, SpectrumVerdict::FullBand, "{result:?}");
    }

    #[test]
    fn silence_gives_no_verdict() {
        let result = check(44_100, &vec![0f32; 44_100 * 3]);
        assert_eq!(result.verdict, SpectrumVerdict::Inconclusive);
    }
}
