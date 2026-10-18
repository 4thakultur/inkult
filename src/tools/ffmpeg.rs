//! Wrapper around `ffmpeg`.
//!
//! yt-dlp needs ffmpeg to remux extracted audio and convert thumbnails; Inkult
//! additionally uses it directly for artwork post-processing, format
//! conversion and for streaming audio the browser cannot play as-is.

use std::io::Cursor;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, Chain, ReadBuf};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

use super::{AbortOnDrop, ToolError, ToolStatus, failure, read_tail, spawn_error};
use crate::domain::quality::Conversion;

const TOOL: &str = "ffmpeg";
/// Generous bound for re-encoding one track (DJ mixes can be hours long).
const CONVERT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Simultaneous streaming processes. Each listener normally needs one; a few
/// spare slots cover a seek that starts a new process before the browser
/// has dropped the previous request.
const STREAM_SLOTS: usize = 4;
const STREAM_SLOT_WAIT: Duration = Duration::from_secs(10);
/// Time allowed until the first audio bytes arrive (network + seeking).
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(30);
/// Simultaneous whole-track decodes for analysis (waveforms). Each one
/// downloads and decodes a full track, so few run at once.
const ANALYSIS_SLOTS: usize = 2;
const ANALYSIS_SLOT_WAIT: Duration = Duration::from_secs(20);
/// Bound for decoding one track for analysis, download included.
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Demuxers allowed for local audio files: every container Inkult stores.
/// Media files are untrusted input, and ffmpeg probes content rather than
/// trusting the extension; without this a file that is really a playlist
/// (HLS, concat) could make ffmpeg read other local files or the network.
const AUDIO_DEMUXERS: &str = "mov,mp3,ogg,flac,wav,aiff,aac,matroska";
/// Demuxers allowed for local artwork files.
const IMAGE_DEMUXERS: &str = "image2,jpeg_pipe,png_pipe,webp_pipe";

/// Arguments that restrict how ffmpeg may open a local input file.
fn local_input(demuxers: &'static str) -> [&'static str; 4] {
    ["-protocol_whitelist", "file", "-format_whitelist", demuxers]
}

pub struct Ffmpeg {
    program: String,
    stream_slots: Arc<Semaphore>,
    analysis_slots: Arc<Semaphore>,
}

/// Input of a streaming pipe.
pub enum StreamInput<'a> {
    /// An HTTPS HLS playlist and the headers its CDN expects.
    Hls {
        url: &'a Url,
        headers: &'a [(String, String)],
    },
    /// A single HTTPS audio file and the headers its CDN expects.
    Https {
        url: &'a Url,
        headers: &'a [(String, String)],
    },
    LocalFile(&'a Path),
}

impl StreamInput<'_> {
    /// `-i` and the restrictions on what ffmpeg may open for it. Network
    /// inputs may only use the protocols they need; a single remote file
    /// is limited to audio demuxers so a crafted response cannot turn into
    /// a playlist pointing elsewhere.
    fn add_args(&self, cmd: &mut Command) {
        let mut remote = |headers: &[(String, String)], protocols: &str| {
            cmd.args(["-protocol_whitelist", protocols]);
            let headers: String = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            if !headers.is_empty() {
                cmd.arg("-headers").arg(headers);
            }
        };
        match self {
            StreamInput::Hls { url, headers } => {
                remote(headers, "https,tls,tcp,crypto,hls");
                cmd.arg("-i").arg(url.as_str());
            }
            StreamInput::Https { url, headers } => {
                remote(headers, "https,tls,tcp");
                cmd.args(["-format_whitelist", AUDIO_DEMUXERS, "-i", url.as_str()]);
            }
            StreamInput::LocalFile(path) => {
                cmd.args(local_input(AUDIO_DEMUXERS)).arg("-i").arg(path);
            }
        }
    }
}

/// Input of a decode for analysis.
enum DecodeInput<'a> {
    Stream(StreamInput<'a>),
    /// Audio written to ffmpeg's standard input.
    Stdin {
        reader: Box<dyn AsyncRead + Send + Unpin>,
        demuxer: &'static str,
    },
}

