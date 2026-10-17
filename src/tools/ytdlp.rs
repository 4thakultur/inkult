//! Wrapper around the `yt-dlp` command-line tool.
//!
//! yt-dlp is used as an extraction and transport engine only. Inkult decides
//! which format to download (see `domain::quality`), and writes tags and
//! organizes files itself, so yt-dlp's own metadata/thumbnail embedding and
//! output templates are not used for anything user-visible.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{ToolError, ToolStatus, failure, read_tail, spawn_error};
use crate::domain::DownloadProgress;
use crate::domain::quality::{AudioFormat, Codec};

const TOOL: &str = "yt-dlp";
const METADATA_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound on JSON output; large playlists are capped separately.
const MAX_JSON_BYTES: usize = 64 * 1024 * 1024;
/// Concurrent metadata (search/info) invocations; downloads are limited by
/// the job manager instead.
const METADATA_CONCURRENCY: usize = 4;
const PROGRESS_PREFIX: &str = "inkult-progress:";

pub struct YtDlp {
    program: String,
    ffmpeg: String,
    metadata_slots: Semaphore,
}

/// Parameters for downloading one track's audio into a private work dir.
pub struct DownloadRequest<'a> {
    pub url: &'a str,
    pub format_id: &'a str,
    pub work_dir: &'a Path,
}

impl YtDlp {
    pub fn new(program: impl Into<String>, ffmpeg: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ffmpeg: ffmpeg.into(),
            metadata_slots: Semaphore::new(METADATA_CONCURRENCY),
        }
    }

    pub async fn status(&self) -> ToolStatus {
        let mut cmd = Command::new(&self.program);
        cmd.arg("--version");
        let result = super::run(TOOL, cmd, Duration::from_secs(20), 4096).await;
        ToolStatus {
            name: TOOL,
            program: self.program.clone(),
            version: result
                .as_ref()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()),
            error: result.err().map(|e| e.to_string()),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        // Ignore user/system config files so behaviour is predictable and
        // fully determined by Inkult.
        cmd.args(["--ignore-config", "--no-cache-dir"]);
        cmd
    }

    async fn json<T: DeserializeOwned>(&self, cmd: Command) -> Result<T, ToolError> {
        let _slot = self
            .metadata_slots
            .acquire()
            .await
            .map_err(|_| ToolError::Cancelled)?;
        let output = super::run(TOOL, cmd, METADATA_TIMEOUT, MAX_JSON_BYTES).await?;
        serde_json::from_slice(&output.stdout).map_err(|e| ToolError::InvalidOutput {
            tool: TOOL,
            reason: e.to_string(),
        })
    }

    /// Run a flat search, e.g. `ytsearch10:query` or a search-page URL.
    pub async fn search_flat(&self, target: &str, limit: usize) -> Result<YtInfo, ToolError> {
        let mut cmd = self.command();
        cmd.args(["-J", "--flat-playlist", "--playlist-end"])
            .arg(limit.to_string())
            .arg("--")
            .arg(target);
        self.json(cmd).await
    }

    /// Full information for a single item, including the format list.
    pub async fn info(&self, url: &str) -> Result<YtInfo, ToolError> {
        let mut cmd = self.command();
        cmd.args(["-J", "--no-playlist", "--"]).arg(url);
        self.json(cmd).await
    }

    /// Lightweight listing of a playlist/album (entries are not extracted).
    pub async fn resolve_flat(&self, url: &str, max_entries: usize) -> Result<YtInfo, ToolError> {
        let mut cmd = self.command();
        cmd.args(["-J", "--flat-playlist", "--playlist-end"])
            .arg(max_entries.to_string())
            .arg("--")
            .arg(url);
        self.json(cmd).await
    }

    /// Download one format's audio into `work_dir` without re-encoding.
    ///
    /// Produces `media.<ext>` (audio remuxed into its natural container) and,
    /// when available, `cover.jpg`. Partial downloads in `work_dir` are
    /// resumed by yt-dlp on the next attempt.
    pub async fn download(
        &self,
        request: DownloadRequest<'_>,
        on_progress: &(dyn Fn(DownloadProgress) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<(), ToolError> {
        let mut cmd = self.command();
        cmd.arg("--no-playlist")
            // Fall back to yt-dlp's own best-audio choice only if the exact
            // format vanished between metadata fetch and download; the real
            // quality is measured from the file afterwards either way.
            .arg("-f")
            .arg(format!("{}/bestaudio", request.format_id))
            // -x without --audio-format keeps the original codec (stream copy).
            .arg("-x")
            .args(["--write-thumbnail", "--convert-thumbnails", "jpg"])
            .arg("-P")
            .arg(request.work_dir)
            .args(["-o", "media.%(ext)s", "-o", "thumbnail:cover.%(ext)s"])
            .args(["--retries", "5", "--fragment-retries", "5"])
            .args(["--socket-timeout", "30", "--no-mtime", "--newline"])
            .arg("--progress-template")
            .arg(format!(
                "download:{PROGRESS_PREFIX}%(progress.downloaded_bytes)s:\
                 %(progress.total_bytes)s:%(progress.total_bytes_estimate)s:%(progress.speed)s"
            ));
        if self.ffmpeg.contains(std::path::MAIN_SEPARATOR) {
            cmd.arg("--ffmpeg-location").arg(&self.ffmpeg);
        }
        cmd.arg("--").arg(request.url);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| spawn_error(TOOL, &self.program, e))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ToolError::Io {
                tool: TOOL,
                source: std::io::Error::other("child pipes unavailable"),
            });
        };
        let stderr_task = tokio::spawn(read_tail(stderr, 16 * 1024, TOOL));

        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    return Err(ToolError::Cancelled);
                }
                line = lines.next_line() => {
                    match line.map_err(|source| ToolError::Io { tool: TOOL, source })? {
                        Some(line) => {
                            if let Some(progress) = parse_progress(&line) {
                                on_progress(progress);
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(ToolError::Cancelled);
            }
            status = child.wait() => status.map_err(|source| ToolError::Io { tool: TOOL, source })?,
        };
        let stderr = stderr_task.await.unwrap_or_else(|_| Ok(String::new()))?;
        if status.success() {
            Ok(())
        } else {
            Err(failure(TOOL, status, &stderr))
        }
    }
}

