//! Waveforms: how loud a track is over time, drawn by the player as its
//! seek bar.
//!
//! The waveform is measured from the decoded audio itself rather than from
//! file sizes or provider hints, so a breakdown or a drop shows up where it
//! actually plays. Each bar is the RMS loudness of its slice of the track:
//! peak values are almost always at full scale in mastered music, which
//! would draw a flat brick, while RMS shows the track's dynamics.
//!
//! Audio is decoded to mono at a low sample rate. That discards treble, but
//! RMS loudness is dominated by the low end, and it keeps decoding a long DJ
//! mix cheap.

use serde::Serialize;

/// Sample rate the audio is decoded at for measuring.
pub const SAMPLE_RATE: u32 = 8000;
/// Bars in a waveform; the UI resamples them to its width.
pub const BARS: usize = 1600;
/// Samples per measuring window at first: 10 ms.
const INITIAL_WINDOW: u64 = SAMPLE_RATE as u64 / 100;
/// Windows kept at most. When reached, neighbouring windows are merged, so
/// memory stays bounded however long the track is.
const MAX_WINDOWS: usize = 1 << 16;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Waveform {
    /// Length the bars span. Bar `i` of `n` starts at `i / n *
    /// duration_secs`. While measuring, this is the length the track is
    /// expected to have, so measured bars already sit where they play.
    pub duration_secs: f64,
    /// How far measuring has got. Bars starting after this are not
    /// measured yet and are 0.
    pub measured_secs: f64,
    /// Loudness of each bar, scaled so the loudest measured is 255.
    pub bars: Vec<u8>,
    /// Whether the whole track is measured; otherwise this is a snapshot.
    pub complete: bool,
}

/// Accumulates the energy of decoded samples in fixed-length windows.
pub struct Envelope {
    /// Samples per window.
    window: u64,
    /// Sum of squared samples of each completed window.
    sums: Vec<f64>,
    current: f64,
    filled: u64,
    total: u64,
}

impl Default for Envelope {
    fn default() -> Self {
        Self::new()
    }
}

impl Envelope {
    pub fn new() -> Self {
        Self {
            window: INITIAL_WINDOW,
            sums: Vec::new(),
            current: 0.0,
            filled: 0,
            total: 0,
        }
    }

    pub fn push(&mut self, samples: &[i16]) {
        for &sample in samples {
            let v = f64::from(sample) / 32768.0;
            self.current += v * v;
            self.filled += 1;
            if self.filled == self.window {
                self.sums.push(self.current);
                self.current = 0.0;
                self.filled = 0;
                if self.sums.len() == MAX_WINDOWS {
                    self.merge_pairs();
                }
            }
        }
        self.total += samples.len() as u64;
    }

    /// Halve the window count by doubling the window length. Windows stay
    /// aligned: the one being filled starts where a doubled window starts.
    fn merge_pairs(&mut self) {
        self.sums = self.sums.chunks(2).map(|pair| pair.iter().sum()).collect();
        self.window *= 2;
    }

    /// The waveform of everything pushed, with at most `bars` bars, or
    /// `None` if no audio was pushed.
    pub fn finish(mut self, sample_rate: u32, bars: usize) -> Option<Waveform> {
        if self.total == 0 {
            return None;
        }
        let last_len = if self.filled > 0 {
            self.sums.push(self.current);
            self.filled
        } else {
            self.window
        };
        self.render(sample_rate, bars, self.total, last_len, true)
    }

    /// The waveform measured so far, laid out over `expected_secs` (or
    /// what was pushed, if longer or unknown). Only completed windows
    /// count, so a measured bar does not change except in scale.
    pub fn snapshot(
        &self,
        sample_rate: u32,
        bars: usize,
        expected_secs: Option<f64>,
    ) -> Option<Waveform> {
        let expected = expected_secs
            .filter(|s| s.is_finite() && *s > 0.0)
            .map_or(0, |s| (s * f64::from(sample_rate)) as u64);
        let span = expected.max(self.total);
        if span == 0 {
            return None;
        }
        self.render(sample_rate, bars, span, self.window, false)
    }

