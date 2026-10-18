//! Music sources.
//!
//! Each provider owns everything source-specific: URL recognition, search,
//! metadata normalization quirks, and how media is fetched. The rest of the
//! application only sees the [`MusicProvider`] trait and domain types.

mod common;
mod soundcloud;
mod youtube;
mod youtube_music;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::domain::DownloadProgress;
use crate::domain::length::HiddenTooLong;
use crate::domain::metadata::TrackMetadata;
use crate::domain::provider::ProviderKind;
use crate::domain::quality::AudioFormat;
use crate::tools::ToolError;

pub use soundcloud::SoundCloudProvider;
pub use youtube::YouTubeProvider;

/// Longest URL accepted from users.
const MAX_URL_LEN: usize = 2048;

/// What a recognized URL points at, judged from its shape alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlKind {
    Track,
    /// Playlist, album, set, or a short link that must be resolved first
    /// (resolution may still yield a single track).
    Collection,
}

/// Lightweight track listing used for search results and collections.
#[derive(Debug, Clone, Serialize)]
pub struct TrackSummary {
    pub provider: ProviderKind,
    pub url: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_secs: Option<f64>,
    pub thumbnail_url: Option<String>,
    /// Position on an album release, when listed as part of one. Used as the
    /// track number if the track's own metadata has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<SummaryYear>,
    /// Record label, when the provider states it as such.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Free text that may name the label (copyright line, tags, uploader,
    /// description). Only searched by the label filter: it is not reliable
    /// enough to show or to tag files with.
    #[serde(skip)]
    pub label_hints: Vec<String>,
    /// Provider pages of the people behind the track: credited artists, and
    /// on upload sites the account that posted it (which may be a label or
    /// a re-uploader rather than one of `artists`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artist_links: Vec<ArtistLink>,
    /// Listed on an artist page because that artist reposted it: someone
    /// else posted it (see `artist_links`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reposted: bool,
}

/// A name linked to that artist's (or uploader's) page on the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtistLink {
    pub name: String,
    /// Canonical page URL, accepted by [`MusicProvider::artist`].
    pub url: String,
}

/// An artist's or uploader's page: who they are and what they posted.
#[derive(Debug, Clone, Serialize)]
pub struct ArtistPage {
    pub provider: ProviderKind,
    /// The page on the provider's site.
    pub url: String,
    pub name: String,
    pub biography: Option<String>,
    /// Square picture of the artist, on the provider's image CDN.
    pub image_url: Option<String>,
    pub tracks: Vec<TrackSummary>,
    /// The track limit was reached; the provider may list more.
    pub truncated: bool,
    /// Whether tracks the artist reposted are listed among their own;
    /// `None` on providers without reposts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reposts: Option<bool>,
    /// Tracks left out for being over the length limit (set by the app).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub too_long: Option<HiddenTooLong>,
}

/// The year a listed track came out, and how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SummaryYear {
    pub year: u16,
    pub basis: YearBasis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum YearBasis {
    /// The provider's release date.
    Release,
    /// Only the upload date is known; re-uploads make it later than the
    /// actual release.
    Upload,
}

/// How much a provider should find out about each search result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDetail {
    /// Whatever the search listing itself returns.
    Listing,
    /// Also release year and label, even if that costs a request per result.
    Release,
}

#[derive(Debug, Clone, Serialize)]
pub struct Collection {
    pub provider: ProviderKind,
    pub url: String,
    pub title: Option<String>,
    pub tracks: Vec<TrackSummary>,
    /// Tracks left out for being over the length limit (set by the app).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub too_long: Option<HiddenTooLong>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolved {
    Track(TrackSummary),
    Collection(Collection),
}

/// Everything needed to choose a format and download a single track.
#[derive(Debug, Clone)]
pub struct TrackDetails {
    pub metadata: TrackMetadata,
    pub formats: Vec<AudioFormat>,
    /// The provider's artwork is a letterboxed frame around a square cover.
    pub artwork_needs_square_crop: bool,
}

/// Files produced by a provider download, inside the job's work directory.
#[derive(Debug, Clone)]
pub struct DownloadedMedia {
    pub audio_path: PathBuf,
    pub artwork_path: Option<PathBuf>,
}

