//! Application services: the operations the UI can perform.
//!
//! The HTTP layer only translates requests to these calls; all validation,
//! orchestration and permission checks live here so they are usable and
//! testable without HTTP. Every operation on behalf of a user takes the
//! authenticated [`User`] as `actor`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::auth::{
    Auth, AuthError, Authenticated, Enrollment, Login, LoginOutcome, NewAccount, Role,
    TwoFactorEnabled, TwoFactorPolicy, TwoFactorStatus, User, UserId,
};
use crate::config::Config;
use crate::domain::length::{Clock, HiddenTooLong};
use crate::domain::provider::{ProviderKind, TrackKey};
use crate::domain::quality::OutputFormat;
use crate::jobs::{EnqueueError, JobId, JobManager, JobSnapshot, Viewer};
use crate::library::archive::{
    self, ArchiveFile, ArchiveStreams, PreparedArchive, PreparedArchives, StreamSlot,
};
use crate::library::sanitize::sanitize_component;
use crate::library::{Library, LibraryEntry, LibraryError, Removal, RemoveFrom};
use crate::metadata::{clean_artists, clean_text};
use crate::playback::range::ByteRange;
use crate::playback::waveform::Waveform;
use crate::playback::{Playback, PlaybackError, PreparedStream, StreamId, StreamResponse};
use crate::providers::{
    ArtistPage, MediaEngine, ProviderError, ProviderRegistry, Resolved, SearchDetail, TrackSummary,
    UrlKind, is_provider_image_url, parse_user_url,
};
use crate::rooms::{MessageView, RoomError, RoomId, RoomView, Rooms, SharedTrack, TrackStatus};
use crate::search::{FilterSummary, RawFilters, SearchFilters};
use crate::settings::{ServerSettings, Settings, SettingsError};
use crate::soulseek::{
    AccountStatus, MemberRules, Override, Session as SoulseekSession, Soulseek, SoulseekAccess,
    SoulseekError, UpgradeWanted,
};
use crate::tools::ToolStatus;
use crate::tools::ffmpeg::Ffmpeg;
use crate::tools::ytdlp::YtDlp;