fn parse_progress(line: &str) -> Option<DownloadProgress> {
    let rest = line.trim().strip_prefix(PROGRESS_PREFIX)?;
    let mut fields = rest.split(':').map(|f| f.trim().parse::<f64>().ok());
    let downloaded = fields.next()?;
    let total = fields.next()?;
    let estimate = fields.next()?;
    let speed = fields.next()?;
    let to_u64 = |v: Option<f64>| v.filter(|v| v.is_finite() && *v >= 0.0).map(|v| v as u64);
    Some(DownloadProgress {
        downloaded_bytes: to_u64(downloaded),
        total_bytes: to_u64(total).or(to_u64(estimate)),
        speed_bytes_per_sec: speed.filter(|s| s.is_finite()),
    })
}

/// Deserialize a field leniently: wrong types become `None` instead of
/// failing the whole document. yt-dlp output varies between extractors and
/// versions (e.g. `has_drm` may be `"maybe"`).
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Subset of yt-dlp's info dict that Inkult uses.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct YtInfo {
    #[serde(deserialize_with = "lenient")]
    pub id: Option<String>,
    #[serde(rename = "_type", deserialize_with = "lenient")]
    pub kind: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub title: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub track: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub artist: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub artists: Option<Vec<String>>,
    #[serde(deserialize_with = "lenient")]
    pub creator: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub creators: Option<Vec<String>>,
    #[serde(deserialize_with = "lenient")]
    pub album: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub album_artist: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub album_artists: Option<Vec<String>>,
    #[serde(deserialize_with = "lenient")]
    pub track_number: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub disc_number: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub release_date: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub release_year: Option<i64>,
    #[serde(deserialize_with = "lenient")]
    pub upload_date: Option<String>,
    /// Upload time (Unix seconds); flat SoundCloud listings carry this but
    /// no `upload_date`.
    #[serde(deserialize_with = "lenient")]
    pub timestamp: Option<i64>,
    #[serde(deserialize_with = "lenient")]
    pub tags: Option<Vec<String>>,
    #[serde(deserialize_with = "lenient")]
    pub genre: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub genres: Option<Vec<String>>,
    #[serde(deserialize_with = "lenient")]
    pub description: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub uploader: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub uploader_url: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub channel: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub channel_id: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub webpage_url: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub url: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub duration: Option<f64>,
    #[serde(deserialize_with = "lenient")]
    pub thumbnail: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub thumbnails: Option<Vec<YtThumbnail>>,
    #[serde(deserialize_with = "lenient")]
    pub formats: Option<Vec<YtFormat>>,
    #[serde(deserialize_with = "lenient")]
    /// Unavailable playlist items may appear as `null`.
    pub entries: Option<Vec<Option<YtInfo>>>,
    /// Single-format items (no `formats` list) carry format fields inline.
    #[serde(flatten)]
    pub inline_format: YtFormat,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct YtThumbnail {
    #[serde(deserialize_with = "lenient")]
    pub url: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub width: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub height: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct YtFormat {
    #[serde(deserialize_with = "lenient")]
    pub format_id: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub acodec: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub vcodec: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub abr: Option<f64>,
    #[serde(deserialize_with = "lenient")]
    pub tbr: Option<f64>,
    #[serde(deserialize_with = "lenient")]
    pub asr: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub audio_channels: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub ext: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub format_note: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub has_drm: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    pub protocol: Option<String>,
    /// Direct media URL of this rendition. For single-format items this is
    /// reported as the item's top-level `url` instead (see [`YtInfo::url`]).
    #[serde(deserialize_with = "lenient")]
    pub url: Option<String>,
    /// Headers yt-dlp would send when fetching `url`; some CDNs require them.
    #[serde(deserialize_with = "lenient")]
    pub http_headers: Option<BTreeMap<String, String>>,
}

impl YtFormat {
    /// Convert to a domain format, or `None` if this rendition carries no
    /// usable audio (video-only, storyboard, DRM-protected).
    pub fn to_audio_format(&self) -> Option<AudioFormat> {
        let id = self.format_id.clone()?;
        if self.acodec.as_deref() == Some("none")
            || self.has_drm == Some(true)
            || self.protocol.as_deref() == Some("mhtml")
        {
            return None;
        }
        let has_video = self.vcodec.as_deref().is_some_and(|v| v != "none");
        if self.acodec.is_none() && has_video {
            // Codec unknown on a video stream: can't tell if audio exists.
            return None;
        }
        let codec = Codec::detect(self.acodec.as_deref(), self.ext.as_deref());
        let note = self.format_note.clone();
        let dynamic_range_compressed = id.ends_with("-drc")
            || note
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().contains("drc"));
        Some(AudioFormat {
            codec,
            bitrate_kbps: self.abr.or(if has_video { None } else { self.tbr }),
            sample_rate_hz: self.asr,
            channels: self.audio_channels,
            container: self.ext.clone(),
            has_video,
            dynamic_range_compressed,
            note,
            id,
        })
    }
}

