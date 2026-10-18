//! Audio quality model and source-format selection.
//!
//! Selection rules, in priority order:
//!
//! 1. Never pick a dynamic-range-compressed variant (YouTube's `-drc` formats)
//!    when an unprocessed one exists: DRC alters the audio itself.
//! 2. A lossless codec always beats any lossy codec.
//! 3. Between lossless formats, prefer higher sample rate, then bitrate.
//! 4. Between lossy formats, compare *codec-weighted* bitrate rather than raw
//!    bitrate: Opus at 160 kbps is perceptually better than AAC at 128 kbps
//!    and roughly on par with MP3 at 256 kbps. The weights are deliberately
//!    coarse; they only need to order the handful of formats a provider offers.
//! 5. Prefer audio-only streams over muxed audio+video (same audio, less data).
//!
//! We never transcode as part of downloading, so the selected source format
//! *is* the downloaded quality. A lossless codec from the source only means
//! the provider delivered lossless data; it cannot prove the upload itself was
//! not made from a lossy master.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Flac,
    Alac,
    /// Uncompressed PCM (WAV/AIFF).
    Pcm,
    Opus,
    Aac,
    Vorbis,
    Mp3,
    Other,
}

impl Codec {
    /// Determine the codec from a yt-dlp style `acodec` string, falling back to
    /// the container extension when the codec is not reported.
    pub fn detect(acodec: Option<&str>, extension: Option<&str>) -> Codec {
        if let Some(codec) = acodec.map(str::to_ascii_lowercase) {
            let codec = codec.as_str();
            let detected = match codec {
                c if c.starts_with("opus") => Some(Codec::Opus),
                c if c.starts_with("mp4a.40.34") || c.starts_with("mp3") => Some(Codec::Mp3),
                c if c.starts_with("mp4a") || c.starts_with("aac") => Some(Codec::Aac),
                c if c.starts_with("flac") => Some(Codec::Flac),
                c if c.starts_with("alac") => Some(Codec::Alac),
                c if c.starts_with("vorbis") => Some(Codec::Vorbis),
                c if c.starts_with("pcm") || c == "wav" => Some(Codec::Pcm),
                _ => None,
            };
            if let Some(codec) = detected {
                return codec;
            }
        }
        match extension.map(str::to_ascii_lowercase).as_deref() {
            Some("flac") => Codec::Flac,
            Some("wav" | "aiff" | "aif") => Codec::Pcm,
            Some("opus") => Codec::Opus,
            Some("mp3") => Codec::Mp3,
            Some("m4a" | "aac") => Codec::Aac,
            Some("ogg" | "oga") => Codec::Vorbis,
            _ => Codec::Other,
        }
    }

    pub fn is_lossless(self) -> bool {
        matches!(self, Codec::Flac | Codec::Alac | Codec::Pcm)
    }