const MAX_QUERY_CHARS: usize = 200;
/// With filters set, ask providers for this many times the results to show,
/// since some of them will be filtered out.
const FILTER_OVERFETCH: usize = 3;
/// Upper bound on results fetched (and, for YouTube, looked up one by one)
/// per provider for a filtered search.
const MAX_FILTER_CANDIDATES: usize = 60;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Enqueue(#[from] EnqueueError),
    #[error("{0}")]
    DependencyMissing(String),
    #[error("not found")]
    NotFound,
    #[error("only the captain may do that")]
    Forbidden,
    #[error("{0}")]
    TooBusy(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Playback(#[from] PlaybackError),
    #[error(transparent)]
    Soulseek(#[from] SoulseekError),
    #[error(transparent)]
    Rooms(#[from] RoomError),
    #[error(transparent)]
    Library(#[from] LibraryError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

#[derive(Debug, Serialize)]
pub struct ProviderFailure {
    pub provider: ProviderKind,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<TrackSummary>,
    /// Providers that failed while others succeeded.
    pub errors: Vec<ProviderFailure>,
    /// Set when filters were applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filtered: Option<FilterSummary>,
    /// Results left out for being over the length limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub too_long: Option<HiddenTooLong>,
}

/// Optional display hints sent with a download request (e.g. from a search
/// result) so the queue shows a title before metadata is resolved. They are
/// never used as metadata.
#[derive(Debug, Default, Deserialize)]
pub struct EnqueueRequest {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// Requested file format; defaults to the original source format.
    #[serde(default)]
    pub output: OutputFormat,
}

/// What to listen to, and in which kind of browser.
#[derive(Debug, Deserialize)]
pub struct StreamRequest {
    #[serde(flatten)]
    pub target: StreamTarget,
    /// Older UIs do not send it.
    #[serde(default)]
    pub engine: MediaEngine,
}

/// What to listen to.
#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum StreamTarget {
    /// A track at a provider, e.g. a search result; nothing is downloaded.
    Provider { url: String },
    /// A file in the library, identified like a file download.
    Library {
        provider: String,
        id: String,
        output: String,
    },
    /// A track shared in a chat room the actor is a member of.
    Shared { room: u64, seq: u64 },
}

/// A library track (identified like a file download) to share in a room.
#[derive(Debug, Deserialize)]
pub struct ShareRequest {
    pub provider: String,
    pub id: String,
    pub output: String,
}

#[derive(Debug, Serialize)]
pub struct EnqueueResponse {
    pub job_ids: Vec<JobId>,
    /// Set when only part of a collection could be queued.
    pub warning: Option<String>,
}

/// A job as one user sees it. Captains also see who requested it.
#[derive(Debug, Serialize)]
pub struct JobView {
    #[serde(flatten)]
    pub job: JobSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<Vec<String>>,
}

/// A library entry as one user sees it. Captains also see its owners;
/// pirates never see other users' ids or names.
#[derive(Debug, Serialize)]
pub struct LibraryItem {
    #[serde(flatten)]
    pub entry: LibraryEntry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<Vec<String>>,
    /// Sort key for the file's real quality (see
    /// [`crate::domain::quality::QualityReport::library_rank`]); higher is
    /// better.
    pub quality_rank: u64,
    /// Every other format, and whether this track can be had in it.
    pub convert_to: Vec<ConvertOption>,
    /// Whether a lossless copy can be looked for on Soulseek. Absent for
    /// tracks that came from Soulseek in the first place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<UpgradeOption>,
}

/// Whether a quality upgrade is possible for a library track.
#[derive(Debug, Serialize)]
pub struct UpgradeOption {
    pub available: bool,
    /// Why it is unavailable, or a note such as "already on disk".
    pub note: Option<String>,
}

/// Look for a lossless copy of a library track (identified like a file
/// download; `from` is the format the user picked it in).
#[derive(Debug, Deserialize)]
pub struct UpgradeRequest {
    pub provider: String,
    pub id: String,
    pub from: String,
}

/// One format a library track could be converted to.
#[derive(Debug, Serialize)]
pub struct ConvertOption {
    pub output: OutputFormat,
    pub available: bool,
    /// Why it is unavailable, or a note such as "already on disk".
    pub note: Option<String>,
}

/// Convert a library track (identified like a file download) to `to`.
#[derive(Debug, Deserialize)]
pub struct ConvertRequest {
    pub provider: String,
    pub id: String,
    pub from: String,
    pub to: OutputFormat,
}

/// Remove one format of a library track (identified like a file download).
/// With `LibraryScope::All` a captain removes it for everyone.
#[derive(Debug, Deserialize)]
pub struct RemoveRequest {
    pub provider: String,
    pub id: String,
    pub output: String,
    #[serde(default)]
    pub scope: LibraryScope,
}

/// Most files one bulk library action takes: far more than anyone selects
/// by hand, small enough that a request stays cheap to check.
pub const MAX_BULK_FILES: usize = 1000;

/// One format of a library track, identified like a file download.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRef {
    pub provider: String,
    pub id: String,
    pub output: String,
}

/// Several library files to act on together.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkRequest {
    pub files: Vec<FileRef>,
    #[serde(default)]
    pub scope: LibraryScope,
}

/// What a bulk removal did, file by file counted.
#[derive(Debug, Default, Serialize)]
pub struct BulkRemoval {
    pub deleted: usize,
    /// Left the user's library; others still have the file.
    pub unlisted: usize,
    /// Not (or no longer) in the library the request named.
    pub missing: usize,
    /// "Title: reason" for each file that could not be removed.
    pub failed: Vec<String>,
}

/// A ZIP download made ready by [`App::prepare_archive`].
#[derive(Debug, Serialize)]
pub struct ArchiveTicket {
    /// Fetch it from `/api/library/archive/{id}`.
    pub id: String,
    pub file_name: String,
    pub files: usize,
    pub bytes: u64,
    /// Selected files that are no longer in the library, left out.
    pub missing: usize,
}

/// Which part of the shared library (or download queue) to act on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryScope {
    /// Tracks (or downloads) the user asked for.
    #[default]
    Mine,
    /// Everything on disk, or every queued job (captains only).
    All,
}

/// An account as listed for the captain.
#[derive(Debug, Serialize)]
pub struct CrewMember {
    #[serde(flatten)]
    pub user: User,
    /// Library files this user owns.
    pub tracks: usize,
    /// The 2FA policy requires two-factor login and it is not set up yet.
    pub two_factor_pending: bool,
    /// The captains' choices for this member over the Soulseek switches.
    pub soulseek: MemberRules,
    /// What the switches and those choices add up to.
    pub soulseek_allowed: bool,
    pub soulseek_sharing_allowed: bool,
}

/// A captain's change to the server settings; fields left out stay as
/// they are. Relaxing the 2FA policy or allowing Soulseek sharing needs the
/// captain's password (and a code): a hijacked session must not be able to
/// switch 2FA off, or start uploading from the host's address.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    #[serde(default)]
    pub two_factor: Option<TwoFactorPolicy>,
    #[serde(default)]
    pub totp_issuer: Option<String>,
    #[serde(default)]
    pub max_track_secs: Option<u32>,
    #[serde(default)]
    pub soulseek: Option<SoulseekAccess>,
    #[serde(default)]
    pub soulseek_sharing: Option<SoulseekAccess>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

/// A captain signs on a user. There is no password field: Inkult generates
/// a temporary one. Unknown fields are refused, so a client still sending a
/// password learns it is not used instead of having it silently dropped.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    pub username: String,
    pub role: Role,
}

/// Changes a captain can make to an account; absent fields stay as they are.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserUpdate {
    #[serde(default)]
    pub role: Option<Role>,
    /// Replace the password with a generated temporary one.
    #[serde(default)]
    pub reset_password: bool,
    /// Lift a lock set by too many failed logins.
    #[serde(default)]
    pub unlock: bool,
    /// Remove two-factor login (lost phone).
    #[serde(default)]
    pub reset_2fa: bool,
    /// Override the Soulseek switch for this member.
    #[serde(default)]
    pub soulseek: Option<Override>,
    /// Override the Soulseek sharing switch for this member.
    #[serde(default)]
    pub soulseek_sharing: Option<Override>,
    /// Confirms letting the member share more (with `code`).
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

/// Server status for the UI. Server paths and tool details are for
/// captains only: pirates learn just what works and what doesn't.
#[derive(Debug, Serialize)]
pub struct Status {
    pub version: &'static str,
    pub tools: Vec<ToolView>,
    pub providers: Vec<ProviderKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_dir: Option<String>,
}

/// One external tool as a user sees it. `program`, `version` and `error`
/// (which can name paths on the server) are filled for captains only.
#[derive(Debug, Serialize)]
pub struct ToolView {
    pub name: &'static str,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ToolView {
    fn new(tool: ToolStatus, captain: bool) -> Self {
        let available = tool.available();
        if captain {
            Self {
                name: tool.name,
                available,
                program: Some(tool.program),
                version: tool.version,
                error: tool.error,
            }
        } else {
            Self {
                name: tool.name,
                available,
                program: None,
                version: None,
                error: None,
            }
        }
    }
}

pub struct App {
    config: Config,
    settings: Settings,
    auth: Auth,
    registry: ProviderRegistry,
    jobs: JobManager,
    library: Arc<Library>,
    playback: Arc<Playback>,
    soulseek: Arc<Soulseek>,
    rooms: Rooms,
    ytdlp: Arc<YtDlp>,
    ffmpeg: Arc<Ffmpeg>,
    tool_status: RwLock<Vec<ToolStatus>>,
    archives: PreparedArchives,
    archive_streams: ArchiveStreams,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: Config,
        settings: Settings,
        auth: Auth,
        registry: ProviderRegistry,
        jobs: JobManager,
        library: Arc<Library>,
        playback: Arc<Playback>,
        soulseek: Arc<Soulseek>,
        rooms: Rooms,
        ytdlp: Arc<YtDlp>,
        ffmpeg: Arc<Ffmpeg>,
    ) -> Self {
        let app = Self {
            config,
            settings,
            auth,
            registry,
            jobs,
            library,
            playback,
            soulseek,
            rooms,
            ytdlp,
            ffmpeg,
            tool_status: RwLock::new(Vec::new()),
            archives: PreparedArchives::default(),
            archive_streams: ArchiveStreams::default(),
        };
        app.refresh_tools().await;
        app
    }

    async fn refresh_tools(&self) -> Vec<ToolStatus> {
        let (ytdlp, ffmpeg) = tokio::join!(self.ytdlp.status(), self.ffmpeg.status());
        for tool in [&ytdlp, &ffmpeg] {
            match (&tool.version, &tool.error) {
                (Some(version), _) => {
                    tracing::info!(tool = tool.name, %version, "external tool available")
                }
                (None, error) => {
                    tracing::error!(tool = tool.name, program = %tool.program, error = ?error, "external tool unavailable")
                }
            }
        }
        let statuses = vec![ytdlp, ffmpeg];
        *self.tool_status.write().await = statuses.clone();
        statuses
    }

    /// Fail fast with a clear message when a required tool is missing.
    /// Missing tools are re-checked so installing one needs no restart.
    async fn require_tools(&self, names: &[&str]) -> Result<(), AppError> {
        let mut statuses = self.tool_status.read().await.clone();
        let missing = |s: &[ToolStatus]| {
            s.iter()
                .filter(|t| names.contains(&t.name) && !t.available())
                .map(|t| format!("{} ({})", t.name, t.program))
                .collect::<Vec<_>>()
        };
        if !missing(&statuses).is_empty() {
            statuses = self.refresh_tools().await;
        }
        let still_missing = missing(&statuses);
        if still_missing.is_empty() {
            Ok(())
        } else {
            Err(AppError::DependencyMissing(format!(
                "required tool not available: {}; install it or set INKULT_YTDLP / INKULT_FFMPEG",
                still_missing.join(", ")
            )))
        }
    }

    /// How many crew members currently have Inkult open.
    pub fn online_count(&self) -> usize {
        self.auth.online_count()
    }

    pub async fn status(&self, actor: &User) -> Status {
        // Re-check missing tools so installing one clears the UI warning
        // without restarting Inkult.
        let mut tools = self.tool_status.read().await.clone();
        if tools.iter().any(|t| !t.available()) {
            tools = self.refresh_tools().await;
        }
        let captain = actor.role.is_captain();
        Status {
            version: env!("CARGO_PKG_VERSION"),
            tools: tools
                .into_iter()
                .map(|tool| ToolView::new(tool, captain))
                .collect(),
            providers: self.registry.all().map(|p| p.kind()).collect(),
            library_dir: captain.then(|| self.library.root().display().to_string()),
        }
    }

    /// Search providers for `query`, keeping only results that pass
    /// `filters`. With a label filter and no query, searches for the label.
    pub async fn search(
        &self,
        query: &str,
        provider: Option<ProviderKind>,
        filters: &RawFilters,
    ) -> Result<SearchResponse, AppError> {
        let filters =
            SearchFilters::parse(filters).map_err(|e| AppError::BadRequest(e.to_string()))?;
        let query = clean_text(query)
            .or_else(|| filters.label_text())
            .filter(|q| q.chars().count() <= MAX_QUERY_CHARS)
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "search query must be 1–{MAX_QUERY_CHARS} characters"
                ))
            })?;
        self.require_tools(&["yt-dlp"]).await?;

        let providers: Vec<_> = match provider {
            Some(kind) => vec![self.registry.get(kind).ok_or(AppError::NotFound)?],
            None => self.registry.all().cloned().collect(),
        };
        let limit = self.config.search_limit;
        let (fetch, detail) = if filters.is_active() {
            let detail = if filters.needs_release_info() {
                SearchDetail::Release
            } else {
                SearchDetail::Listing
            };
            (
                (limit * FILTER_OVERFETCH).min(MAX_FILTER_CANDIDATES),
                detail,
            )
        } else {
            (limit, SearchDetail::Listing)
        };
        let mut tasks = tokio::task::JoinSet::new();
        for (order, provider) in providers.into_iter().enumerate() {
            let query = query.clone();
            tasks.spawn(async move {
                let outcome = provider.search(&query, fetch, detail).await;
                (order, provider.kind(), outcome)
            });
        }

        let mut outcomes = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => tracing::error!(error = %e, "search task panicked"),
            }
        }
        outcomes.sort_by_key(|(order, _, _)| *order);

        let mut response = SearchResponse {
            results: Vec::new(),
            errors: Vec::new(),
            filtered: None,
            too_long: None,
        };
        let mut summary = FilterSummary::default();
        let mut too_long = 0;
        for (_, provider, outcome) in outcomes {
            // Before the filters, so the overfetch is spent on real tracks
            // and the filter counts don't include mixes.
            let outcome = outcome.map(|mut results| {
                if let Some(hidden) = self.drop_too_long(&mut results) {
                    too_long += hidden.count;
                }
                results
            });
            match outcome {
                Ok(results) if filters.is_active() => {
                    let kept = filters.apply(results, limit, &mut summary);
                    response.results.extend(kept);
                }
                Ok(mut results) => response.results.append(&mut results),
                Err(e) => {
                    tracing::warn!(%provider, error = %e, "search failed");
                    response.errors.push(ProviderFailure {
                        provider,
                        message: e.to_string(),
                    });
                }
            }
        }
        response.filtered = filters.is_active().then_some(summary);
        response.too_long = self
            .settings
            .track_limit()
            .get()
            .secs()
            .filter(|_| too_long > 0)
            .map(|max_secs| HiddenTooLong {
                count: too_long,
                max_secs,
            });
        if response.results.is_empty() && !response.errors.is_empty() && provider.is_some() {
            let message = response.errors.remove(0).message;
            return Err(AppError::BadRequest(message));
        }
        Ok(response)
    }

    pub async fn resolve(&self, raw_url: &str) -> Result<Resolved, AppError> {
        let url = parse_user_url(raw_url)?;
        let (provider, _) = self
            .registry
            .classify(&url)
            .ok_or(ProviderError::UnsupportedUrl)?;
        self.require_tools(&["yt-dlp"]).await?;
        let resolved = provider.resolve(&url, self.config.playlist_limit).await?;
        self.limit_length(resolved)
    }

    /// Leave out listed tracks that are over the length limit.
    fn drop_too_long(&self, tracks: &mut Vec<TrackSummary>) -> Option<HiddenTooLong> {
        self.settings
            .track_limit()
            .get()
            .drop_too_long(tracks, |t| t.duration_secs)
    }

    /// A single track over the length limit is refused; a collection loses
    /// its tracks that are.
    fn limit_length(&self, resolved: Resolved) -> Result<Resolved, AppError> {
        match resolved {
            Resolved::Track(track) => {
                self.settings
                    .track_limit()
                    .get()
                    .check(track.duration_secs)
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                Ok(Resolved::Track(track))
            }
            Resolved::Collection(mut collection) => {
                collection.too_long = self.drop_too_long(&mut collection.tracks);
                Ok(Resolved::Collection(collection))
            }
        }
    }

    /// An artist's or uploader's page: biography, link to the provider's
    /// page and the tracks they posted (up to the playlist limit), with
    /// the ones they reposted if asked and the provider has reposts.
    pub async fn artist(
        &self,
        raw_url: &str,
        include_reposts: bool,
    ) -> Result<ArtistPage, AppError> {
        let url = parse_user_url(raw_url)?;
        let provider = self
            .registry
            .artist_provider(&url)
            .ok_or(ProviderError::UnsupportedUrl)?;
        self.require_tools(&["yt-dlp"]).await?;
        let mut page = provider
            .artist(&url, self.config.playlist_limit, include_reposts)
            .await?;
        page.too_long = self.drop_too_long(&mut page.tracks);
        // Shown in every viewer's browser: only the provider's image CDN.
        page.image_url = page.image_url.filter(|u| is_provider_image_url(u));
        Ok(page)
    }

    /// Queue a track, or every track of a playlist/album/set, for `actor`.
    /// Tracks already on disk are not downloaded again; they are added to
    /// the actor's library when their job runs.
    pub async fn enqueue(
        &self,
        actor: &User,
        request: EnqueueRequest,
    ) -> Result<EnqueueResponse, AppError> {
        if !request.output.is_downloadable() {
            return Err(AppError::BadRequest(format!(
                "{} cannot be downloaded; use Quality upgrade on a library track",
                request.output.label()
            )));
        }
        let url = parse_user_url(&request.url)?;
        let (provider, kind) = self
            .registry
            .classify(&url)
            .ok_or(ProviderError::UnsupportedUrl)?;
        self.require_tools(&["yt-dlp", "ffmpeg"]).await?;

        if kind == UrlKind::Track {
            let hint = TrackSummary {
                provider: provider.kind(),
                url: url.to_string(),
                title: request
                    .title
                    .as_deref()
                    .and_then(clean_text)
                    .unwrap_or_else(|| url.to_string()),
                artists: clean_artists(&request.artists),
                album: None,
                duration_secs: None,
                track_number: None,
                thumbnail_url: request.thumbnail_url.filter(|u| is_provider_image_url(u)),
                year: None,
                label: None,
                label_hints: Vec::new(),
                artist_links: Vec::new(),
                reposted: false,
            };
            let id = self
                .jobs
                .enqueue(provider, url, Some(&hint), request.output, actor.id)?;
            return Ok(EnqueueResponse {
                job_ids: vec![id],
                warning: None,
            });
        }

        let resolved = provider.resolve(&url, self.config.playlist_limit).await?;
        let (tracks, too_long) = match self.limit_length(resolved)? {
            Resolved::Track(track) => (vec![track], None),
            Resolved::Collection(collection) => (collection.tracks, collection.too_long),
        };
        if tracks.is_empty() {
            return Err(AppError::BadRequest(match too_long {
                Some(hidden) => format!(
                    "every track in this collection is over the {} limit (likely mixes or sets)",
                    Clock(f64::from(hidden.max_secs))
                ),
                None => "this collection contains no tracks".into(),
            }));
        }
        let total = tracks.len();
        let mut job_ids = Vec::with_capacity(total);
        for track in &tracks {
            let Ok(track_url) = parse_user_url(&track.url) else {
                continue;
            };
            let Some((track_provider, UrlKind::Track)) = self.registry.classify(&track_url) else {
                continue;
            };
            match self.jobs.enqueue(
                track_provider,
                track_url,
                Some(track),
                request.output,
                actor.id,
            ) {
                Ok(id) => job_ids.push(id),
                Err(e) if job_ids.is_empty() => return Err(e.into()),
                Err(e) => {
                    return Ok(EnqueueResponse {
                        warning: Some(format!("Queued {} of {total} tracks: {e}", job_ids.len())),
                        job_ids,
                    });
                }
            }
        }
        let mut notes = Vec::new();
        if job_ids.len() < total {
            notes.push(format!(
                "Queued {} of {total} tracks; some entries were not downloadable",
                job_ids.len()
            ));
        }
        if let Some(hidden) = too_long {
            notes.push(format!(
                "Skipped {} over {} (likely mixes or sets)",
                hidden.count,
                Clock(f64::from(hidden.max_secs))
            ));
        }
        let warning = (!notes.is_empty()).then(|| notes.join("; "));
        Ok(EnqueueResponse { job_ids, warning })
    }

    /// Captains see their own jobs like anyone else unless they ask for
    /// the whole queue.
    fn viewer(actor: &User, scope: LibraryScope) -> Result<Viewer, AppError> {
        match scope {
            LibraryScope::Mine => Ok(Viewer::User(actor.id)),
            LibraryScope::All => {
                Self::require_captain(actor)?;
                Ok(Viewer::All)
            }
        }
    }

    fn require_captain(actor: &User) -> Result<(), AppError> {
        if actor.role.is_captain() {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    fn names(names: &HashMap<UserId, String>, ids: &BTreeSet<UserId>) -> Vec<String> {
        let mut list: Vec<String> = ids
            .iter()
            .map(|id| names.get(id).cloned().unwrap_or_else(|| format!("#{id}")))
            .collect();
        list.sort();
        list
    }

    /// The actor's jobs, or (captains, `LibraryScope::All`) every job.
    /// Captains also see who asked for each one.
    pub async fn jobs(&self, actor: &User, scope: LibraryScope) -> Result<Vec<JobView>, AppError> {
        let jobs = self.jobs.list(Self::viewer(actor, scope)?);
        if !actor.role.is_captain() {
            return Ok(jobs
                .into_iter()
                .map(|job| JobView {
                    job,
                    requested_by: None,
                })
                .collect());
        }
        let names = self.auth.usernames().await;
        Ok(jobs
            .into_iter()
            .map(|job| JobView {
                requested_by: Some(Self::names(&names, &job.requesters)),
                job,
            })
            .collect())
    }

    /// In `LibraryScope::Mine` a captain only withdraws from a job shared
    /// with others, like a pirate; `All` stops it for everyone.
    pub fn cancel_job(&self, actor: &User, id: JobId, scope: LibraryScope) -> Result<(), AppError> {
        if self.jobs.cancel(id, Self::viewer(actor, scope)?) {
            Ok(())
        } else {
            Err(AppError::NotFound)
        }
    }

    /// Skip the Soulseek copy a quality upgrade is waiting for.
    pub fn skip_job(&self, actor: &User, id: JobId, scope: LibraryScope) -> Result<(), AppError> {
        if self.jobs.skip(id, Self::viewer(actor, scope)?) {
            Ok(())
        } else {
            Err(AppError::NotFound)
        }
    }

    pub fn clear_finished_jobs(
        &self,
        actor: &User,
        scope: LibraryScope,
    ) -> Result<usize, AppError> {
        Ok(self.jobs.clear_finished(Self::viewer(actor, scope)?))
    }

    fn can_access(actor: &User, entry: &LibraryEntry) -> bool {
        actor.role.is_captain() || entry.owners.contains(&actor.id)
    }

    /// The actor's library, or (captains, `LibraryScope::All`) everything
    /// on disk with its owners.
    pub async fn library(
        &self,
        actor: &User,
        scope: LibraryScope,
    ) -> Result<Vec<LibraryItem>, AppError> {
        if scope == LibraryScope::All {
            Self::require_captain(actor)?;
        }
        let all = self.library.entries().await;
        let options = ConversionOptions::new(&all, actor.id);
        let soulseek_block = self.upgrade_session(actor).err().map(|e| e.to_string());
        let names = if actor.role.is_captain() {
            Some(self.auth.usernames().await)
        } else {
            None
        };
        Ok(all
            .iter()
            .filter(|e| scope == LibraryScope::All || e.owners.contains(&actor.id))
            .map(|entry| {
                let owned_by = names.as_ref().map(|n| Self::names(n, &entry.owners));
                let convert_to = options.for_entry(entry);
                let upgrade = options.upgrade_for(entry, soulseek_block.as_deref());
                let mut entry = entry.clone();
                // "Added" in the viewer's own library is when they got the
                // file, which for a shared file can be long after it was
                // first downloaded; the whole hold shows the file's time.
                if scope == LibraryScope::Mine {
                    entry.added_at = entry.added_for(actor.id);
                }
                entry.owners.clear();
                entry.owned_since.clear();
                LibraryItem {
                    quality_rank: entry.quality.library_rank(),
                    entry,
                    owned_by,
                    convert_to,
                    upgrade,
                }
            })
            .collect())
    }

    /// Queue converting a library track to another format for `actor`. The
    /// conversion starts from the best copy of the track on disk, which may
    /// be another user's (only the result is added to the actor's library).
    /// If the format is already on disk, it is just added to their library.
    pub async fn convert(
        &self,
        actor: &User,
        request: ConvertRequest,
    ) -> Result<EnqueueResponse, AppError> {
        let (entry, _) = self
            .library_entry(actor, &request.provider, &request.id, &request.from)
            .await?;
        let target = request.to;
        if let Some(existing) = self.library.find(&entry.key, target).await {
            if existing.owners.contains(&actor.id) {
                return Err(AppError::BadRequest(format!(
                    "this track is already in your library as {}",
                    target.label()
                )));
            }
        } else {
            let (best, _) = self
                .library
                .best_source(&entry.key)
                .await
                .ok_or(AppError::NotFound)?;
            target
                .library_conversion(&best.quality)
                .map_err(|reason| AppError::BadRequest(format!("cannot convert: {reason}")))?;
            self.require_tools(&["ffmpeg"]).await?;
        }
        let id = self.jobs.enqueue_conversion(&entry, target, actor.id)?;
        Ok(EnqueueResponse {
            job_ids: vec![id],
            warning: None,
        })
    }

    /// Queue a quality upgrade: look for a lossless copy of a library track
    /// on Soulseek, through the actor's own Soulseek session, and add it as
    /// the track's Lossless format. If one is already on disk it is just
    /// added to their library (`job_ids` then stays empty).
    pub async fn upgrade(
        &self,
        actor: &User,
        request: UpgradeRequest,
    ) -> Result<EnqueueResponse, AppError> {
        let (entry, _) = self
            .library_entry(actor, &request.provider, &request.id, &request.from)
            .await?;
        if entry.key.provider == ProviderKind::Soulseek {
            return Err(AppError::BadRequest(
                "quality upgrades are for tracks downloaded from search; this one came from Soulseek"
                    .into(),
            ));
        }
        let lossless = OutputFormat::Lossless;
        if let Some(existing) = self.library.find(&entry.key, lossless).await {
            if existing.owners.contains(&actor.id) {
                return Err(AppError::BadRequest(
                    "a lossless copy of this track is already in your library".into(),
                ));
            }
            self.library
                .add_owners(&entry.key, lossless, &BTreeSet::from([actor.id]))
                .await;
            return Ok(EnqueueResponse {
                job_ids: Vec::new(),
                warning: Some("A lossless copy was already on disk; added to your library".into()),
            });
        }
        let (best, _) = self
            .library
            .best_source(&entry.key)
            .await
            .ok_or(AppError::NotFound)?;
        if best.quality.lossless {
            return Err(AppError::BadRequest(format!(
                "this track is already lossless ({})",
                best.quality.summary
            )));
        }
        let session = self.upgrade_session(actor)?;
        self.require_tools(&["ffmpeg"]).await?;
        let wanted = UpgradeWanted::from_library(&self.library, &entry.key)
            .await
            .ok_or(AppError::NotFound)?;
        let id = self
            .jobs
            .enqueue_upgrade(&entry, wanted, session, actor.id)?;
        tracing::info!(user = %actor.username, track = %entry.key, job = id, "quality upgrade queued");
        Ok(EnqueueResponse {
            job_ids: vec![id],
            warning: None,
        })
    }

    /// Quality upgrade straight from a search result: look for a lossless
    /// copy of a provider's track on Soulseek without downloading the
    /// provider's own (lossy) stream. The job resolves the track's metadata
    /// and cover from the provider to tag the file with.
    pub async fn upgrade_from_search(
        &self,
        actor: &User,
        request: EnqueueRequest,
    ) -> Result<EnqueueResponse, AppError> {
        let url = parse_user_url(&request.url)?;
        let (provider, kind) = self
            .registry
            .classify(&url)
            .ok_or(ProviderError::UnsupportedUrl)?;
        if kind != UrlKind::Track {
            return Err(AppError::BadRequest(
                "quality upgrades are for single tracks, not playlists or albums".into(),
            ));
        }
        let session = self.upgrade_session(actor)?;
        self.require_tools(&["yt-dlp", "ffmpeg"]).await?;
        let title = request
            .title
            .as_deref()
            .and_then(clean_text)
            .unwrap_or_else(|| url.to_string());
        let artists = clean_artists(&request.artists);
        let thumbnail_url = request.thumbnail_url.filter(|u| is_provider_image_url(u));
        let id = self.jobs.enqueue_upgrade_from_source(
            provider,
            url,
            (title, artists, thumbnail_url),
            session,
            actor.id,
        )?;
        tracing::info!(user = %actor.username, job = id, "quality upgrade from search queued");
        Ok(EnqueueResponse {
            job_ids: vec![id],
            warning: None,
        })
    }

    /// The actor's Soulseek session, which quality upgrades search with.
    fn upgrade_session(&self, actor: &User) -> Result<Arc<SoulseekSession>, AppError> {
        self.soulseek_allowed(actor).map_err(|_| {
            AppError::BadRequest(
                "Soulseek is turned off on this server, and upgrades search it".into(),
            )
        })?;
        self.soulseek.require(actor.id).map_err(|_| {
            AppError::BadRequest(
                "sign in to Soulseek in [2] SOULSEEK first; upgrades search it with your account"
                    .into(),
            )
        })
    }

    /// Remove a track in one format from the actor's library; its file is
    /// deleted once nobody else has it. A captain removing from the whole
    /// library (`LibraryScope::All`) deletes it for everyone.
    pub async fn remove_from_library(
        &self,
        actor: &User,
        request: RemoveRequest,
    ) -> Result<Removal, AppError> {
        let from = match request.scope {
            LibraryScope::Mine => RemoveFrom::Only(actor.id),
            LibraryScope::All => {
                Self::require_captain(actor)?;
                RemoveFrom::Everyone
            }
        };
        let provider: ProviderKind = request.provider.parse().map_err(|_| AppError::NotFound)?;
        let output: OutputFormat = request.output.parse().map_err(|_| AppError::NotFound)?;
        let key = TrackKey {
            provider,
            id: request.id,
        };
        let removal = self
            .library
            .remove(&key, output, from)
            .await?
            .ok_or(AppError::NotFound)?;
        tracing::info!(
            user = %actor.username,
            track = %key,
            output = output.label(),
            ?removal,
            "removed from library"
        );
        Ok(removal)
    }

    /// [`App::remove_from_library`] for several files at once. Files that
    /// are gone are counted, failures listed; neither stops the rest.
    pub async fn remove_many_from_library(
        &self,
        actor: &User,
        request: BulkRequest,
    ) -> Result<BulkRemoval, AppError> {
        let from = match request.scope {
            LibraryScope::Mine => RemoveFrom::Only(actor.id),
            LibraryScope::All => {
                Self::require_captain(actor)?;
                RemoveFrom::Everyone
            }
        };
        let files = Self::bulk_files(request.files)?;
        let mut summary = BulkRemoval::default();
        let mut items = Vec::with_capacity(files.len());
        for file in &files {
            match Self::parse_file_ref(file) {
                Some(item) => items.push(item),
                None => summary.missing += 1,
            }
        }
        let results = self.library.remove_many(&items, from).await;
        for ((key, output), result) in items.iter().zip(results) {
            match result {
                Ok(Some(removal)) => {
                    tracing::info!(
                        user = %actor.username,
                        track = %key,
                        output = output.label(),
                        ?removal,
                        "removed from library"
                    );
                    match removal {
                        Removal::Deleted => summary.deleted += 1,
                        Removal::Unlisted => summary.unlisted += 1,
                    }
                }
                Ok(None) => summary.missing += 1,
                Err(e) => {
                    tracing::error!(user = %actor.username, track = %key, error = %e, "bulk removal failed");
                    let title = match self.library.find(key, *output).await {
                        Some(entry) => entry.title,
                        None => key.to_string(),
                    };
                    summary.failed.push(format!("{title}: {e}"));
                }
            }
        }
        Ok(summary)
    }

    /// Check a set of library files and get them ready to be fetched as
    /// one ZIP, folders kept as in the library. Files no longer there are
    /// left out and counted.
    pub async fn prepare_archive(
        &self,
        actor: &User,
        request: BulkRequest,
    ) -> Result<ArchiveTicket, AppError> {
        let refs = Self::bulk_files(request.files)?;
        let mut files = Vec::with_capacity(refs.len());
        let mut sources = Vec::with_capacity(refs.len());
        let mut missing = 0;
        for file in &refs {
            let Ok((entry, path)) = self
                .library_entry(actor, &file.provider, &file.id, &file.output)
                .await
            else {
                missing += 1;
                continue;
            };
            let Ok(meta) = tokio::fs::metadata(&path).await else {
                missing += 1;
                continue;
            };
            // A path refused as an archive name (see `entry_name`) still
            // gets its file, under a sanitized file name at the top level.
            let name = archive::entry_name(&entry.path).unwrap_or_else(|| {
                let raw = entry
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                sanitize_component(&raw, "track")
            });
            files.push(ArchiveFile {
                name: archive::fit_name(name),
                path,
                size: meta.len(),
                modified: meta.modified().ok(),
            });
            sources.push((entry.key, entry.output));
        }
        if files.is_empty() {
            return Err(AppError::BadRequest(
                "none of the selected files are in your library any more".into(),
            ));
        }
        archive::dedupe_names(&mut files);
        let file_name = format!(
            "inkult-{}-track{}.zip",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        );
        let ticket = ArchiveTicket {
            id: String::new(),
            file_name: file_name.clone(),
            files: files.len(),
            bytes: archive::archive_len(&files),
            missing,
        };
        tracing::info!(user = %actor.username, files = ticket.files, bytes = ticket.bytes, "archive prepared");
        let id = self.archives.insert(actor.id, file_name, files, sources);
        Ok(ArchiveTicket { id, ..ticket })
    }

    /// An archive the actor prepared, to be sent now, with a slot counting
    /// it as running. Access is checked again: a file the actor lost (or
    /// that was deleted or replaced) since preparing is left out, and sizes
    /// are read afresh so the announced length is right.
    pub async fn prepared_archive(
        &self,
        actor: &User,
        id: &str,
    ) -> Result<(PreparedArchive, StreamSlot), AppError> {
        let mut prepared = self.archives.get(id, actor.id).ok_or(AppError::NotFound)?;
        let slot = self.archive_streams.start(actor.id).ok_or_else(|| {
            AppError::TooBusy(
                "other ZIP downloads are still running; wait for them to finish".into(),
            )
        })?;
        let mut files = Vec::with_capacity(prepared.files.len());
        for (file, (key, output)) in prepared.files.drain(..).zip(&prepared.sources) {
            let Some(entry) = self
                .library
                .find(key, *output)
                .await
                .filter(|e| Self::can_access(actor, e))
            else {
                continue;
            };
            if self.library.file_path(&entry).await.as_ref() != Some(&file.path) {
                continue;
            }
            let Ok(meta) = tokio::fs::metadata(&file.path).await else {
                continue;
            };
            files.push(ArchiveFile {
                size: meta.len(),
                modified: meta.modified().ok(),
                ..file
            });
        }
        if files.is_empty() {
            return Err(AppError::NotFound);
        }
        prepared.files = files;
        Ok((prepared, slot))
    }

    /// Share several library tracks in a room at once. Either all of them
    /// are sent or (when one is not the actor's to share) none.
    pub async fn share_tracks(
        &self,
        actor: &User,
        room: RoomId,
        request: BulkRequest,
    ) -> Result<Vec<MessageView>, AppError> {
        let files = Self::bulk_files(request.files)?;
        let mut entries = Vec::with_capacity(files.len());
        for file in &files {
            let (entry, _) = self
                .library_entry(actor, &file.provider, &file.id, &file.output)
                .await
                .map_err(|_| {
                    AppError::BadRequest(
                        "a selected track is no longer in your library; nothing was sent".into(),
                    )
                })?;
            entries.push(entry);
        }
        let tracks = entries.iter().map(SharedTrack::from).collect();
        let mut messages = self.rooms.share_tracks(actor, room, tracks).await?;
        for (message, entry) in messages.iter_mut().zip(&entries) {
            message.track_status = Some(if entry.owners.contains(&actor.id) {
                TrackStatus::InLibrary
            } else {
                TrackStatus::Available
            });
        }
        tracing::info!(by = %actor.username, tracks = messages.len(), "tracks shared in chat room");
        Ok(messages)
    }

    /// The files of a bulk request, each once, in the order given.
    fn bulk_files(files: Vec<FileRef>) -> Result<Vec<FileRef>, AppError> {
        if files.is_empty() {
            return Err(AppError::BadRequest("no files selected".into()));
        }
        if files.len() > MAX_BULK_FILES {
            return Err(AppError::BadRequest(format!(
                "at most {MAX_BULK_FILES} files at a time"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        Ok(files
            .into_iter()
            .filter(|f| seen.insert(f.clone()))
            .collect())
    }

    fn parse_file_ref(file: &FileRef) -> Option<(TrackKey, OutputFormat)> {
        let provider: ProviderKind = file.provider.parse().ok()?;
        let output: OutputFormat = file.output.parse().ok()?;
        Some((
            TrackKey {
                provider,
                id: file.id.clone(),
            },
            output,
        ))
    }

    /// Find a library entry the actor may access, and its file.
    async fn library_entry(
        &self,
        actor: &User,
        provider: &str,
        id: &str,
        output: &str,
    ) -> Result<(LibraryEntry, std::path::PathBuf), AppError> {
        let provider: ProviderKind = provider.parse().map_err(|_| AppError::NotFound)?;
        let output: OutputFormat = output.parse().map_err(|_| AppError::NotFound)?;
        let key = TrackKey {
            provider,
            id: id.to_owned(),
        };
        let entry = self
            .library
            .find(&key, output)
            .await
            .filter(|e| Self::can_access(actor, e))
            .ok_or(AppError::NotFound)?;
        let path = self
            .library
            .file_path(&entry)
            .await
            .ok_or(AppError::NotFound)?;
        Ok((entry, path))
    }

    /// Locate a library file for download by its identity and format.
    pub async fn library_file(
        &self,
        actor: &User,
        provider: &str,
        id: &str,
        output: &str,
    ) -> Result<LibraryFile, AppError> {
        let (entry, path) = self.library_entry(actor, provider, id, output).await?;
        let file_name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "track".to_owned());
        Ok(LibraryFile { path, file_name })
    }

    /// Get a track ready to be listened to in the browser.
    pub async fn prepare_stream(
        &self,
        actor: &User,
        request: StreamRequest,
    ) -> Result<PreparedStream, AppError> {
        let engine = request.engine;
        match request.target {
            StreamTarget::Provider { url } => {
                let url = parse_user_url(&url)?;
                let (provider, kind) = self
                    .registry
                    .classify(&url)
                    .ok_or(ProviderError::UnsupportedUrl)?;
                if kind != UrlKind::Track {
                    return Err(AppError::BadRequest(
                        "only single tracks can be played; open the collection to play its tracks"
                            .into(),
                    ));
                }
                // ffmpeg is only needed for some streams and reported when used.
                self.require_tools(&["yt-dlp"]).await?;
                Ok(self.playback.prepare_remote(provider, url, engine).await?)
            }
            StreamTarget::Library {
                provider,
                id,
                output,
            } => {
                let (entry, path) = self.library_entry(actor, &provider, &id, &output).await?;
                Ok(self
                    .playback
                    .prepare_local(entry.key.provider, path, &entry.quality, actor.id, engine)
                    .await?)
            }
            StreamTarget::Shared { room, seq } => {
                let (entry, path) = self.shared_entry(actor, RoomId(room), seq).await?;
                Ok(self
                    .playback
                    .prepare_local(entry.key.provider, path, &entry.quality, actor.id, engine)
                    .await?)
            }
        }
    }

    pub async fn open_stream(
        &self,
        actor: &User,
        id: StreamId,
        range: Option<ByteRange>,
        start_secs: Option<f64>,
    ) -> Result<StreamResponse, AppError> {
        Ok(self.playback.open(id, actor.id, range, start_secs).await?)
    }

    pub async fn stream_waveform(
        &self,
        actor: &User,
        id: StreamId,
    ) -> Result<Arc<Waveform>, AppError> {
        Ok(self.playback.waveform(id, actor.id).await?)
    }

    // ---------- soulseek ----------
    //
    // Each Inkult user has their own Soulseek account and session; every
    // operation acts on the actor's own session only.

    /// Whether the captains' rules let the actor use Soulseek.
    fn soulseek_allowed(&self, actor: &User) -> Result<(), AppError> {
        if self.settings.soulseek_rules().may_use(actor.id, actor.role) {
            Ok(())
        } else {
            Err(SoulseekError::Disabled.into())
        }
    }

    pub async fn soulseek_status(&self, actor: &User) -> AccountStatus {
        if self.soulseek_allowed(actor).is_err() {
            return self.soulseek.disabled_status(actor.id, actor.role).await;
        }
        // Soulseek is back on for them after a pause: reconnect a
        // remembered account (only full-access sessions get here).
        self.soulseek.resume_suspended(actor.id, actor.role);
        self.soulseek.status(actor.id, actor.role).await
    }

    pub async fn soulseek_sign_in(
        &self,
        actor: &User,
        username: &str,
        password: &str,
        remember: bool,
    ) -> Result<AccountStatus, AppError> {
        self.soulseek_allowed(actor)?;
        Ok(self
            .soulseek
            .sign_in(actor.id, actor.role, username, password, remember)
            .await?)
    }

    pub async fn soulseek_sign_out(&self, actor: &User) {
        self.soulseek.sign_out(actor.id).await;
    }

    /// The actor's signed-in Soulseek session.
    pub fn soulseek(&self, actor: &User) -> Result<Arc<SoulseekSession>, AppError> {
        self.soulseek_allowed(actor)?;
        Ok(self.soulseek.require(actor.id)?)
    }

    // ---------- accounts ----------

    /// Chat rooms; every operation takes the acting user and enforces who
    /// may do what itself.
    pub fn rooms(&self) -> &Rooms {
        &self.rooms
    }

    /// A room's messages, with each shared track's status for the actor.
    pub async fn room_view(
        &self,
        actor: &User,
        room: RoomId,
        after: Option<u64>,
    ) -> Result<RoomView, AppError> {
        let mut view = self.rooms.view(actor, room, after).await?;
        self.annotate_tracks(actor, &mut view.messages).await;
        Ok(view)
    }

    pub async fn join_room(&self, actor: &User, room: RoomId) -> Result<RoomView, AppError> {
        let mut view = self.rooms.join(actor, room).await?;
        self.annotate_tracks(actor, &mut view.messages).await;
        Ok(view)
    }

    pub async fn join_private_room(
        &self,
        actor: &User,
        name: &str,
        password: &str,
    ) -> Result<RoomView, AppError> {
        let mut view = self.rooms.join_private(actor, name, password).await?;
        self.annotate_tracks(actor, &mut view.messages).await;
        Ok(view)
    }

    async fn annotate_tracks(&self, actor: &User, messages: &mut [MessageView]) {
        for message in messages {
            let Some(track) = &message.track else {
                continue;
            };
            let status = match self.library.find(&track.key, track.output).await {
                None => TrackStatus::Gone,
                Some(entry) if entry.owners.contains(&actor.id) => TrackStatus::InLibrary,
                Some(_) => TrackStatus::Available,
            };
            message.track_status = Some(status);
        }
    }

    /// Share one of the actor's library tracks in a room they are in. Every
    /// member of the room may then play it and add it to their library.
    pub async fn share_track(
        &self,
        actor: &User,
        room: RoomId,
        request: ShareRequest,
    ) -> Result<MessageView, AppError> {
        let (entry, _) = self
            .library_entry(actor, &request.provider, &request.id, &request.output)
            .await?;
        let mut message = self
            .rooms
            .share_track(actor, room, SharedTrack::from(&entry))
            .await?;
        message.track_status = Some(if entry.owners.contains(&actor.id) {
            TrackStatus::InLibrary
        } else {
            TrackStatus::Available
        });
        tracing::info!(track = %entry.key, by = %actor.username, "track shared in chat room");
        Ok(message)
    }

    /// The library entry and file behind a shared track, if the actor is in
    /// the room and the file still exists.
    async fn shared_entry(
        &self,
        actor: &User,
        room: RoomId,
        seq: u64,
    ) -> Result<(LibraryEntry, std::path::PathBuf), AppError> {
        let track = self.rooms.shared_track(actor, room, seq).await?;
        let gone = || AppError::BadRequest("this track is no longer in the library".into());
        let entry = self
            .library
            .find(&track.key, track.output)
            .await
            .ok_or_else(gone)?;
        let path = self.library.file_path(&entry).await.ok_or_else(gone)?;
        Ok((entry, path))
    }

    /// Add a track shared in a room to the actor's library. The file is
    /// already on disk, so this only records the actor as an owner, like
    /// downloading a track someone else already has.
    pub async fn claim_shared_track(
        &self,
        actor: &User,
        room: RoomId,
        seq: u64,
    ) -> Result<(), AppError> {
        let (entry, _) = self.shared_entry(actor, room, seq).await?;
        if entry.owners.contains(&actor.id) {
            return Err(AppError::BadRequest(
                "this track is already in your library".into(),
            ));
        }
        self.library
            .add_owners(&entry.key, entry.output, &BTreeSet::from([actor.id]))
            .await
            .ok_or(AppError::NotFound)?;
        tracing::info!(track = %entry.key, user = %actor.username, "shared track added to library");
        Ok(())
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn setup_required(&self) -> bool {
        self.auth.setup_required()
    }

    pub fn session_max_age(&self) -> std::time::Duration {
        self.auth.session_max_age()
    }

    pub async fn setup(
        &self,
        code: &str,
        username: &str,
        password: &str,
    ) -> Result<Login, AppError> {
        let login = self.auth.setup(code, username, password).await?;
        self.after_login(&login);
        Ok(login)
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<LoginOutcome, AppError> {
        let outcome = self.auth.login(username, password).await?;
        if let LoginOutcome::Complete(login) = &outcome {
            self.after_login(login);
        }
        Ok(outcome)
    }

    /// A login finished. Only once the session has full access (second
    /// factor passed, no password change or 2FA enrollment pending) does
    /// the user's remembered Soulseek account reconnect: until then the
    /// person at the keyboard has not proven to be the account's owner.
    fn after_login(&self, login: &Login) {
        if !login.password_change_required
            && !self.auth.two_factor_enrollment_required(&login.user)
            && self.soulseek_allowed(&login.user).is_ok()
        {
            self.soulseek
                .resume_remembered(login.user.id, login.user.role);
        }
    }

    /// Whether the policy requires 2FA for this account and it is not on.
    pub fn two_factor_enrollment_required(&self, user: &User) -> bool {
        self.auth.two_factor_enrollment_required(user)
    }

    pub async fn login_second_factor(
        &self,
        challenge: &str,
        code: &str,
    ) -> Result<Login, AppError> {
        let login = self.auth.login_second_factor(challenge, code).await?;
        self.after_login(&login);
        Ok(login)
    }

    pub async fn two_factor_status(&self, actor: &User) -> Result<TwoFactorStatus, AppError> {
        Ok(self.auth.two_factor_status(actor).await?)
    }

    pub async fn begin_two_factor(&self, actor: &User) -> Result<Enrollment, AppError> {
        Ok(self.auth.begin_two_factor(actor).await?)
    }

    pub async fn enable_two_factor(
        &self,
        actor: &User,
        password: &str,
        code: &str,
    ) -> Result<TwoFactorEnabled, AppError> {
        let enabled = self.auth.enable_two_factor(actor, password, code).await?;
        // Lifts the enrollment restriction a 2FA policy puts on the session.
        self.after_login(&enabled.login);
        Ok(enabled)
    }

    pub async fn disable_two_factor(
        &self,
        actor: &User,
        password: &str,
        code: &str,
    ) -> Result<(), AppError> {
        Ok(self.auth.disable_two_factor(actor, password, code).await?)
    }

    pub async fn replace_authenticator(
        &self,
        actor: &User,
        password: &str,
        current_code: &str,
        new_code: &str,
    ) -> Result<Login, AppError> {
        let login = self
            .auth
            .replace_authenticator(actor, password, current_code, new_code)
            .await?;
        self.after_login(&login);
        Ok(login)
    }

    pub async fn regenerate_recovery_codes(
        &self,
        actor: &User,
        password: &str,
        code: &str,
    ) -> Result<Vec<String>, AppError> {
        Ok(self
            .auth
            .regenerate_recovery_codes(actor, password, code)
            .await?)
    }

    pub fn logout(&self, token: &str) {
        self.auth.logout(token);
    }

    pub async fn authenticate(&self, token: &str) -> Option<Authenticated> {
        self.auth.authenticate(token).await
    }

    pub async fn change_own_password(
        &self,
        actor: &User,
        current: &str,
        new: &str,
    ) -> Result<Login, AppError> {
        let login = self.auth.change_own_password(actor, current, new).await?;
        // Lifts the restriction a temporary or weak password puts on the
        // session.
        self.after_login(&login);
        Ok(login)
    }

    pub async fn crew(&self, actor: &User) -> Result<Vec<CrewMember>, AppError> {
        Self::require_captain(actor)?;
        let mut counts: HashMap<UserId, usize> = HashMap::new();
        for entry in self.library.entries().await {
            for owner in entry.owners {
                *counts.entry(owner).or_default() += 1;
            }
        }
        let settings = self.settings.get().await;
        let rules = self.settings.soulseek_rules();
        Ok(self
            .auth
            .list()
            .await
            .into_iter()
            .map(|user| CrewMember {
                tracks: counts.get(&user.id).copied().unwrap_or(0),
                two_factor_pending: self.auth.two_factor_enrollment_required(&user),
                soulseek: settings
                    .soulseek_members
                    .get(&user.id)
                    .copied()
                    .unwrap_or_default(),
                soulseek_allowed: rules.may_use(user.id, user.role),
                soulseek_sharing_allowed: rules.may_share(user.id, user.role),
                user,
            })
            .collect())
    }

    pub async fn server_settings(&self, actor: &User) -> Result<ServerSettings, AppError> {
        Self::require_captain(actor)?;
        Ok(self.settings.get().await)
    }

    /// Change the server settings (captains). They are saved first and
    /// applied at once: a stricter 2FA policy restricts every session
    /// without it on its next request.
    pub async fn update_server_settings(
        &self,
        actor: &User,
        update: SettingsUpdate,
    ) -> Result<ServerSettings, AppError> {
        Self::require_captain(actor)?;
        // Held throughout, so two captains' changes cannot interleave.
        let mut current = self.settings.lock().await;
        let mut next = current.clone();
        if let Some(policy) = update.two_factor {
            next.two_factor = policy;
        }
        if let Some(issuer) = update.totp_issuer {
            next.totp_issuer = issuer.trim().to_owned();
        }
        if let Some(secs) = update.max_track_secs {
            next.max_track_secs = secs;
        }
        if let Some(access) = update.soulseek {
            next.soulseek = access;
        }
        if let Some(sharing) = update.soulseek_sharing {
            next.soulseek_sharing = sharing;
        }
        self.commit_settings(
            actor,
            &mut current,
            next,
            update.password.as_deref(),
            update.code.as_deref(),
        )
        .await
    }

    /// Check, save and apply `next` in place of `current` (whose lock the
    /// caller holds throughout, so two captains' changes cannot interleave).
    /// `password` and `code` confirm a change that loosens security.
    async fn commit_settings(
        &self,
        actor: &User,
        current: &mut tokio::sync::MutexGuard<'_, ServerSettings>,
        next: ServerSettings,
        password: Option<&str>,
        code: Option<&str>,
    ) -> Result<ServerSettings, AppError> {
        next.validate().map_err(AppError::BadRequest)?;
        if next == **current {
            return Ok(next);
        }
        let relaxed = current.two_factor.is_weakened_by(next.two_factor);
        let roles: HashMap<UserId, Role> = (self.auth.list().await)
            .into_iter()
            .map(|user| (user.id, user.role))
            .collect();
        let sharing_widened = current.widens_sharing(&next, |id| roles.get(&id).copied());
        if relaxed || sharing_widened {
            let password = password.ok_or_else(|| {
                AppError::BadRequest(if relaxed {
                    "relaxing the two-factor rule needs your password and a code".into()
                } else {
                    "allowing more Soulseek sharing needs your password and a code".into()
                })
            })?;
            self.auth.confirm_identity(actor, password, code).await?;
        }
        let previous = (**current).clone();
        self.settings.replace(current, next.clone()).await?;
        self.auth.set_config(next.auth());
        if relaxed {
            tracing::warn!(captain = %actor.username, from = ?previous.two_factor, to = ?next.two_factor, "two-factor rule relaxed");
        } else if previous.two_factor != next.two_factor {
            tracing::info!(captain = %actor.username, to = ?next.two_factor, "two-factor rule tightened");
        }
        if previous.totp_issuer != next.totp_issuer {
            tracing::info!(captain = %actor.username, issuer = %next.totp_issuer, "authenticator name changed");
        }
        if previous.max_track_secs != next.max_track_secs {
            tracing::info!(captain = %actor.username, max_track_secs = next.max_track_secs, "track length limit changed");
        }
        // Still under the settings lock, so a captain flipping a rule back
        // cannot overtake the sessions being brought in line with it.
        if previous.soulseek != next.soulseek {
            tracing::warn!(captain = %actor.username, from = ?previous.soulseek, to = ?next.soulseek, "Soulseek access changed");
        }
        if previous.soulseek_sharing != next.soulseek_sharing {
            tracing::warn!(captain = %actor.username, from = ?previous.soulseek_sharing, to = ?next.soulseek_sharing, "Soulseek sharing rule changed");
        }
        let members_changed = previous.soulseek_members != next.soulseek_members;
        if previous.soulseek != next.soulseek || members_changed {
            let rules = self.settings.soulseek_rules();
            for user in self.auth.list().await {
                if !rules.may_use(user.id, user.role) {
                    self.soulseek.suspend(user.id).await;
                }
            }
        }
        if previous.soulseek_sharing != next.soulseek_sharing || members_changed {
            self.soulseek.apply_sharing_rule().await;
        }
        Ok(next)
    }

    /// A captain's choice for one member over the Soulseek switches.
    async fn set_member_rules(
        &self,
        actor: &User,
        id: UserId,
        update: &UserUpdate,
    ) -> Result<(), AppError> {
        let user = self
            .auth
            .list()
            .await
            .into_iter()
            .find(|u| u.id == id)
            .ok_or(AppError::NotFound)?;
        let mut current = self.settings.lock().await;
        let mut next = current.clone();
        let mut member = next.soulseek_members.get(&id).copied().unwrap_or_default();
        if let Some(choice) = update.soulseek {
            member.soulseek = choice;
        }
        if let Some(choice) = update.soulseek_sharing {
            member.sharing = choice;
        }
        if member.is_default() {
            next.soulseek_members.remove(&id);
        } else {
            next.soulseek_members.insert(id, member);
        }
        let changed = next.soulseek_members != current.soulseek_members;
        self.commit_settings(
            actor,
            &mut current,
            next,
            update.password.as_deref(),
            update.code.as_deref(),
        )
        .await?;
        if changed {
            tracing::warn!(captain = %actor.username, user = %user.username, soulseek = ?member.soulseek, sharing = ?member.sharing, "Soulseek choice for a member changed");
        }
        Ok(())
    }

    pub async fn create_user(&self, actor: &User, new: NewUser) -> Result<NewAccount, AppError> {
        Self::require_captain(actor)?;
        Ok(self.auth.create_user(&new.username, new.role).await?)
    }

    /// Apply a captain's changes to an account. Returns the temporary
    /// password when one was generated (`reset_password`).
    pub async fn update_user(
        &self,
        actor: &User,
        id: UserId,
        update: UserUpdate,
    ) -> Result<Option<String>, AppError> {
        Self::require_captain(actor)?;
        if (update.reset_password || update.reset_2fa) && id == actor.id {
            // Resetting without the current password is for other accounts;
            // otherwise a stolen session could lock the captain out, or
            // strip their second factor.
            return Err(AppError::BadRequest(
                "change your own password or two-factor login from your own settings; they ask for the current password"
                    .into(),
            ));
        }
        // First: it is the change that may be refused for want of the
        // captain's password, and nothing else should happen then.
        if update.soulseek.is_some() || update.soulseek_sharing.is_some() {
            self.set_member_rules(actor, id, &update).await?;
        }
        let temporary_password = if update.reset_password {
            Some(self.auth.reset_password(id).await?)
        } else {
            None
        };
        if let Some(role) = update.role {
            self.auth.set_role(id, role).await?;
            self.soulseek.member_changed(id, role).await;
        }
        if update.unlock {
            self.auth.unlock(id).await?;
        }
        if update.reset_2fa {
            self.auth.reset_two_factor(id).await?;
        }
        Ok(temporary_password)
    }

    /// Delete an account. Its files stay in the shared library (other users
    /// may own them, and account cleanup never deletes music); only its
    /// ownership, pending requests and room memberships are dropped.
    pub async fn delete_user(&self, actor: &User, id: UserId) -> Result<(), AppError> {
        Self::require_captain(actor)?;
        if id == actor.id {
            return Err(AppError::BadRequest(
                "you cannot delete your own account; ask another captain".into(),
            ));
        }
        self.auth.delete(id).await?;
        self.jobs.forget_user(id);
        self.soulseek.forget_user(id).await;
        self.rooms.forget_user(id).await;
        self.library.remove_owner(id).await;
        // Last, once nothing of theirs runs: dropping a block would read as
        // letting them in, but they are gone. So no confirmation (and
        // nothing above can be skipped by one being refused). Ids are
        // random, so a choice left over after a failed save never applies
        // to anyone else.
        let mut current = self.settings.lock().await;
        if current.soulseek_members.contains_key(&id) {
            let mut next = current.clone();
            next.soulseek_members.remove(&id);
            self.settings.replace(&mut current, next).await?;
        }
        Ok(())
    }
}

/// Which formats each track of the library can be had in, for one user.
/// Computed from the index alone (no disk access) for listing; the
/// conversion job re-checks against the files when it runs.
struct ConversionOptions<'a> {
    /// Best copy of each track, as a conversion source.
    best: HashMap<&'a TrackKey, &'a LibraryEntry>,
    /// Formats each track exists in, and whether the user owns that copy.
    present: HashMap<(&'a TrackKey, OutputFormat), bool>,
}

impl<'a> ConversionOptions<'a> {
    fn new(entries: &'a [LibraryEntry], user: UserId) -> Self {
        let mut best: HashMap<&TrackKey, &LibraryEntry> = HashMap::new();
        let mut present = HashMap::new();
        for entry in entries {
            best.entry(&entry.key)
                .and_modify(|b| {
                    if entry.quality.source_rank() > b.quality.source_rank() {
                        *b = entry;
                    }
                })
                .or_insert(entry);
            *present.entry((&entry.key, entry.output)).or_insert(false) |=
                entry.owners.contains(&user);
        }
        Self { best, present }
    }

    fn for_entry(&self, entry: &LibraryEntry) -> Vec<ConvertOption> {
        let source = self.best.get(&entry.key).copied().unwrap_or(entry);
        OutputFormat::ALL
            .into_iter()
            // A lossless copy is never converted to; see `upgrade_for`.
            .filter(|&output| output != entry.output && output != OutputFormat::Lossless)
            .map(|output| {
                let (available, note) = match self.present.get(&(&entry.key, output)) {
                    Some(true) => (false, Some("already in your library".to_owned())),
                    Some(false) => (true, Some("already on disk; added instantly".to_owned())),
                    None => match output.library_conversion(&source.quality) {
                        Ok(_) => (true, None),
                        Err(reason) => (false, Some(reason)),
                    },
                };
                ConvertOption {
                    output,
                    available,
                    note,
                }
            })
            .collect()
    }
}

impl ConversionOptions<'_> {
    /// Whether the user can ask for a quality upgrade of `entry`'s track.
    /// Soulseek tracks get none: they are already someone's best copy.
    /// `soulseek_block` says why the user cannot search Soulseek, if so.
    fn upgrade_for(
        &self,
        entry: &LibraryEntry,
        soulseek_block: Option<&str>,
    ) -> Option<UpgradeOption> {
        if entry.key.provider == ProviderKind::Soulseek {
            return None;
        }
        let (available, note) = match self.present.get(&(&entry.key, OutputFormat::Lossless)) {
            Some(true) => (false, "a lossless copy is already in your library".to_owned()),
            Some(false) => (true, "a lossless copy is on disk; added instantly".to_owned()),
            None => match self.best.get(&entry.key) {
                Some(best) if best.quality.lossless => (
                    false,
                    format!("already lossless ({})", best.quality.summary),
                ),
                _ => match soulseek_block {
                    Some(reason) => (false, reason.to_owned()),
                    None => (
                        true,
                        "Find a lossless copy (FLAC, WAV, AIFF) on Soulseek and add it as another format"
                            .to_owned(),
                    ),
                },
            },
        };
        Some(UpgradeOption {
            available,
            note: Some(note),
        })
    }
}

/// A library file ready to be sent to the user's device.
pub struct LibraryFile {
    pub path: std::path::PathBuf,
    pub file_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quality::{Codec, QualityReport};

    fn user(id: u64, role: Role) -> User {
        User {
            id: UserId(id),
            username: format!("user{id}"),
            role,
            created_at: 0,
            locked_at: None,
            two_factor_enabled: true,
            password_temporary: false,
            temporary_password_expires_at: None,
        }
    }

    fn entry(
        provider: ProviderKind,
        output: OutputFormat,
        codec: Codec,
        owner: u64,
    ) -> LibraryEntry {
        LibraryEntry {
            key: TrackKey {
                provider,
                id: "t".into(),
            },
            path: "Artist/Singles/T.x".into(),
            title: "T".into(),
            artists: vec!["Artist".into()],
            album: None,
            source_url: "https://example/t".into(),
            artwork_url: None,
            quality: QualityReport::new(
                codec,
                "x".into(),
                Some(900),
                Some(44_100),
                Some(16),
                Some(2),
            ),
            output,
            added_at: 1,
            owners: BTreeSet::from([UserId(owner)]),
            owned_since: Default::default(),
            obtained_from: None,
        }
    }

    #[test]
    fn upgrades_are_offered_for_lossy_tracks_from_search_only() {
        let opus = entry(
            ProviderKind::YouTube,
            OutputFormat::Original,
            Codec::Opus,
            1,
        );
        let entries = [opus.clone()];
        let options = ConversionOptions::new(&entries, UserId(1));
        let offer = options.upgrade_for(&opus, None).unwrap();
        assert!(offer.available);
        let offer = options.upgrade_for(&opus, Some("not signed in")).unwrap();
        assert!(!offer.available, "needs a Soulseek session");
        // Lossless is not a conversion target.
        assert!(
            options
                .for_entry(&opus)
                .iter()
                .all(|o| o.output != OutputFormat::Lossless)
        );

        let slsk = entry(
            ProviderKind::Soulseek,
            OutputFormat::Original,
            Codec::Mp3,
            1,
        );
        let entries = [slsk.clone()];
        assert!(
            ConversionOptions::new(&entries, UserId(1))
                .upgrade_for(&slsk, None)
                .is_none()
        );

        let wav = entry(
            ProviderKind::SoundCloud,
            OutputFormat::Original,
            Codec::Pcm,
            1,
        );
        let entries = [wav.clone()];
        let offer = ConversionOptions::new(&entries, UserId(1))
            .upgrade_for(&wav, None)
            .unwrap();
        assert!(!offer.available, "already lossless");

        // Someone else's upgrade is on disk: added instantly, even signed out.
        let flac = entry(
            ProviderKind::YouTube,
            OutputFormat::Lossless,
            Codec::Flac,
            2,
        );
        let entries = [opus.clone(), flac];
        let options = ConversionOptions::new(&entries, UserId(1));
        let offer = options.upgrade_for(&opus, Some("not signed in")).unwrap();
        assert!(offer.available);
        let options = ConversionOptions::new(&entries, UserId(2));
        assert!(!options.upgrade_for(&opus, None).unwrap().available);
    }

    #[test]
    fn download_queue_defaults_to_own_jobs_for_everyone() {
        let captain = user(1, Role::Captain);
        let pirate = user(2, Role::Pirate);
        assert_eq!(
            App::viewer(&captain, LibraryScope::Mine).ok(),
            Some(Viewer::User(UserId(1)))
        );
        assert_eq!(
            App::viewer(&pirate, LibraryScope::Mine).ok(),
            Some(Viewer::User(UserId(2)))
        );
        assert_eq!(
            App::viewer(&captain, LibraryScope::All).ok(),
            Some(Viewer::All)
        );
        assert!(matches!(
            App::viewer(&pirate, LibraryScope::All),
            Err(AppError::Forbidden)
        ));
    }
}