impl YtInfo {
    pub fn is_playlist(&self) -> bool {
        matches!(self.kind.as_deref(), Some("playlist" | "multi_video"))
    }

    pub fn into_entries(self) -> impl Iterator<Item = YtInfo> {
        self.entries.into_iter().flatten().flatten()
    }

    pub fn entries(&self) -> impl Iterator<Item = &YtInfo> {
        self.entries.iter().flatten().flatten()
    }

    pub fn audio_formats(&self) -> Vec<AudioFormat> {
        match &self.formats {
            Some(formats) if !formats.is_empty() => formats
                .iter()
                .filter_map(YtFormat::to_audio_format)
                .collect(),
            _ => self.inline_format.to_audio_format().into_iter().collect(),
        }
    }

    /// Largest thumbnail by pixel area, falling back to `thumbnail`.
    /// Only `https` URLs are returned since they are shown in the browser.
    pub fn best_thumbnail(&self) -> Option<String> {
        let best = self
            .thumbnails
            .iter()
            .flatten()
            .filter(|t| t.url.as_deref().is_some_and(is_https))
            .max_by_key(|t| u64::from(t.width.unwrap_or(0)) * u64::from(t.height.unwrap_or(0)))
            .and_then(|t| t.url.clone());
        best.or_else(|| self.thumbnail.clone().filter(|u| is_https(u)))
    }
}

fn is_https(url: &str) -> bool {
    url.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_lines() {
        let p = parse_progress("inkult-progress:1024:4096:NA:512.5").unwrap();
        assert_eq!(p.downloaded_bytes, Some(1024));
        assert_eq!(p.total_bytes, Some(4096));
        assert_eq!(p.speed_bytes_per_sec, Some(512.5));
        assert_eq!(p.fraction(), Some(0.25));

        let est = parse_progress("inkult-progress:10:NA:100.0:NA").unwrap();
        assert_eq!(est.total_bytes, Some(100));
        assert_eq!(est.speed_bytes_per_sec, None);

        assert!(parse_progress("[download] 10% of 3MiB").is_none());
    }

    #[test]
    fn lenient_parsing_tolerates_odd_types() {
        let json = r#"{
            "id": "abc", "title": "T", "track_number": "seven",
            "formats": [
                {"format_id": "251", "acodec": "opus", "vcodec": "none", "abr": 135.2, "asr": 48000, "ext": "webm", "has_drm": "maybe"},
                {"format_id": "251-drc", "acodec": "opus", "vcodec": "none", "abr": 140.0, "ext": "webm", "format_note": "medium, DRC"},
                {"format_id": "137", "acodec": "none", "vcodec": "avc1", "tbr": 4000},
                {"format_id": "sb0", "protocol": "mhtml"}
            ]
        }"#;
        let info: YtInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.track_number, None);
        let formats = info.audio_formats();
        assert_eq!(
            formats.len(),
            2,
            "video-only and storyboard excluded: {formats:?}"
        );
        assert_eq!(formats[0].codec, Codec::Opus);
        assert!(!formats[0].dynamic_range_compressed);
        assert!(formats[1].dynamic_range_compressed);
    }

    #[test]
    fn single_format_items_use_inline_fields() {
        let json = r#"{"id": "1", "format_id": "download", "ext": "wav", "vcodec": "none"}"#;
        let info: YtInfo = serde_json::from_str(json).unwrap();
        let formats = info.audio_formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].codec, Codec::Pcm);
    }

    #[test]
    fn picks_largest_https_thumbnail() {
        let json = r#"{"thumbnails": [
            {"url": "https://i/small.jpg", "width": 120, "height": 90},
            {"url": "http://i/insecure.jpg", "width": 4000, "height": 4000},
            {"url": "https://i/max.jpg", "width": 1280, "height": 720}
        ]}"#;
        let info: YtInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.best_thumbnail().as_deref(), Some("https://i/max.jpg"));
    }
}