    /// Rough perceptual efficiency relative to MP3 at the same bitrate.
    fn lossy_efficiency(self) -> f64 {
        match self {
            Codec::Opus => 1.5,
            Codec::Aac | Codec::Vorbis => 1.2,
            Codec::Mp3 => 1.0,
            Codec::Other => 0.9,
            // Lossless codecs are ranked separately; the weight is irrelevant.
            Codec::Flac | Codec::Alac | Codec::Pcm => 1.0,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Codec::Flac => "FLAC",
            Codec::Alac => "ALAC",
            Codec::Pcm => "PCM",
            Codec::Opus => "Opus",
            Codec::Aac => "AAC",
            Codec::Vorbis => "Vorbis",
            Codec::Mp3 => "MP3",
            Codec::Other => "Unknown",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// One downloadable audio rendition offered by a provider.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioFormat {
    /// Provider-specific identifier used to request this exact rendition.
    pub id: String,
    pub codec: Codec,
    pub bitrate_kbps: Option<f64>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u32>,
    pub container: Option<String>,
    pub has_video: bool,
    pub dynamic_range_compressed: bool,
    pub note: Option<String>,
}

impl AudioFormat {
    fn lossy_score(&self) -> f64 {
        self.bitrate_kbps.unwrap_or(0.0) * self.codec.lossy_efficiency()
    }

    /// Human-readable description such as "Opus · 160 kbps · 48 kHz".
    pub fn summary(&self) -> String {
        describe(
            self.codec,
            self.bitrate_kbps.map(|b| b.round() as u32),
            self.sample_rate_hz,
            None,
        )
    }
}

/// Total order over formats: `Greater` means "better source".
fn compare(a: &AudioFormat, b: &AudioFormat) -> Ordering {
    let lossless = |f: &AudioFormat| f.codec.is_lossless();
    // `false < true`, so negate DRC/video so that "unprocessed" and
    // "audio-only" rank higher.
    (!a.dynamic_range_compressed)
        .cmp(&!b.dynamic_range_compressed)
        .then_with(|| lossless(a).cmp(&lossless(b)))
        .then_with(|| {
            if lossless(a) && lossless(b) {
                a.sample_rate_hz
                    .cmp(&b.sample_rate_hz)
                    .then_with(|| a.lossy_score().total_cmp(&b.lossy_score()))
            } else {
                a.lossy_score().total_cmp(&b.lossy_score())
            }
        })
        .then_with(|| (!a.has_video).cmp(&!b.has_video))
        .then_with(|| a.sample_rate_hz.cmp(&b.sample_rate_hz))
}

/// Pick the best source rendition, or `None` if no format carries audio.
pub fn select_best(formats: &[AudioFormat]) -> Option<&AudioFormat> {
    formats.iter().max_by(|a, b| compare(a, b))
}

/// The file format the user wants in their library.
///
/// Only [`OutputFormat::Mp3_320`] and [`OutputFormat::Aiff`] re-encode.
/// Re-encoding cannot add quality: an MP3 320 made from a 160 kbps stream
/// is bigger and more compatible, never better. Those options exist for
/// players and DJ gear that cannot read Opus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutputFormat {
    /// Best source format, kept exactly as delivered.
    #[default]
    #[serde(rename = "original")]
    Original,
    /// Best AAC stream the source offers, kept as delivered (no re-encode).
    #[serde(rename = "aac")]
    Aac,
    /// Best source, re-encoded to constant-bitrate 320 kbps MP3.
    #[serde(rename = "mp3_320")]
    Mp3_320,
    /// Best source, decoded to uncompressed 16-bit PCM in AIFF. No further
    /// loss beyond the source, at roughly 10 MB per minute.
    #[serde(rename = "aiff")]
    Aiff,
    /// A lossless copy of the track found elsewhere (on Soulseek) by a
    /// quality upgrade. Never downloaded from the track's own provider or
    /// made by converting: no local file or lossy stream can become one.
    #[serde(rename = "lossless")]
    Lossless,
}

/// A re-encode applied after download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    Mp3Cbr320,
    AiffPcm16,
}