/// How a listening stream is delivered by the provider's CDN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTransport {
    /// One file over HTTPS that supports byte ranges (seekable as-is).
    Progressive,
    /// An HLS playlist of segments; must be joined into one stream.
    Hls,
}

/// The media stack of the browser that will play a listening stream.
///
/// Safari, and every browser on iOS and iPadOS (they all must use
/// WebKit), plays `<audio>` through Apple's AVFoundation. It plays WebM
/// only after downloading the whole file, so a preview takes seconds to
/// start, and it expects byte ranges and a known length for everything it
/// plays. On the other hand it decodes AIFF and ALAC itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaEngine {
    /// Chromium and Firefox engines: WebM, Ogg, MP4, MP3 and FLAC alike.
    #[default]
    Standard,
    /// Apple's AVFoundation; see the type's documentation.
    Apple,
}

/// A rendition the browser can play to listen to a track before it is
/// downloaded. Only used for listening: downloads pick their format through
/// `domain::quality` and never go through this.
#[derive(Clone)]
pub struct StreamSource {
    /// Signed, short-lived CDN URL. Treat as a secret: never log it.
    pub media_url: Url,
    /// Headers the CDN expects (user agent etc.), as reported by yt-dlp.
    pub headers: Vec<(String, String)>,
    pub transport: StreamTransport,
    pub format: AudioFormat,
    pub duration_secs: Option<f64>,
}

impl std::fmt::Debug for StreamSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSource")
            .field("host", &self.media_url.host_str())
            .field("transport", &self.transport)
            .field("format", &self.format.id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("no provider supports this URL (supported: YouTube, YouTube Music, SoundCloud)")]
    UnsupportedUrl,
    #[error("{provider}: content unavailable: {reason}")]
    Unavailable {
        provider: ProviderKind,
        reason: String,
    },
    #[error(
        "{provider}: request blocked by the provider (bot detection or sign-in required): {reason}"
    )]
    Blocked {
        provider: ProviderKind,
        reason: String,
    },
    #[error("{provider}: rate limited, try again later")]
    RateLimited { provider: ProviderKind },
    #[error("{provider}: network error: {reason}")]
    Network {
        provider: ProviderKind,
        reason: String,
    },
    #[error("{provider}: no downloadable audio format is offered for this track")]
    NoAudio { provider: ProviderKind },
    #[error("{provider}: unexpected response: {reason}")]
    InvalidResponse {
        provider: ProviderKind,
        reason: String,
    },
    #[error("{provider}: {reason}")]
    Failed {
        provider: ProviderKind,
        reason: String,
    },
    #[error(transparent)]
    Tool(ToolError),
    #[error("operation cancelled")]
    Cancelled,
}

impl ProviderError {
    /// Transient failures worth retrying with backoff.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited { .. } | ProviderError::Network { .. }
        )
    }
}

