//! Behaviour shared by the yt-dlp backed providers.
//!
//! Metadata precedence implemented here (highest first):
//!
//! 1. Structured fields reported by the provider (`track`, `artists`, ...).
//! 2. Values parsed from the provider's display title ("Artist - Title").
//! 3. The uploader/channel name as artist.
//!
//! Values from 2 and 3 are recorded in `TrackMetadata::inferred`.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use url::Url;

use super::{
    DownloadedMedia, ProviderError, StreamSource, StreamTransport, SummaryYear, TrackSummary,
    YearBasis,
};
use crate::domain::DownloadProgress;
use crate::domain::metadata::{MetadataField, ReleaseDate, TrackMetadata};
use crate::domain::provider::ProviderKind;
use crate::domain::quality::{AudioFormat, Codec, select_best};
use crate::metadata::{
    clean_artists, clean_multiline, clean_text, split_artist_title, strip_title_noise,
};
use crate::tools::ToolError;
use crate::tools::ytdlp::{DownloadRequest, YtDlp, YtFormat, YtInfo};

/// Containers yt-dlp may leave after extracting audio without re-encoding.
const AUDIO_EXTENSIONS: &[&str] = &[
    "opus", "ogg", "oga", "m4a", "mp3", "flac", "wav", "aif", "aiff", "aac",
];

/// Provider-local identifiers end up in paths and the library index, so only
/// a conservative character set is accepted.
pub fn is_valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with('.')
}

/// Read a response body, giving up once it exceeds `max_bytes`.
pub async fn read_body_capped(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > max_bytes {
            return Err("response too large".into());
        }
    }
    Ok(bytes)
}

/// Map a yt-dlp failure onto a provider error, deciding retryability from
/// the error text yt-dlp printed.
pub fn classify_error(provider: ProviderKind, err: ToolError) -> ProviderError {
    let message = match err {
        ToolError::Cancelled => return ProviderError::Cancelled,
        ToolError::Timeout { .. } => {
            return ProviderError::Network {
                provider,
                reason: err.to_string(),
            };
        }
        ToolError::Failed { ref message, .. } => message.clone(),
        other => return ProviderError::Tool(other),
    };
    let lower = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    let reason = message.trim_start_matches("ERROR:").trim().to_owned();

    if has(&[
        "http error 429",
        "too many requests",
        "rate limit",
        "rate-limit",
    ]) {
        ProviderError::RateLimited { provider }
    } else if has(&[
        "not a bot",
        "sign in to confirm",
        "login required",
        "use --cookies",
    ]) {
        ProviderError::Blocked { provider, reason }
    } else if has(&[
        "video unavailable",
        "private video",
        "not available",
        "has been removed",
        "been terminated",
        "copyright",
        "members-only",
        "premieres in",
        "http error 404",
        "does not exist",
        "unsupported url",
        "geo restrict",
        "no video formats found",
        "requested format is not available",
    ]) {
        ProviderError::Unavailable { provider, reason }
    } else if has(&[
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "connection aborted",
        "temporary failure",
        "name resolution",
        "network is unreachable",
        "http error 5",
        "http error 403",
        "unable to download",
        "incomplete read",
        "remote end closed",
        "ssl",
    ]) {
        ProviderError::Network { provider, reason }
    } else {
        ProviderError::Failed { provider, reason }
    }
}