impl Conversion {
    pub fn codec(self) -> Codec {
        match self {
            Conversion::Mp3Cbr320 => Codec::Mp3,
            Conversion::AiffPcm16 => Codec::Pcm,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Conversion::Mp3Cbr320 => "mp3",
            Conversion::AiffPcm16 => "aiff",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown output format '{0}'")]
pub struct UnknownOutputFormat(String);

impl std::str::FromStr for OutputFormat {
    type Err = UnknownOutputFormat;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        OutputFormat::ALL
            .into_iter()
            .find(|f| f.slug() == s)
            .ok_or_else(|| UnknownOutputFormat(s.to_owned()))
    }
}

impl OutputFormat {
    /// Stable identifier, identical to the serialized form.
    pub fn slug(self) -> &'static str {
        match self {
            OutputFormat::Original => "original",
            OutputFormat::Aac => "aac",
            OutputFormat::Mp3_320 => "mp3_320",
            OutputFormat::Aiff => "aiff",
            OutputFormat::Lossless => "lossless",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Original => "Best quality (original)",
            OutputFormat::Aac => "AAC (M4A), no re-encoding",
            OutputFormat::Mp3_320 => "MP3 320 kbps",
            OutputFormat::Aiff => "AIFF",
            OutputFormat::Lossless => "Lossless (quality upgrade)",
        }
    }

    pub fn conversion(self) -> Option<Conversion> {
        match self {
            OutputFormat::Original | OutputFormat::Aac | OutputFormat::Lossless => None,
            OutputFormat::Mp3_320 => Some(Conversion::Mp3Cbr320),
            OutputFormat::Aiff => Some(Conversion::AiffPcm16),
        }
    }

    /// How to make this output from a file already in the library, or why
    /// that is impossible. Only re-encoding outputs qualify: "original" and
    /// "AAC without re-encoding" mean the provider's own stream, which no
    /// local file can be turned back into. Re-encoding a file into its own
    /// codec is refused, since it would only lose quality.
    pub fn library_conversion(self, source: &QualityReport) -> Result<Conversion, String> {
        match self {
            OutputFormat::Original => {
                Err("the original can only come from the source; download it instead".into())
            }
            OutputFormat::Aac => Err(
                "AAC without re-encoding needs the source's own AAC stream; download it instead"
                    .into(),
            ),
            OutputFormat::Lossless => Err(
                "a lossless copy cannot be made from a library file; use Quality upgrade".into(),
            ),
            OutputFormat::Mp3_320 | OutputFormat::Aiff => {
                let conversion = self.conversion().ok_or("no conversion for this format")?;
                if source.codec == conversion.codec() {
                    Err(format!(
                        "already {}; re-encoding it to itself would only lose quality",
                        source.codec
                    ))
                } else {
                    Ok(conversion)
                }
            }
        }
    }

    /// All formats, in menu order.
    pub const ALL: [OutputFormat; 5] = [
        OutputFormat::Original,
        OutputFormat::Aac,
        OutputFormat::Mp3_320,
        OutputFormat::Aiff,
        OutputFormat::Lossless,
    ];

    /// Whether a provider download can produce this format. A lossless
    /// upgrade only comes from a quality upgrade: labelling a provider's
    /// lossy stream "lossless" would be a lie.
    pub fn is_downloadable(self) -> bool {
        self != OutputFormat::Lossless
    }

    /// The source rendition to download for this output. Conversions always
    /// start from the best source, since every generation of lossy encoding
    /// compounds the loss of the previous one.
    pub fn select_source(self, formats: &[AudioFormat]) -> Option<&AudioFormat> {
        match self {
            OutputFormat::Aac => {
                let aac: Vec<AudioFormat> = formats
                    .iter()
                    .filter(|f| f.codec == Codec::Aac)
                    .cloned()
                    .collect();
                let best = select_best(&aac)?;
                formats.iter().find(|f| f.id == best.id)
            }
            _ => select_best(formats),
        }
    }
}

/// Quality of the file that actually ended up on disk, measured from the
/// file itself after download rather than trusted from provider claims.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    pub codec: Codec,
    pub container: String,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    /// True only if the delivered codec is lossless. See module docs.
    pub lossless: bool,
    /// Whether Inkult re-encoded the audio (only on explicit request).
    pub transcoded: bool,
    /// Source quality when the file was re-encoded, e.g. "Opus · 48 kHz · 135 kbps".
    #[serde(default)]
    pub converted_from: Option<String>,
    pub summary: String,
    /// Spectrum check of a file in a lossless codec (see `crate::spectrum`).
    /// A file found to be made from lossy audio is not `lossless`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spectrum: Option<SpectrumCheck>,
}

/// What the spectrum of a lossless-codec file says about its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpectrumVerdict {
    /// Content up to the top of the range: no sign of lossy encoding.
    FullBand,
    /// A lossy encoder's cutoff: decoded from a lossy file.
    Lossy,
    /// High-resolution container around CD-rate audio.
    Upsampled,
    /// Too quiet or too dull at the top to tell.
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpectrumCheck {
    pub verdict: SpectrumVerdict,
    /// Where the audio content ends, when found.
    pub cutoff_hz: Option<u32>,
    /// Bitrate of the lossy encode a cutoff suggests (rough; for ranking).
    pub estimated_kbps: Option<u32>,
    /// For people, e.g. "audio stops sharply at 16.0 kHz …".
    pub summary: String,
}