impl DecodeInput<'_> {
    /// Add the input arguments; returns what to feed to standard input.
    fn add_args(self, cmd: &mut Command) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        match self {
            DecodeInput::Stream(input) => {
                input.add_args(cmd);
                cmd.stdin(Stdio::null());
                None
            }
            DecodeInput::Stdin { reader, demuxer } => {
                cmd.args(["-protocol_whitelist", "pipe", "-format_whitelist", demuxer])
                    .args(["-f", demuxer, "-i", "pipe:0"])
                    .stdin(Stdio::piped());
                Some(reader)
            }
        }
    }
}

/// Copy `reader` into ffmpeg's standard input while its output is read;
/// closing stdin ends the input.
fn feed(
    reader: Option<Box<dyn AsyncRead + Send + Unpin>>,
    stdin: Option<tokio::process::ChildStdin>,
) -> Option<AbortOnDrop<std::io::Result<()>>> {
    let (mut reader, mut stdin) = (reader?, stdin?);
    Some(AbortOnDrop(tokio::spawn(async move {
        match tokio::io::copy(&mut reader, &mut stdin).await {
            Ok(_) => Ok(()),
            // ffmpeg stopped reading; its exit status tells why.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(e) => Err(e),
        }
    })))
}

/// What a streaming pipe writes. All variants can be produced without
/// knowing the total length up front, which a pipe requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeFormat {
    /// AAC copied into ADTS frames (no re-encode).
    Adts,
    /// MP3 copied as-is (no re-encode).
    Mp3,
    /// Opus copied into Ogg pages (no re-encode).
    Ogg,
    /// Decoded and re-encoded losslessly to FLAC.
    Flac,
}

impl PipeFormat {
    pub fn mime(self) -> &'static str {
        match self {
            PipeFormat::Adts => "audio/aac",
            PipeFormat::Mp3 => "audio/mpeg",
            PipeFormat::Ogg => "audio/ogg",
            PipeFormat::Flac => "audio/flac",
        }
    }

    fn args(self) -> [&'static str; 4] {
        match self {
            PipeFormat::Adts => ["-c:a", "copy", "-f", "adts"],
            PipeFormat::Mp3 => ["-c:a", "copy", "-f", "mp3"],
            PipeFormat::Ogg => ["-c:a", "copy", "-f", "ogg"],
            PipeFormat::Flac => ["-c:a", "flac", "-f", "flac"],
        }
    }
}

/// Audio streamed from a running ffmpeg process. Dropping it kills the
/// process and frees its streaming slot.
pub struct AudioPipe {
    reader: Chain<Cursor<Vec<u8>>, ChildStdout>,
    child: Child,
    stderr: tokio::task::JoinHandle<Result<String, ToolError>>,
    _feeder: Option<AbortOnDrop<std::io::Result<()>>>,
    _slot: OwnedSemaphorePermit,
}

impl AudioPipe {
    /// After reading to the end: whether ffmpeg produced all of the audio,
    /// or stopped early (e.g. the network failed midway).
    pub async fn wait(mut self) -> Result<(), ToolError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|source| ToolError::Io { tool: TOOL, source })?;
        if status.success() {
            return Ok(());
        }
        let stderr = (&mut self.stderr)
            .await
            .unwrap_or_else(|_| Ok(String::new()))?;
        Err(failure(TOOL, status, &stderr))
    }
}