fn structured_artists(info: &YtInfo) -> Vec<String> {
    let split = |s: &String| s.split(", ").map(str::to_owned).collect::<Vec<_>>();
    let candidates = [
        info.artists.clone(),
        info.artist.as_ref().map(split),
        info.creators.clone(),
        info.creator.as_ref().map(split),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(clean_artists)
        .find(|artists| !artists.is_empty())
        .unwrap_or_default()
}

/// Determine title and artists following the module-level precedence.
pub fn title_and_artists(
    info: &YtInfo,
    uploader_artist: Option<&str>,
) -> (String, Vec<String>, Vec<MetadataField>) {
    let mut inferred = Vec::new();
    let raw_title = info.title.as_deref().and_then(clean_text);
    let parsed = raw_title.as_deref().and_then(split_artist_title);
    let mut artists = structured_artists(info);

    let title = match info.track.as_deref().and_then(clean_text) {
        Some(track) => track,
        None => {
            let guess = match &parsed {
                Some((_, title)) => title.clone(),
                None => raw_title
                    .as_deref()
                    .map(strip_title_noise)
                    .unwrap_or_else(|| "Untitled".to_owned()),
            };
            if Some(&guess) != raw_title.as_ref() {
                inferred.push(MetadataField::Title);
            }
            guess
        }
    };

    if artists.is_empty() {
        let fallback = parsed
            .map(|(artist, _)| artist)
            .or_else(|| uploader_artist.and_then(clean_text));
        if let Some(artist) = fallback {
            artists = vec![artist];
            inferred.push(MetadataField::Artists);
        }
    }
    (title, artists, inferred)
}

pub fn summary_from_entry(
    provider: ProviderKind,
    entry: &YtInfo,
    url: String,
    uploader_artist: Option<&str>,
) -> TrackSummary {
    let (title, artists, _) = title_and_artists(entry, uploader_artist);
    TrackSummary {
        provider,
        url,
        title,
        artists,
        album: entry.album.as_deref().and_then(clean_text),
        duration_secs: entry.duration.filter(|d| d.is_finite() && *d >= 0.0),
        thumbnail_url: entry.best_thumbnail(),
        track_number: None,
        year: release_date(entry).map(|date| SummaryYear {
            year: date.year,
            basis: YearBasis::Release,
        }),
        label: None,
        label_hints: Vec::new(),
        artist_links: Vec::new(),
        reposted: false,
    }
}

fn release_date(info: &YtInfo) -> Option<ReleaseDate> {
    info.release_date
        .as_deref()
        .and_then(ReleaseDate::parse)
        .or_else(|| info.release_year.and_then(ReleaseDate::from_year))
}

/// Build normalized metadata from a full yt-dlp info dict.
pub fn base_metadata(
    provider: ProviderKind,
    info: &YtInfo,
    source_url: String,
    uploader_artist: Option<&str>,
) -> Result<TrackMetadata, ProviderError> {
    let id = info
        .id
        .clone()
        .filter(|id| is_valid_id(id))
        .ok_or_else(|| ProviderError::InvalidResponse {
            provider,
            reason: "track has no usable identifier".into(),
        })?;
    let (title, artists, mut inferred) = title_and_artists(info, uploader_artist);

    let release_date = match release_date(info) {
        Some(date) => Some(date),
        None => {
            let upload = info.upload_date.as_deref().and_then(ReleaseDate::parse);
            if upload.is_some() {
                inferred.push(MetadataField::ReleaseDate);
            }
            upload
        }
    };

    let genres = match (&info.genres, &info.genre) {
        (Some(list), _) if !list.is_empty() => clean_artists(list),
        (_, Some(genre)) => clean_artists([genre]),
        _ => Vec::new(),
    };

    let album_artist = info
        .album_artists
        .as_ref()
        .and_then(|list| list.first())
        .or(info.album_artist.as_ref())
        .and_then(|s| clean_text(s));

    Ok(TrackMetadata {
        provider,
        provider_track_id: id,
        source_url,
        title,
        artists,
        album: info.album.as_deref().and_then(clean_text),
        album_artist,
        track_number: info.track_number.filter(|n| *n > 0),
        disc_number: info.disc_number.filter(|n| *n > 0),
        release_date,
        genres,
        description: info.description.as_deref().and_then(clean_multiline),
        copyright: None,
        label: None,
        isrc: None,
        artwork_url: info.best_thumbnail(),
        duration_secs: info.duration.filter(|d| d.is_finite() && *d >= 0.0),
        inferred,
    })
}

/// Download via yt-dlp and locate the produced files in `work_dir`.
pub async fn download_with_ytdlp(
    ytdlp: &YtDlp,
    provider: ProviderKind,
    url: &str,
    format: &AudioFormat,
    work_dir: &Path,
    on_progress: &(dyn Fn(DownloadProgress) + Send + Sync),
    cancel: &CancellationToken,
) -> Result<DownloadedMedia, ProviderError> {
    let request = DownloadRequest {
        url,
        format_id: &format.id,
        work_dir,
    };
    ytdlp
        .download(request, on_progress, cancel)
        .await
        .map_err(|e| classify_error(provider, e))?;

    let (audio, artwork) = find_outputs(work_dir)
        .await
        .map_err(|e| ProviderError::Failed {
            provider,
            reason: format!("could not inspect download directory: {e}"),
        })?;
    let audio_path = audio.ok_or_else(|| ProviderError::InvalidResponse {
        provider,
        reason: "download finished but produced no audio file".into(),
    })?;
    Ok(DownloadedMedia {
        audio_path,
        artwork_path: artwork,
    })
}

/// Whether mainstream browsers can play `codec` delivered this way.
///
/// Progressive files are proxied byte-for-byte, so the container matters.
/// HLS segments are re-wrapped (never re-encoded) by ffmpeg into a plain
/// stream, which exists for these codecs only.
fn browser_playable(codec: Codec, transport: StreamTransport, container: Option<&str>) -> bool {
    match transport {
        StreamTransport::Progressive => matches!(
            (codec, container),
            (Codec::Opus | Codec::Vorbis, Some("webm" | "ogg" | "opus"))
                | (Codec::Aac, Some("m4a" | "mp4"))
                | (Codec::Mp3, Some("mp3"))
                | (Codec::Flac, Some("flac"))
        ),
        StreamTransport::Hls => matches!(codec, Codec::Aac | Codec::Mp3 | Codec::Opus),
    }
}

struct StreamCandidate<'a> {
    format: AudioFormat,
    url: Url,
    transport: StreamTransport,
    raw: &'a YtFormat,
}