impl QualityReport {
    pub fn new(
        codec: Codec,
        container: String,
        bitrate_kbps: Option<u32>,
        sample_rate_hz: Option<u32>,
        bit_depth: Option<u8>,
        channels: Option<u8>,
    ) -> Self {
        Self {
            codec,
            container,
            bitrate_kbps,
            sample_rate_hz,
            bit_depth,
            channels,
            lossless: codec.is_lossless(),
            transcoded: false,
            converted_from: None,
            summary: describe(codec, bitrate_kbps, sample_rate_hz, bit_depth),
            spectrum: None,
        }
    }

    /// Record a spectrum check. A file shown to be decoded from lossy audio
    /// keeps its codec but is no longer called lossless.
    pub fn with_spectrum(mut self, check: SpectrumCheck) -> Self {
        if check.verdict == SpectrumVerdict::Lossy {
            self.lossless = false;
        }
        self.spectrum = Some(check);
        self
    }

    /// Whether the spectrum check found lossy audio in this file.
    pub fn spectrum_lossy(&self) -> bool {
        self.spectrum
            .as_ref()
            .is_some_and(|s| s.verdict == SpectrumVerdict::Lossy)
    }

    /// Mark this file as re-encoded from `source`. A lossless container made
    /// from a lossy source is not lossless audio, so `lossless` requires both.
    pub fn converted_from(mut self, source: &AudioFormat) -> Self {
        self.transcoded = true;
        self.lossless = self.codec.is_lossless() && source.codec.is_lossless();
        self.converted_from = Some(source.summary());
        self
    }

    /// Mark this file as converted from another library file. The history
    /// is kept: an AIFF made from an MP3 that was itself made from Opus
    /// says so, and is lossless only if every step was.
    pub fn converted_from_file(mut self, source: &QualityReport) -> Self {
        self.transcoded = true;
        self.lossless = self.codec.is_lossless() && source.lossless;
        self.converted_from = Some(match &source.converted_from {
            Some(earlier) => format!("{} ← {earlier}", source.summary),
            None => source.summary.clone(),
        });
        self
    }

    /// Rank library copies of one track as conversion sources: an original
    /// download first (no generation loss yet), then uncompressed or
    /// lossless containers (decoding added no loss), then codec-weighted
    /// bitrate, the same perceptual ordering used to pick download sources
    /// (Opus at 103 kbps beats AAC at 127 kbps).
    ///
    /// A lossless container shown by its spectrum to hold decoded lossy
    /// audio ranks as the lossy encode it came from, so a fake FLAC of an
    /// MP3 128 is not preferred over a real Opus 135.
    pub fn source_rank(&self) -> (bool, bool, u32) {
        let (kbps, efficiency) = match self.spectrum.as_ref().and_then(|s| s.estimated_kbps) {
            Some(estimate) if self.spectrum_lossy() => (estimate, 1.0),
            _ => (
                self.bitrate_kbps.unwrap_or(0),
                self.codec.lossy_efficiency(),
            ),
        };
        let weighted = f64::from(kbps) * efficiency * 10.0;
        let lossless = self.codec.is_lossless() && !self.spectrum_lossy();
        (!self.transcoded, lossless, weighted as u32)
    }