#[async_trait]
pub trait MusicProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Recognize a URL belonging to this provider. Must not do any I/O.
    fn classify(&self, url: &Url) -> Option<UrlKind>;

    async fn search(
        &self,
        query: &str,
        limit: usize,
        detail: SearchDetail,
    ) -> Result<Vec<TrackSummary>, ProviderError>;

    /// Recognize an artist or uploader page URL. Must not do any I/O.
    fn is_artist_url(&self, url: &Url) -> bool;

    /// Biography and the tracks posted on an artist or uploader page, at
    /// most `max_tracks` of them. With `include_reposts`, providers that
    /// have reposts list those too, interleaved by date as on the page.
    async fn artist(
        &self,
        url: &Url,
        max_tracks: usize,
        include_reposts: bool,
    ) -> Result<ArtistPage, ProviderError>;

    /// Expand a URL into a single track or a list of tracks.
    async fn resolve(&self, url: &Url, max_tracks: usize) -> Result<Resolved, ProviderError>;

    /// Full metadata and available formats for a single track URL.
    async fn track_details(&self, url: &Url) -> Result<TrackDetails, ProviderError>;

    /// A stream of a single track that `engine` plays well, for listening
    /// to it without downloading it first.
    async fn stream_source(
        &self,
        url: &Url,
        engine: MediaEngine,
    ) -> Result<StreamSource, ProviderError>;

    /// Download `format` of the track into `work_dir`, without transcoding.
    async fn download(
        &self,
        details: &TrackDetails,
        format: &AudioFormat,
        work_dir: &Path,
        on_progress: &(dyn Fn(DownloadProgress) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<DownloadedMedia, ProviderError>;
}

pub struct ProviderRegistry {
    providers: Vec<Arc<dyn MusicProvider>>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Arc<dyn MusicProvider>>) -> Self {
        Self { providers }
    }

    pub fn classify(&self, url: &Url) -> Option<(Arc<dyn MusicProvider>, UrlKind)> {
        self.providers
            .iter()
            .find_map(|p| p.classify(url).map(|kind| (Arc::clone(p), kind)))
    }

    pub fn artist_provider(&self, url: &Url) -> Option<Arc<dyn MusicProvider>> {
        self.providers
            .iter()
            .find(|p| p.is_artist_url(url))
            .cloned()
    }

    pub fn get(&self, kind: ProviderKind) -> Option<Arc<dyn MusicProvider>> {
        self.providers.iter().find(|p| p.kind() == kind).cloned()
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn MusicProvider>> {
        self.providers.iter()
    }
}

/// Parse and validate a user-supplied URL before any provider sees it.
///
/// Only http(s) URLs without embedded credentials are accepted; providers
/// then only accept their own hosts, so arbitrary URLs never reach yt-dlp.
pub fn parse_user_url(raw: &str) -> Result<Url, ProviderError> {
    let raw = raw.trim();
    if raw.len() > MAX_URL_LEN {
        return Err(ProviderError::InvalidUrl("URL is too long".into()));
    }
    let url = Url::parse(raw).map_err(|e| ProviderError::InvalidUrl(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ProviderError::InvalidUrl(
            "only http and https URLs are supported".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProviderError::InvalidUrl(
            "URLs with embedded credentials are not accepted".into(),
        ));
    }
    Ok(url)
}

/// Image CDNs of the supported providers.
const IMAGE_HOST_SUFFIXES: &[&str] = &[
    "ytimg.com",
    "ggpht.com",
    "googleusercontent.com",
    "sndcdn.com",
];

/// Whether a client-supplied image URL points at a provider's image CDN.
///
/// Thumbnails sent by the browser with a download request are shown to
/// other users (captains see every job). Accepting any URL would let one
/// user make another's browser fetch an address of their choosing, e.g. to
/// log their IP address.
pub fn is_provider_image_url(raw: &str) -> bool {
    if raw.len() > MAX_URL_LEN {
        return false;
    }
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let Some(url::Host::Domain(host)) = url.host() else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && IMAGE_HOST_SUFFIXES.iter().any(|suffix| {
            host == *suffix
                || host
                    .strip_suffix(suffix)
                    .is_some_and(|rest| rest.ends_with('.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_provider_image_urls() {
        assert!(is_provider_image_url(
            "https://i.ytimg.com/vi/x/hqdefault.jpg"
        ));
        assert!(is_provider_image_url(
            "https://i1.sndcdn.com/artworks-x-t500x500.jpg"
        ));
        assert!(is_provider_image_url(
            "https://lh3.googleusercontent.com/abc=w544"
        ));
        assert!(!is_provider_image_url(
            "http://i.ytimg.com/vi/x/hqdefault.jpg"
        ));
        assert!(!is_provider_image_url("https://evilytimg.com/pixel.gif"));
        assert!(!is_provider_image_url(
            "https://i.ytimg.com.evil.example/p.gif"
        ));
        assert!(!is_provider_image_url("https://i.ytimg.com:8443/p.gif"));
        assert!(!is_provider_image_url("https://tracker.example/p.gif"));
    }

    #[test]
    fn rejects_unsafe_urls() {
        assert!(parse_user_url("file:///etc/passwd").is_err());
        assert!(parse_user_url("https://user:pw@youtube.com/watch?v=x").is_err());
        assert!(parse_user_url("not a url").is_err());
        assert!(parse_user_url(&format!("https://x.com/{}", "a".repeat(3000))).is_err());
        assert!(parse_user_url("  https://youtu.be/dQw4w9WgXcQ ").is_ok());
    }
}