    /// Bars over `span` samples from the completed windows in `sums`, the
    /// last of which is `last_len` samples long.
    fn render(
        &self,
        sample_rate: u32,
        bars: usize,
        span: u64,
        last_len: u64,
        complete: bool,
    ) -> Option<Waveform> {
        if bars == 0 {
            return None;
        }
        let measured_windows = self.sums.len();
        let windows = (span.div_ceil(self.window) as usize).max(measured_windows);
        let count = bars.min(windows);
        let len_of = |i: usize| {
            if complete && i + 1 == measured_windows {
                last_len
            } else {
                self.window
            }
        };
        // Bars are measured left to right, so the measured ones are a prefix.
        let rms: Vec<f64> = (0..count)
            .map_while(|bar| {
                let range = bar * windows / count..(bar + 1) * windows / count;
                if range.end > measured_windows {
                    return None;
                }
                let energy: f64 = self.sums[range.clone()].iter().sum();
                let samples: u64 = range.map(len_of).sum();
                Some((energy / samples as f64).sqrt())
            })
            .collect();
        let loudest = rms.iter().copied().fold(0.0, f64::max);
        let mut levels: Vec<u8> = rms
            .iter()
            .map(|&r| {
                if loudest > 0.0 {
                    // In 0..=255 by construction.
                    (r / loudest * 255.0).round() as u8
                } else {
                    0
                }
            })
            .collect();
        let measured = levels.len();
        levels.resize(count, 0);
        let duration_secs = span as f64 / f64::from(sample_rate);
        Some(Waveform {
            duration_secs,
            measured_secs: if complete {
                duration_secs
            } else {
                duration_secs * measured as f64 / count as f64
            },
            bars: levels,
            complete,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, amplitude: i16) -> Vec<i16> {
        (0..len)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn bars_follow_loudness_over_time() {
        let mut env = Envelope::new();
        // 2 s silence, 2 s quiet, 4 s loud.
        env.push(&vec![0; 2 * SAMPLE_RATE as usize]);
        env.push(&tone(2 * SAMPLE_RATE as usize, 8000));
        env.push(&tone(4 * SAMPLE_RATE as usize, 32000));
        let wf = env.finish(SAMPLE_RATE, 8).unwrap();
        assert_eq!(wf.duration_secs, 8.0);
        assert_eq!(wf.bars, vec![0, 0, 64, 64, 255, 255, 255, 255]);
    }

    #[test]
    fn short_tracks_get_fewer_bars() {
        let mut env = Envelope::new();
        env.push(&tone(INITIAL_WINDOW as usize * 3 + 5, 1000));
        let wf = env.finish(SAMPLE_RATE, BARS).unwrap();
        assert_eq!(wf.bars.len(), 4);
        assert!(wf.bars.iter().all(|&b| b == 255));
    }

    #[test]
    fn long_tracks_stay_bounded_and_aligned() {
        let mut env = Envelope::new();
        // Enough audio to merge windows twice; quiet first quarter.
        let total = MAX_WINDOWS * INITIAL_WINDOW as usize * 3;
        env.push(&vec![0; total / 4]);
        for chunk in tone(total - total / 4, 20000).chunks(4099) {
            env.push(chunk);
        }
        assert!(env.sums.len() < MAX_WINDOWS);
        assert_eq!(env.window, INITIAL_WINDOW * 4);
        let wf = env.finish(SAMPLE_RATE, 4).unwrap();
        assert_eq!(wf.bars, vec![0, 255, 255, 255]);
        assert_eq!(wf.duration_secs, total as f64 / f64::from(SAMPLE_RATE));
    }

    #[test]
    fn snapshots_place_measured_bars_on_the_expected_timeline() {
        let mut env = Envelope::new();
        // 2 s quiet then 2 s loud of an expected 8 s track.
        env.push(&tone(2 * SAMPLE_RATE as usize, 8000));
        env.push(&tone(2 * SAMPLE_RATE as usize, 32000));
        let wf = env.snapshot(SAMPLE_RATE, 8, Some(8.0)).unwrap();
        assert!(!wf.complete);
        assert_eq!(wf.duration_secs, 8.0);
        assert_eq!(wf.measured_secs, 4.0);
        assert_eq!(wf.bars, vec![64, 64, 255, 255, 0, 0, 0, 0]);

        // Audio running past the expected length extends the span.
        let wf = env.snapshot(SAMPLE_RATE, 8, Some(1.0)).unwrap();
        assert_eq!(wf.duration_secs, 4.0);
        assert_eq!(wf.measured_secs, 4.0);
        let wf = env.snapshot(SAMPLE_RATE, 8, None).unwrap();
        assert_eq!(wf.bars, vec![64, 64, 64, 64, 255, 255, 255, 255]);

        let done = env.finish(SAMPLE_RATE, 8).unwrap();
        assert!(done.complete);
        assert_eq!(done.measured_secs, done.duration_secs);
    }

    #[test]
    fn snapshot_before_any_audio() {
        let env = Envelope::new();
        let wf = env.snapshot(SAMPLE_RATE, 4, Some(2.0)).unwrap();
        assert_eq!(wf.bars, vec![0; 4]);
        assert_eq!(wf.measured_secs, 0.0);
        assert_eq!(env.snapshot(SAMPLE_RATE, 4, None), None);
    }

    #[test]
    fn silence_and_nothing() {
        assert_eq!(Envelope::new().finish(SAMPLE_RATE, BARS), None);
        let mut env = Envelope::new();
        env.push(&[0; 1000]);
        assert!(
            env.finish(SAMPLE_RATE, BARS)
                .unwrap()
                .bars
                .iter()
                .all(|&b| b == 0)
        );
    }
}