impl AsyncRead for AudioPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl Ffmpeg {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            stream_slots: Arc::new(Semaphore::new(STREAM_SLOTS)),
            analysis_slots: Arc::new(Semaphore::new(ANALYSIS_SLOTS)),
        }
    }

    pub async fn status(&self) -> ToolStatus {
        let mut cmd = Command::new(&self.program);
        cmd.args(["-hide_banner", "-version"]);
        let result = super::run(TOOL, cmd, Duration::from_secs(20), 64 * 1024).await;
        ToolStatus {
            name: TOOL,
            program: self.program.clone(),
            version: result
                .as_ref()
                .ok()
                .map(|o| parse_version(&String::from_utf8_lossy(&o.stdout))),
            error: result.err().map(|e| e.to_string()),
        }
    }

    /// Crop an image to its centred square and write it as JPEG.
    ///
    /// YouTube "art track" thumbnails are 16:9 frames with the square album
    /// cover in the middle; cropping recovers the actual cover.
    pub async fn crop_to_square(&self, input: &Path, output: &Path) -> Result<(), ToolError> {
        self.write_jpeg(input, output, Some("crop='min(iw,ih)':'min(iw,ih)'"))
            .await
    }

    /// Re-encode an image (e.g. a WebP thumbnail) as JPEG, which every
    /// player can show as embedded artwork. `output` must end in `.jpg`.
    pub async fn image_to_jpeg(&self, input: &Path, output: &Path) -> Result<(), ToolError> {
        self.write_jpeg(input, output, None).await
    }

    async fn write_jpeg(
        &self,
        input: &Path,
        output: &Path,
        filter: Option<&str>,
    ) -> Result<(), ToolError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
            .args(local_input(IMAGE_DEMUXERS))
            .arg("-i")
            .arg(input);
        if let Some(filter) = filter {
            cmd.args(["-vf", filter]);
        }
        cmd.args(["-frames:v", "1", "-q:v", "2"]).arg(output);
        super::run(TOOL, cmd, Duration::from_secs(60), 64 * 1024).await?;
        Ok(())
    }

    /// Re-encode the first audio stream of `input` into `output`.
    ///
    /// Metadata and artwork are dropped here and written afterwards by the
    /// tagger, so every output gets the same normalized tags. `-n` makes
    /// ffmpeg refuse to overwrite an existing file.
    pub async fn convert(
        &self,
        input: &Path,
        output: &Path,
        conversion: Conversion,
    ) -> Result<(), ToolError> {
        let codec_args: &[&str] = match conversion {
            Conversion::Mp3Cbr320 => &["-c:a", "libmp3lame", "-b:a", "320k"],
            Conversion::AiffPcm16 => &["-c:a", "pcm_s16be"],
        };
        let mut cmd = Command::new(&self.program);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-n"])
            .args(local_input(AUDIO_DEMUXERS))
            .arg("-i")
            .arg(input)
            .args(["-map", "0:a:0", "-vn", "-map_metadata", "-1"])
            .args(codec_args)
            .arg(output);
        super::run(TOOL, cmd, CONVERT_TIMEOUT, 64 * 1024).await?;
        Ok(())
    }

    /// Start streaming the first audio stream of `input` from `start_secs`.
    ///
    /// Returns once the first audio bytes are available, so an expired URL
    /// or unreadable file is reported as an error rather than as an empty
    /// stream. Network inputs may only use the protocols HLS over HTTPS
    /// needs; local inputs may only read files.
    pub async fn stream_audio(
        &self,
        input: StreamInput<'_>,
        start_secs: f64,
        format: PipeFormat,
    ) -> Result<AudioPipe, ToolError> {
        self.stream_audio_inner(DecodeInput::Stream(input), start_secs, format)
            .await
    }

    /// Like [`Ffmpeg::stream_audio`] from the start, for audio read from
    /// `reader` in the container `demuxer`. Only that demuxer and standard
    /// input may be opened.
    pub async fn stream_audio_from(
        &self,
        reader: impl AsyncRead + Send + Unpin + 'static,
        demuxer: &'static str,
        format: PipeFormat,
    ) -> Result<AudioPipe, ToolError> {
        let input = DecodeInput::Stdin {
            reader: Box::new(reader),
            demuxer,
        };
        self.stream_audio_inner(input, 0.0, format).await
    }

    async fn stream_audio_inner(
        &self,
        input: DecodeInput<'_>,
        start_secs: f64,
        format: PipeFormat,
    ) -> Result<AudioPipe, ToolError> {
        let slot = tokio::time::timeout(
            STREAM_SLOT_WAIT,
            Arc::clone(&self.stream_slots).acquire_owned(),
        )
        .await
        .map_err(|_| ToolError::Busy { tool: TOOL })?
        .map_err(|_| ToolError::Cancelled)?;

        let mut cmd = Command::new(&self.program);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
        if start_secs > 0.0 {
            cmd.arg("-ss").arg(format!("{start_secs:.3}"));
        }
        let reader = input.add_args(&mut cmd);
        cmd.args(["-map", "0:a:0", "-vn", "-map_metadata", "-1"])
            .args(format.args())
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| spawn_error(TOOL, &self.program, e))?;
        let (Some(mut stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ToolError::Io {
                tool: TOOL,
                source: std::io::Error::other("child pipes unavailable"),
            });
        };
        // Keeps draining stderr for the life of the process so ffmpeg never
        // blocks on a full pipe.
        let stderr_task = tokio::spawn(read_tail(stderr, 8 * 1024, TOOL));
        let feeder = feed(reader, child.stdin.take());

        let mut first = vec![0u8; 64 * 1024];
        let read = tokio::time::timeout(STREAM_START_TIMEOUT, stdout.read(&mut first))
            .await
            .map_err(|_| ToolError::Timeout {
                tool: TOOL,
                secs: STREAM_START_TIMEOUT.as_secs(),
            })?
            .map_err(|source| ToolError::Io { tool: TOOL, source })?;
        if read == 0 {
            let status = child
                .wait()
                .await
                .map_err(|source| ToolError::Io { tool: TOOL, source })?;
            let stderr = stderr_task.await.unwrap_or_else(|_| Ok(String::new()))?;
            return Err(if status.success() {
                ToolError::Failed {
                    tool: TOOL,
                    status: status.to_string(),
                    message: "no audio at this position".into(),
                }
            } else {
                failure(TOOL, status, &stderr)
            });
        }
        first.truncate(read);
        Ok(AudioPipe {
            reader: Cursor::new(first).chain(stdout),
            child,
            stderr: stderr_task,
            _feeder: feeder,
            _slot: slot,
        })
    }

    /// Decode the whole first audio stream of `input` to mono 16-bit PCM at
    /// `sample_rate`, handing the samples to `sink` as they arrive so a
    /// long track never sits in memory. Returns the number of samples.
    pub async fn decode_mono(
        &self,
        input: StreamInput<'_>,
        sample_rate: u32,
        sink: impl FnMut(&[i16]),
    ) -> Result<u64, ToolError> {
        self.decode_mono_inner(DecodeInput::Stream(input), sample_rate, sink)
            .await
    }

    /// Like [`Ffmpeg::decode_mono`], as 32-bit float samples: for analysis
    /// that must not have 16-bit quantization noise added (it would bury
    /// the quiet ultrasonic content of high-resolution files).
    pub async fn decode_mono_f32(
        &self,
        input: StreamInput<'_>,
        sample_rate: u32,
        sink: impl FnMut(&[f32]),
    ) -> Result<u64, ToolError> {
        self.decode_mono_inner(DecodeInput::Stream(input), sample_rate, sink)
            .await
    }

    /// Like [`Ffmpeg::decode_mono`], for audio read from `reader` in the
    /// container `demuxer` (e.g. `aac` for ADTS). Only that demuxer and
    /// standard input may be opened. An error reading `reader` fails the
    /// decode rather than ending the audio early.
    pub async fn decode_mono_from(
        &self,
        reader: impl AsyncRead + Send + Unpin + 'static,
        demuxer: &'static str,
        sample_rate: u32,
        sink: impl FnMut(&[i16]),
    ) -> Result<u64, ToolError> {
        let input = DecodeInput::Stdin {
            reader: Box::new(reader),
            demuxer,
        };
        self.decode_mono_inner(input, sample_rate, sink).await
    }

    async fn decode_mono_inner<S: PcmSample>(
        &self,
        input: DecodeInput<'_>,
        sample_rate: u32,
        mut sink: impl FnMut(&[S]),
    ) -> Result<u64, ToolError> {
        let _slot = tokio::time::timeout(
            ANALYSIS_SLOT_WAIT,
            Arc::clone(&self.analysis_slots).acquire_owned(),
        )
        .await
        .map_err(|_| ToolError::Busy { tool: TOOL })?
        .map_err(|_| ToolError::Cancelled)?;

        let mut cmd = Command::new(&self.program);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
        let reader = input.add_args(&mut cmd);
        cmd.args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "1"])
            .arg("-ar")
            .arg(sample_rate.to_string())
            .args(["-c:a", S::CODEC, "-f", S::FORMAT, "pipe:1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| spawn_error(TOOL, &self.program, e))?;
        let (Some(mut stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ToolError::Io {
                tool: TOOL,
                source: std::io::Error::other("child pipes unavailable"),
            });
        };

        let feeder = feed(reader, child.stdin.take());
        let work = async {
            let stderr_task = tokio::spawn(read_tail(stderr, 8 * 1024, TOOL));
            let mut buf = vec![0u8; 64 * 1024];
            let mut samples = Vec::with_capacity(buf.len() / S::BYTES);
            // A read may end in the middle of a sample.
            let mut carry: Vec<u8> = Vec::with_capacity(S::BYTES);
            let mut total: u64 = 0;
            loop {
                let read = stdout
                    .read(&mut buf)
                    .await
                    .map_err(|source| ToolError::Io { tool: TOOL, source })?;
                if read == 0 {
                    break;
                }
                samples.clear();
                let mut bytes = &buf[..read];
                if !carry.is_empty() {
                    let missing = (S::BYTES - carry.len()).min(bytes.len());
                    carry.extend_from_slice(&bytes[..missing]);
                    bytes = &bytes[missing..];
                    if carry.len() == S::BYTES {
                        samples.push(S::from_le(&carry));
                        carry.clear();
                    }
                }
                let whole = bytes.chunks_exact(S::BYTES);
                carry.extend_from_slice(whole.remainder());
                samples.extend(whole.map(S::from_le));
                total += samples.len() as u64;
                sink(&samples);
            }
            let status = child
                .wait()
                .await
                .map_err(|source| ToolError::Io { tool: TOOL, source })?;
            let stderr = stderr_task.await.unwrap_or_else(|_| Ok(String::new()))?;
            if let Some(mut feeder) = feeder {
                // The input was cut short: what was decoded is not the track.
                if let Ok(Err(source)) = (&mut feeder.0).await {
                    return Err(ToolError::Io { tool: TOOL, source });
                }
            }
            if !status.success() {
                return Err(failure(TOOL, status, &stderr));
            }
            if total == 0 {
                return Err(ToolError::Failed {
                    tool: TOOL,
                    status: status.to_string(),
                    message: "no audio decoded".into(),
                });
            }
            Ok(total)
        };
        tokio::time::timeout(ANALYSIS_TIMEOUT, work)
            .await
            .map_err(|_| ToolError::Timeout {
                tool: TOOL,
                secs: ANALYSIS_TIMEOUT.as_secs(),
            })?
    }
}