fn stream_candidate<'a>(
    raw: &'a YtFormat,
    fallback_url: Option<&str>,
) -> Option<StreamCandidate<'a>> {
    let format = raw.to_audio_format()?;
    if format.has_video {
        // Same audio as an audio-only rendition, plus video bandwidth.
        return None;
    }
    let transport = match raw.protocol.as_deref() {
        None | Some("https" | "http") => StreamTransport::Progressive,
        Some("m3u8" | "m3u8_native") => StreamTransport::Hls,
        // DASH manifests, RTMP and the like are not supported for listening.
        Some(_) => return None,
    };
    let url = Url::parse(raw.url.as_deref().or(fallback_url)?).ok()?;
    if url.scheme() != "https"
        || !browser_playable(format.codec, transport, format.container.as_deref())
    {
        return None;
    }
    Some(StreamCandidate {
        format,
        url,
        transport,
        raw,
    })
}

/// Choose the rendition used to listen to a track before downloading it.
///
/// Progressive files win over HLS because the browser can seek in them
/// natively and they need no processing. Within a transport the ordering is
/// the same as for downloads (no DRC, best codec-weighted bitrate).
pub fn pick_stream(provider: ProviderKind, info: &YtInfo) -> Result<StreamSource, ProviderError> {
    let candidates: Vec<StreamCandidate<'_>> = match &info.formats {
        Some(formats) if !formats.is_empty() => formats
            .iter()
            .filter_map(|f| stream_candidate(f, None))
            .collect(),
        _ => stream_candidate(&info.inline_format, info.url.as_deref())
            .into_iter()
            .collect(),
    };
    let best_of = |transport: StreamTransport| {
        let pool: Vec<&StreamCandidate<'_>> = candidates
            .iter()
            .filter(|c| c.transport == transport)
            .collect();
        let formats: Vec<AudioFormat> = pool.iter().map(|c| c.format.clone()).collect();
        let best = select_best(&formats)?;
        pool.into_iter().find(|c| c.format.id == best.id)
    };
    let chosen = best_of(StreamTransport::Progressive)
        .or_else(|| best_of(StreamTransport::Hls))
        .ok_or_else(|| ProviderError::Unavailable {
            provider,
            reason: "no stream format a browser can play is offered for this track".into(),
        })?;

    let headers = chosen
        .raw
        .http_headers
        .iter()
        .flatten()
        .filter(|(name, value)| {
            !name.is_empty() && !format!("{name}{value}").contains(['\r', '\n'])
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    Ok(StreamSource {
        media_url: chosen.url.clone(),
        headers,
        transport: chosen.transport,
        format: chosen.format.clone(),
        duration_secs: info.duration.filter(|d| d.is_finite() && *d > 0.0),
    })
}

async fn find_outputs(dir: &Path) -> std::io::Result<(Option<PathBuf>, Option<PathBuf>)> {
    let mut audio: Option<(u64, PathBuf)> = None;
    let mut artwork = None;
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if !meta.is_file() {
            continue;
        }
        let (Some(stem), Some(ext)) = (
            path.file_stem().and_then(|s| s.to_str()),
            path.extension().and_then(|s| s.to_str()),
        ) else {
            continue;
        };
        let ext = ext.to_ascii_lowercase();
        if stem == "media" && AUDIO_EXTENSIONS.contains(&ext.as_str()) {
            if audio.as_ref().is_none_or(|(size, _)| meta.len() > *size) {
                audio = Some((meta.len(), path));
            }
        } else if stem == "cover" && matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
            artwork = Some(path);
        }
    }
    Ok((audio.map(|(_, p)| p), artwork))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(json: &str) -> YtInfo {
        serde_json::from_str(json).unwrap()
    }

    fn failed(message: &str) -> ToolError {
        ToolError::Failed {
            tool: "yt-dlp",
            status: "exit status: 1".into(),
            message: message.into(),
        }
    }

    #[test]
    fn validates_ids() {
        assert!(is_valid_id("dQw4w9WgXcQ"));
        assert!(is_valid_id("123456789"));
        assert!(!is_valid_id("../etc"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("a/b"));
    }

    #[test]
    fn classifies_errors_by_retryability() {
        let p = ProviderKind::YouTube;
        assert!(
            classify_error(p, failed("ERROR: HTTP Error 429: Too Many Requests")).is_retryable()
        );
        assert!(classify_error(p, failed("ERROR: Read timed out")).is_retryable());
        let unavailable = classify_error(p, failed("ERROR: [youtube] x: Video unavailable"));
        assert!(matches!(unavailable, ProviderError::Unavailable { .. }));
        assert!(!unavailable.is_retryable());
        assert!(matches!(
            classify_error(p, failed("ERROR: Sign in to confirm you're not a bot")),
            ProviderError::Blocked { .. }
        ));
        assert!(matches!(
            classify_error(p, ToolError::Cancelled),
            ProviderError::Cancelled
        ));
    }

    #[test]
    fn structured_fields_win_over_title_parsing() {
        let i = info(
            r#"{"id":"x","title":"Somebody - Else (Official Video)","track":"Real Title","artists":["Real Artist"]}"#,
        );
        let (title, artists, inferred) = title_and_artists(&i, Some("Uploader"));
        assert_eq!(title, "Real Title");
        assert_eq!(artists, vec!["Real Artist"]);
        assert!(inferred.is_empty());
    }

    #[test]
    fn falls_back_to_title_parsing_then_uploader() {
        let i = info(r#"{"id":"x","title":"Bicep - Glue (Official Video)"}"#);
        let (title, artists, inferred) = title_and_artists(&i, Some("Some Label"));
        assert_eq!(title, "Glue");
        assert_eq!(artists, vec!["Bicep"]);
        assert_eq!(inferred, vec![MetadataField::Title, MetadataField::Artists]);

        let i = info(r#"{"id":"x","title":"Glue"}"#);
        let (title, artists, inferred) = title_and_artists(&i, Some("Bicep"));
        assert_eq!(title, "Glue");
        assert_eq!(artists, vec!["Bicep"]);
        assert_eq!(inferred, vec![MetadataField::Artists]);
    }

    #[test]
    fn base_metadata_prefers_release_date_over_upload_date() {
        let i = info(
            r#"{"id":"x","title":"T","artists":["A"],"release_date":"20200102","upload_date":"20210101"}"#,
        );
        let m = base_metadata(ProviderKind::YouTube, &i, "u".into(), None).unwrap();
        assert_eq!(m.release_date.unwrap().to_string(), "2020-01-02");
        assert!(m.inferred.is_empty());

        let i = info(r#"{"id":"x","title":"T","artists":["A"],"upload_date":"20210101"}"#);
        let m = base_metadata(ProviderKind::YouTube, &i, "u".into(), None).unwrap();
        assert_eq!(m.inferred, vec![MetadataField::ReleaseDate]);
    }

    #[test]
    fn base_metadata_rejects_unsafe_ids() {
        let i = info(r#"{"id":"../../x","title":"T"}"#);
        assert!(base_metadata(ProviderKind::YouTube, &i, "u".into(), None).is_err());
    }

    #[test]
    fn stream_prefers_progressive_unprocessed_audio() {
        let i = info(
            r#"{"id":"x","duration":213.5,"formats":[
                {"format_id":"233","protocol":"m3u8_native","acodec":"mp4a.40.2","vcodec":"none","abr":256,"ext":"mp4","url":"https://cdn/hls.m3u8"},
                {"format_id":"251-drc","protocol":"https","acodec":"opus","vcodec":"none","abr":140,"ext":"webm","url":"https://cdn/drc"},
                {"format_id":"251","protocol":"https","acodec":"opus","vcodec":"none","abr":130,"ext":"webm","url":"https://cdn/251","http_headers":{"User-Agent":"UA","Bad":"a\r\nInjected: 1"}},
                {"format_id":"140","protocol":"https","acodec":"mp4a.40.2","vcodec":"none","abr":129,"ext":"m4a","url":"https://cdn/140"},
                {"format_id":"18","protocol":"https","acodec":"mp4a.40.2","vcodec":"avc1","abr":500,"ext":"mp4","url":"https://cdn/18"},
                {"format_id":"plain","protocol":"http","acodec":"mp3","vcodec":"none","abr":999,"ext":"mp3","url":"http://cdn/insecure"}
            ]}"#,
        );
        let s = pick_stream(ProviderKind::YouTube, &i).unwrap();
        assert_eq!(s.format.id, "251");
        assert_eq!(s.transport, StreamTransport::Progressive);
        assert_eq!(s.media_url.as_str(), "https://cdn/251");
        assert_eq!(s.headers, vec![("User-Agent".to_owned(), "UA".to_owned())]);
        assert_eq!(s.duration_secs, Some(213.5));
    }

    #[test]
    fn stream_falls_back_to_hls() {
        let i = info(
            r#"{"id":"x","formats":[
                {"format_id":"hls_aac_96k","protocol":"m3u8_native","acodec":"mp4a.40.2","vcodec":"none","abr":96,"ext":"m4a","url":"https://cdn/96.m3u8"},
                {"format_id":"hls_aac_160k","protocol":"m3u8_native","acodec":"mp4a.40.2","vcodec":"none","abr":160,"ext":"m4a","url":"https://cdn/160.m3u8"},
                {"format_id":"dash","protocol":"http_dash_segments","acodec":"opus","vcodec":"none","abr":300,"ext":"webm","url":"https://cdn/dash"}
            ]}"#,
        );
        let s = pick_stream(ProviderKind::SoundCloud, &i).unwrap();
        assert_eq!(s.format.id, "hls_aac_160k");
        assert_eq!(s.transport, StreamTransport::Hls);

        let none = info(
            r#"{"id":"x","formats":[{"format_id":"a","protocol":"https","acodec":"alac","vcodec":"none","ext":"m4a","url":"https://cdn/a"}]}"#,
        );
        assert!(matches!(
            pick_stream(ProviderKind::SoundCloud, &none),
            Err(ProviderError::Unavailable { .. })
        ));
    }

    #[test]
    fn stream_uses_inline_format_url() {
        let i = info(
            r#"{"id":"1","format_id":"download","protocol":"https","acodec":"mp3","vcodec":"none","ext":"mp3","url":"https://cdn/file.mp3"}"#,
        );
        let s = pick_stream(ProviderKind::SoundCloud, &i).unwrap();
        assert_eq!(s.media_url.as_str(), "https://cdn/file.mp3");
    }

    #[tokio::test]
    async fn finds_largest_media_file_and_cover() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("media.opus"), vec![0u8; 100])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("media.webm.part"), vec![0u8; 500])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("cover.jpg"), b"x")
            .await
            .unwrap();
        let (audio, cover) = find_outputs(dir.path()).await.unwrap();
        assert_eq!(audio.unwrap().file_name().unwrap(), "media.opus");
        assert_eq!(cover.unwrap().file_name().unwrap(), "cover.jpg");
    }
}