    /// Orders files by how much of the recording they actually carry, for
    /// sorting a library by quality (higher is better): lossless audio
    /// first, by bit depth then sample rate; then lossy originals by
    /// codec-weighted bitrate; then lossy re-encodes, whose real quality is
    /// bounded by a source they no longer carry, however high their own
    /// bitrate (an MP3 320 made from Opus 132 is not better than the Opus).
    pub fn library_rank(&self) -> u64 {
        const TIER: u64 = 1_000_000_000_000;
        if self.lossless {
            let depth = u64::from(self.bit_depth.unwrap_or(0));
            let rate = u64::from(self.sample_rate_hz.unwrap_or(0)).min(9_999_999);
            return 3 * TIER + depth * 10_000_000 + rate;
        }
        let (original, _, weighted) = self.source_rank();
        let tier = if original { 2 } else { 1 };
        tier * TIER + u64::from(weighted)
    }
}

fn describe(
    codec: Codec,
    bitrate_kbps: Option<u32>,
    sample_rate_hz: Option<u32>,
    bit_depth: Option<u8>,
) -> String {
    let mut parts = vec![codec.display_name().to_owned()];
    if let Some(depth) = bit_depth.filter(|_| codec.is_lossless()) {
        parts.push(format!("{depth}-bit"));
    }
    if let Some(rate) = sample_rate_hz {
        let khz = f64::from(rate) / 1000.0;
        parts.push(if rate % 1000 == 0 {
            format!("{khz:.0} kHz")
        } else {
            format!("{khz:.1} kHz")
        });
    }
    if let Some(bitrate) = bitrate_kbps {
        parts.push(format!("{bitrate} kbps"));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(id: &str, codec: Codec, kbps: f64) -> AudioFormat {
        AudioFormat {
            id: id.to_owned(),
            codec,
            bitrate_kbps: Some(kbps),
            sample_rate_hz: Some(48_000),
            channels: Some(2),
            container: None,
            has_video: false,
            dynamic_range_compressed: false,
            note: None,
        }
    }

    #[test]
    fn detects_codecs_from_ytdlp_strings() {
        assert_eq!(Codec::detect(Some("opus"), Some("webm")), Codec::Opus);
        assert_eq!(Codec::detect(Some("mp4a.40.2"), Some("m4a")), Codec::Aac);
        assert_eq!(Codec::detect(Some("mp4a.40.34"), None), Codec::Mp3);
        assert_eq!(Codec::detect(None, Some("wav")), Codec::Pcm);
        assert_eq!(Codec::detect(Some("weird"), Some("flac")), Codec::Flac);
        assert_eq!(Codec::detect(None, None), Codec::Other);
    }

    #[test]
    fn prefers_opus_over_lower_effective_aac() {
        // Typical YouTube offering: 140 (AAC ~129k) and 251 (Opus ~135k).
        let formats = [
            fmt("140", Codec::Aac, 129.0),
            fmt("251", Codec::Opus, 135.0),
        ];
        assert_eq!(select_best(&formats).unwrap().id, "251");
    }

    #[test]
    fn prefers_high_bitrate_aac_over_low_opus() {
        // YouTube Premium AAC 256k beats standard Opus.
        let formats = [
            fmt("141", Codec::Aac, 256.0),
            fmt("251", Codec::Opus, 135.0),
        ];
        assert_eq!(select_best(&formats).unwrap().id, "141");
    }

    #[test]
    fn lossless_beats_any_lossy() {
        let formats = [
            fmt("mp3", Codec::Mp3, 320.0),
            fmt("orig", Codec::Pcm, 0.0),
            fmt("opus", Codec::Opus, 256.0),
        ];
        assert_eq!(select_best(&formats).unwrap().id, "orig");
    }

    #[test]
    fn never_prefers_drc_variant() {
        let mut drc = fmt("251-drc", Codec::Opus, 140.0);
        drc.dynamic_range_compressed = true;
        let formats = [drc, fmt("251", Codec::Opus, 135.0)];
        assert_eq!(select_best(&formats).unwrap().id, "251");
    }

    #[test]
    fn prefers_audio_only_when_equal() {
        let mut muxed = fmt("18", Codec::Aac, 128.0);
        muxed.has_video = true;
        let formats = [muxed, fmt("140", Codec::Aac, 128.0)];
        assert_eq!(select_best(&formats).unwrap().id, "140");
    }

    #[test]
    fn aac_output_picks_best_aac_without_reencoding() {
        let formats = [
            fmt("139", Codec::Aac, 48.0),
            fmt("251", Codec::Opus, 135.0),
            fmt("140", Codec::Aac, 129.0),
        ];
        assert_eq!(OutputFormat::Aac.select_source(&formats).unwrap().id, "140");
        assert_eq!(OutputFormat::Aac.conversion(), None);
        assert!(OutputFormat::Aac.select_source(&formats[1..2]).is_none());
    }

    #[test]
    fn conversions_start_from_the_best_source() {
        let formats = [
            fmt("140", Codec::Aac, 129.0),
            fmt("251", Codec::Opus, 135.0),
        ];
        assert_eq!(
            OutputFormat::Mp3_320.select_source(&formats).unwrap().id,
            "251"
        );
        assert_eq!(OutputFormat::Aiff.conversion(), Some(Conversion::AiffPcm16));
    }

    #[test]
    fn output_format_wire_names() {
        assert_eq!(
            serde_json::to_string(&OutputFormat::Mp3_320).unwrap(),
            "\"mp3_320\""
        );
        let parsed: OutputFormat = serde_json::from_str("\"aiff\"").unwrap();
        assert_eq!(parsed, OutputFormat::Aiff);
        for output in [
            OutputFormat::Original,
            OutputFormat::Aac,
            OutputFormat::Mp3_320,
            OutputFormat::Aiff,
            OutputFormat::Lossless,
        ] {
            assert_eq!(output.slug().parse::<OutputFormat>().unwrap(), output);
            assert_eq!(
                serde_json::to_string(&output).unwrap(),
                format!("\"{}\"", output.slug())
            );
        }
    }

    #[test]
    fn decoded_lossy_source_is_not_lossless() {
        let source = fmt("251", Codec::Opus, 135.0);
        let aiff = QualityReport::new(
            Codec::Pcm,
            "aiff".into(),
            Some(1536),
            Some(48_000),
            Some(16),
            Some(2),
        )
        .converted_from(&source);
        assert!(!aiff.lossless);
        assert!(aiff.transcoded);
        assert_eq!(
            aiff.converted_from.as_deref(),
            Some("Opus · 48 kHz · 135 kbps")
        );

        let flac_source = fmt("orig", Codec::Flac, 900.0);
        let from_flac = QualityReport::new(
            Codec::Pcm,
            "aiff".into(),
            None,
            Some(44_100),
            Some(16),
            None,
        )
        .converted_from(&flac_source);
        assert!(from_flac.lossless);
    }

    fn report(codec: Codec, kbps: u32) -> QualityReport {
        QualityReport::new(
            codec,
            "x".into(),
            Some(kbps),
            Some(48_000),
            Some(16),
            Some(2),
        )
    }

    #[test]
    fn library_conversion_rules() {
        let opus = report(Codec::Opus, 135);
        assert_eq!(
            OutputFormat::Mp3_320.library_conversion(&opus),
            Ok(Conversion::Mp3Cbr320)
        );
        assert_eq!(
            OutputFormat::Aiff.library_conversion(&opus),
            Ok(Conversion::AiffPcm16)
        );
        assert!(OutputFormat::Original.library_conversion(&opus).is_err());
        assert!(OutputFormat::Aac.library_conversion(&opus).is_err());
        let mp3 = report(Codec::Mp3, 320);
        assert!(OutputFormat::Mp3_320.library_conversion(&mp3).is_err());
        assert!(OutputFormat::Aiff.library_conversion(&mp3).is_ok());
        assert!(
            OutputFormat::Aiff
                .library_conversion(&report(Codec::Pcm, 1536))
                .is_err()
        );
        let flac = report(Codec::Flac, 900);
        assert!(OutputFormat::Lossless.library_conversion(&flac).is_err());
        assert!(!OutputFormat::Lossless.is_downloadable());
        assert!(OutputFormat::Original.is_downloadable());
    }

    #[test]
    fn file_conversions_keep_history_and_honesty() {
        let opus = report(Codec::Opus, 135);
        let mp3 = report(Codec::Mp3, 320).converted_from_file(&opus);
        assert!(mp3.transcoded && !mp3.lossless);
        assert_eq!(mp3.converted_from.as_deref(), Some(opus.summary.as_str()));

        // AIFF from that MP3: still not lossless, and the chain is visible.
        let aiff = report(Codec::Pcm, 1536).converted_from_file(&mp3);
        assert!(!aiff.lossless);
        assert_eq!(
            aiff.converted_from.as_deref().unwrap(),
            format!("{} ← {}", mp3.summary, opus.summary)
        );

        // Lossless in, lossless out.
        let flac = report(Codec::Flac, 900);
        assert!(report(Codec::Pcm, 1536).converted_from_file(&flac).lossless);

        // Codec weighting, as for downloads: Opus 103 beats AAC 127.
        assert!(report(Codec::Opus, 103).source_rank() > report(Codec::Aac, 127).source_rank());

        // The original beats any conversion, however high its bitrate.
        assert!(opus.source_rank() > mp3.source_rank());
        assert!(aiff.source_rank() > mp3.source_rank());
    }

    #[test]
    fn library_rank_orders_by_real_quality() {
        let opus = report(Codec::Opus, 132);
        let mp3 = report(Codec::Mp3, 320).converted_from_file(&opus);
        let aiff = report(Codec::Pcm, 1536).converted_from_file(&opus);
        let flac = report(Codec::Flac, 900);
        let mut hires = report(Codec::Flac, 2500);
        hires.bit_depth = Some(24);

        assert!(hires.library_rank() > flac.library_rank());
        // A lossless copy of lossless audio is still lossless.
        let flac_aiff = report(Codec::Pcm, 1536).converted_from_file(&flac);
        assert_eq!(flac_aiff.library_rank(), flac.library_rank());
        assert!(flac.library_rank() > report(Codec::Aac, 256).library_rank());
        assert!(report(Codec::Aac, 256).library_rank() > opus.library_rank());
        // Re-encodes of lossy audio rank below every lossy original.
        assert!(report(Codec::Mp3, 128).library_rank() > mp3.library_rank());
        assert!(opus.library_rank() > aiff.library_rank());
    }

    #[test]
    fn spectrum_exposes_fake_lossless() {
        let flac = report(Codec::Flac, 900);
        let fake = flac.clone().with_spectrum(SpectrumCheck {
            verdict: SpectrumVerdict::Lossy,
            cutoff_hz: Some(16_000),
            estimated_kbps: Some(128),
            summary: "cut".into(),
        });
        assert!(!fake.lossless);
        assert!(fake.library_rank() < flac.library_rank());
        assert!(fake.source_rank() < report(Codec::Opus, 135).source_rank());
        assert!(fake.library_rank() < report(Codec::Aac, 256).library_rank());

        let real = flac.clone().with_spectrum(SpectrumCheck {
            verdict: SpectrumVerdict::FullBand,
            cutoff_hz: Some(21_900),
            estimated_kbps: None,
            summary: "full".into(),
        });
        assert!(real.lossless);
        assert_eq!(real.library_rank(), flac.library_rank());
    }

    #[test]
    fn empty_formats_select_nothing() {
        assert!(select_best(&[]).is_none());
    }

    #[test]
    fn report_never_claims_lossless_for_lossy_codec() {
        let report = QualityReport::new(
            Codec::Opus,
            "opus".into(),
            Some(160),
            Some(48_000),
            None,
            Some(2),
        );
        assert!(!report.lossless);
        assert_eq!(report.summary, "Opus · 48 kHz · 160 kbps");

        let flac = QualityReport::new(
            Codec::Flac,
            "flac".into(),
            Some(900),
            Some(44_100),
            Some(16),
            None,
        );
        assert!(flac.lossless);
        assert_eq!(flac.summary, "FLAC · 16-bit · 44.1 kHz · 900 kbps");
    }
}