/// A raw little-endian PCM sample type ffmpeg can write to a pipe.
trait PcmSample: Copy {
    const CODEC: &'static str;
    const FORMAT: &'static str;
    const BYTES: usize;
    /// `bytes` is exactly `BYTES` long.
    fn from_le(bytes: &[u8]) -> Self;
}

impl PcmSample for i16 {
    const CODEC: &'static str = "pcm_s16le";
    const FORMAT: &'static str = "s16le";
    const BYTES: usize = 2;
    fn from_le(bytes: &[u8]) -> Self {
        i16::from_le_bytes([bytes[0], bytes[1]])
    }
}

impl PcmSample for f32 {
    const CODEC: &'static str = "pcm_f32le";
    const FORMAT: &'static str = "f32le";
    const BYTES: usize = 4;
    fn from_le(bytes: &[u8]) -> Self {
        f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

/// "ffmpeg version 7.1.1-1 Copyright (c) ..." → "7.1.1-1".
fn parse_version(output: &str) -> String {
    let first_line = output.lines().next().unwrap_or_default().trim();
    first_line
        .strip_prefix("ffmpeg version ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or(first_line)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the real ffmpeg (as the Soulseek end-to-end tests do).
    #[tokio::test]
    async fn covers_become_jpeg_with_or_without_cropping() {
        let dir = tempfile::tempdir().unwrap();
        let thumbnail = dir.path().join("thumb.download");
        let made = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .args(["color=red:s=160x90", "-frames:v", "1", "-f", "webp"])
            .arg(&thumbnail)
            .status()
            .expect("ffmpeg runs");
        assert!(made.success());
        let ffmpeg = Ffmpeg::new("ffmpeg");
        for (crop, size) in [(false, (160, 90)), (true, (90, 90))] {
            let out = dir.path().join(format!("cover-{crop}.jpg"));
            if crop {
                ffmpeg.crop_to_square(&thumbnail, &out).await.unwrap();
            } else {
                ffmpeg.image_to_jpeg(&thumbnail, &out).await.unwrap();
            }
            let data = std::fs::read(&out).unwrap();
            assert!(data.starts_with(&[0xFF, 0xD8, 0xFF]), "JPEG");
            let probe = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=width,height",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&out)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&probe.stdout).trim(),
                format!("{},{}", size.0, size.1)
            );
        }
    }

    #[test]
    fn extracts_version_token() {
        assert_eq!(
            parse_version("ffmpeg version 7.1.1-1+b1 Copyright (c) 2000-2025\nbuilt with gcc"),
            "7.1.1-1+b1"
        );
        assert_eq!(parse_version("something else"), "something else");
    }
}
