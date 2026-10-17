//! SoundCloud.
//!
//! Some uploaders enable the "original file" download, which yt-dlp exposes
//! as the `download` format; it is frequently WAV/FLAC/AIFF and is picked up
//! automatically by quality selection when present. Otherwise SoundCloud
//! streams are lossy (MP3 128k / Opus 64k / AAC 160k depending on track).
//!
//! Artist pages are uploader profiles. The profile page embeds data for its
//! own scripts (`window.__sc_hydration`): the biography, the user id, and
//! the public client id of SoundCloud's web app. With those, the tracks are
//! listed through the API the web app itself uses, which (unlike yt-dlp's
//! flat profile listing) includes artwork, duration and dates. Neither is
//! an official interface: if they fail, yt-dlp lists the tracks without
//! those details, and the biography is left out.
//!
//! With reposts included, the listing is the profile's "All" stream: posts
//! and reposts interleaved by when they were posted or reposted.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::common::{
    base_metadata, classify_error, download_with_ytdlp, pick_stream, read_body_capped,
    summary_from_entry,
};
use super::{
    ArtistLink, ArtistPage, Collection, DownloadedMedia, MusicProvider, ProviderError, Resolved,
    SearchDetail, StreamSource, SummaryYear, TrackDetails, TrackSummary, UrlKind, YearBasis,
};
use crate::domain::DownloadProgress;
use serde_json::Value;

use crate::domain::metadata::ReleaseDate;
use crate::domain::provider::ProviderKind;
use crate::domain::quality::AudioFormat;
use crate::metadata::{clean_multiline, clean_text};
use crate::tools::ytdlp::{YtDlp, YtInfo};

const KIND: ProviderKind = ProviderKind::SoundCloud;

/// First path segments that are site sections, not user profiles.
const RESERVED_ROOTS: &[&str] = &[
    "discover",
    "search",
    "stream",
    "you",
    "upload",
    "charts",
    "pages",
    "settings",
    "messages",
    "notifications",
    "terms-of-use",
    "jobs",
    "imprint",
    "mobile",
    "signin",
];
/// Second path segments that are profile tabs, not tracks.
const PROFILE_TABS: &[&str] = &[
    "tracks",
    "albums",
    "likes",
    "reposts",
    "popular-tracks",
    "followers",
    "following",
    "comments",
    "spotlight",
];

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
const MAX_PROFILE_PAGE_BYTES: usize = 4 * 1024 * 1024;
const HYDRATION_START: &str = "window.__sc_hydration = ";
const API_HOST: &str = "api-v2.soundcloud.com";
const TRACKS_PAGE_SIZE: usize = 200;
/// Bounds the requests made for one profile's track list.
const MAX_TRACK_PAGES: usize = 50;
const MAX_API_PAGE_BYTES: usize = 16 * 1024 * 1024;

pub struct SoundCloudProvider {
    ytdlp: Arc<YtDlp>,
    /// `None` if the HTTP client could not be built; artist pages then have
    /// no biography.
    web: Option<reqwest::Client>,
}

impl SoundCloudProvider {
    pub fn new(ytdlp: Arc<YtDlp>) -> Self {
        let web = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .inspect_err(|e| tracing::warn!(error = %e, "SoundCloud profile pages unavailable"))
            .ok();
        Self { ytdlp, web }
    }

    /// `user` must already be validated with [`parse_user`].
    async fn profile(&self, user: &str) -> Option<Profile> {
        let web = self.web.as_ref()?;
        let result = async {
            let response = web
                .get(profile_url(user))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let body = read_body_capped(response, MAX_PROFILE_PAGE_BYTES).await?;
            parse_profile(&String::from_utf8_lossy(&body)).ok_or_else(|| "no profile data".into())
        };
        result
            .await
            .inspect_err(|e: &String| tracing::warn!(error = %e, "SoundCloud profile page failed"))
            .ok()
    }

    /// The tracks a user posted (and reposted, with `include_reposts`),
    /// newest first, at most `max_tracks`.
    async fn api_tracks(
        &self,
        profile: &Profile,
        max_tracks: usize,
        include_reposts: bool,
    ) -> Result<TrackList, String> {
        let web = self.web.as_ref().ok_or("no HTTP client")?;
        let (Some(user_id), Some(client_id)) = (profile.user_id, profile.client_id.as_deref())
        else {
            return Err("the profile page has no user or client id".into());
        };
        let (path, parse_item): (_, fn(&Value) -> Option<TrackSummary>) = if include_reposts {
            (format!("stream/users/{user_id}"), stream_item)
        } else {
            (format!("users/{user_id}/tracks"), api_track)
        };
        let mut url =
            Url::parse(&format!("https://{API_HOST}/{path}")).map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("limit", &TRACKS_PAGE_SIZE.min(max_tracks).to_string())
            .append_pair("linked_partitioning", "1");
        let mut tracks = Vec::new();
        let mut seen = HashSet::new();
        for _ in 0..MAX_TRACK_PAGES {
            let response = web
                .get(with_client_id(&url, client_id))
                .header("Origin", "https://soundcloud.com")
                .send()
                .await
                // The URL holds the client id: keep it out of messages.
                .map_err(|e| e.without_url().to_string())?;
            let body = read_body_capped(response, MAX_API_PAGE_BYTES).await?;
            let page: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            tracks.extend(
                page.get("collection")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(parse_item)
                    // A track reposted again later is listed once.
                    .filter(|track| seen.insert(track.url.clone())),
            );
            // Only follow pages on the API host itself.
            let next = page
                .get("next_href")
                .and_then(Value::as_str)
                .and_then(|u| Url::parse(u).ok())
                .filter(|u| u.scheme() == "https" && u.host_str() == Some(API_HOST));
            match next {
                Some(next) if tracks.len() < max_tracks => url = next,
                next => {
                    let truncated = tracks.len() > max_tracks || next.is_some();
                    tracks.truncate(max_tracks);
                    return Ok(TrackList { tracks, truncated });
                }
            }
        }
        tracks.truncate(max_tracks);
        Ok(TrackList {
            tracks,
            truncated: true,
        })
    }

    /// Fallback listing through yt-dlp: titles and links only.
    async fn ytdlp_tracks(
        &self,
        user: &str,
        uploader: &str,
        max_tracks: usize,
        include_reposts: bool,
    ) -> Result<TrackList, ProviderError> {
        // The bare profile URL is the "All" stream, reposts included.
        let listing = if include_reposts {
            profile_url(user)
        } else {
            format!("{}/tracks", profile_url(user))
        };
        let info = self
            .ytdlp
            .resolve_flat(&listing, max_tracks)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        let truncated = info.entries().count() >= max_tracks;
        let tracks = info
            .into_entries()
            .filter_map(|mut entry| {
                // Profile listings leave out the uploader of each track. A
                // track under someone else's profile was reposted; its
                // uploader's name is unknown here.
                let reposted =
                    include_reposts && !track_url(&entry).is_some_and(|url| posted_by(&url, user));
                if !reposted {
                    entry.uploader.get_or_insert_with(|| uploader.to_owned());
                }
                let mut summary = summarize(entry)?;
                summary.reposted = reposted;
                Some(summary)
            })
            .take(max_tracks)
            .collect();
        Ok(TrackList { tracks, truncated })
    }
}

struct TrackList {
    tracks: Vec<TrackSummary>,
    truncated: bool,
}

/// `url` with `client_id` set (replacing any already there).
fn with_client_id(url: &Url, client_id: &str) -> Url {
    let mut out = url.clone();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "client_id")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    out.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("client_id", client_id);
    out
}

/// "-large" artwork is 100px; the same image exists at 500px.
fn large_image(url: &str) -> Option<String> {
    url.starts_with("https://")
        .then(|| url.replace("-large.", "-t500x500."))
}

/// One track of an API listing, in the shape yt-dlp reports it, so that
/// search results and profile listings are normalized the same way.
fn api_track(track: &Value) -> Option<TrackSummary> {
    if track.get("kind").and_then(Value::as_str) != Some("track") {
        return None;
    }
    let text = |pointer: &str| {
        track
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    // SoundCloud shows the uploader's avatar for tracks without artwork.
    let artwork = text("/artwork_url")
        .or_else(|| text("/user/avatar_url"))
        .and_then(|u| large_image(&u));
    let date = |pointer: &str| text(pointer).and_then(|d| d.get(..10).map(str::to_owned));
    let entry = YtInfo {
        id: track
            .get("id")
            .and_then(Value::as_u64)
            .map(|id| id.to_string()),
        title: text("/title"),
        artists: text("/publisher_metadata/artist").map(|artist| vec![artist]),
        album: text("/publisher_metadata/album_title"),
        release_date: date("/release_date").or_else(|| date("/publisher_metadata/release_date")),
        uploader: text("/user/username"),
        uploader_url: text("/user/permalink_url"),
        webpage_url: text("/permalink_url"),
        duration: track
            .get("duration")
            .and_then(Value::as_f64)
            .map(|ms| ms / 1000.0),
        thumbnail: artwork,
        genre: text("/genre"),
        description: text("/description"),
        ..Default::default()
    };
    let mut summary = summarize(entry)?;
    if summary.year.is_none() {
        summary.year = date("/created_at")
            .as_deref()
            .and_then(ReleaseDate::parse)
            .map(|d| SummaryYear {
                year: d.year,
                basis: YearBasis::Upload,
            });
    }
    Some(summary)
}

/// One item of a profile's "All" stream: a track that was posted or
/// reposted. Playlists and their reposts are left out, as on the tracks
/// listing.
fn stream_item(item: &Value) -> Option<TrackSummary> {
    let reposted = match item.get("type").and_then(Value::as_str)? {
        "track" => false,
        "track-repost" => true,
        _ => return None,
    };
    let mut summary = api_track(item.get("track")?)?;
    summary.reposted = reposted;
    Some(summary)
}

/// Whether canonical track URL `url` is under `user`'s profile.
fn posted_by(url: &str, user: &str) -> bool {
    url.strip_prefix("https://soundcloud.com/")
        .and_then(|path| path.split_once('/'))
        .is_some_and(|(owner, _)| owner.eq_ignore_ascii_case(user))
}

/// What a profile page says about its owner.
#[derive(Debug, Default, PartialEq)]
struct Profile {
    name: Option<String>,
    biography: Option<String>,
    image_url: Option<String>,
    user_id: Option<u64>,
    /// Public id of SoundCloud's web app, needed by its API.
    client_id: Option<String>,
}

fn parse_profile(html: &str) -> Option<Profile> {
    let start = html.find(HYDRATION_START)? + HYDRATION_START.len();
    let rest = &html[start..];
    let json = &rest[..rest.find(";</script>")?];
    let entries: Vec<Value> = serde_json::from_str(json).ok()?;
    let data = |name: &str| {
        entries
            .iter()
            .find(|e| e.get("hydratable").and_then(Value::as_str) == Some(name))?
            .get("data")
    };
    let user = data("user")?;
    let text = |key: &str| user.get(key).and_then(Value::as_str);
    Some(Profile {
        name: text("username").and_then(clean_text),
        biography: text("description").and_then(clean_multiline),
        image_url: text("avatar_url").and_then(large_image),
        user_id: user.get("id").and_then(Value::as_u64),
        client_id: data("apiClient")
            .and_then(|c| c.get("id"))
            .and_then(Value::as_str)
            .filter(|id| {
                (1..=128).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .map(str::to_owned),
    })
}

fn profile_url(user: &str) -> String {
    format!("https://soundcloud.com/{user}")
}

/// The user of a profile URL (`/user` or its `/user/tracks` tab).
fn parse_user(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com"
    ) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        [user] | [user, "tracks"] if is_slug(user) && !RESERVED_ROOTS.contains(user) => {
            Some((*user).to_owned())
        }
        _ => None,
    }
}

/// Link to the profile of whoever uploaded `entry`.
fn uploader_link(entry: &YtInfo) -> Option<ArtistLink> {
    let name = entry.uploader.as_deref().and_then(clean_text)?;
    let url = Url::parse(entry.uploader_url.as_deref()?).ok()?;
    Some(ArtistLink {
        name,
        url: profile_url(&parse_user(&url)?),
    })
}

fn is_slug(s: &str) -> bool {
    (1..=255).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Classify a SoundCloud URL and return its canonical form (query dropped).
fn parse(url: &Url) -> Option<(UrlKind, String)> {
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if !segments.iter().all(|s| is_slug(s)) {
        return None;
    }
    match host.as_str() {
        // Short links redirect to a track or set; resolve to find out which.
        "on.soundcloud.com" if segments.len() == 1 => Some((
            UrlKind::Collection,
            format!("https://on.soundcloud.com/{}", segments[0]),
        )),
        "api.soundcloud.com" => match segments.as_slice() {
            ["tracks", id] if id.chars().all(|c| c.is_ascii_digit()) => Some((
                UrlKind::Track,
                format!("https://api.soundcloud.com/tracks/{id}"),
            )),
            _ => None,
        },
        "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com" => {
            let canonical = format!("https://soundcloud.com/{}", segments.join("/"));
            match segments.as_slice() {
                [user, ..] if RESERVED_ROOTS.contains(user) => None,
                [_, "sets", _] | [_, "sets", _, _] => Some((UrlKind::Collection, canonical)),
                [_, track] if !PROFILE_TABS.contains(track) => Some((UrlKind::Track, canonical)),
                // Private tracks: /user/track/s-secret
                [_, track, secret] if !PROFILE_TABS.contains(track) && secret.starts_with("s-") => {
                    Some((UrlKind::Track, canonical))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn track_url(entry: &YtInfo) -> Option<String> {
    [entry.webpage_url.as_deref(), entry.url.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|u| Url::parse(u).ok())
        .find_map(|u| match parse(&u) {
            Some((UrlKind::Track, canonical)) => Some(canonical),
            _ => None,
        })
}

/// yt-dlp fills SoundCloud's `track` with the upload title and `artist`
/// with the uploader. Those echoes are not structured metadata: clear them
/// so the shared title parsing and uploader fallback apply (and the fields
/// are marked as inferred). A distinct publisher-provided artist is kept.
fn demote_echoed_fields(info: &mut YtInfo) {
    if info.track.is_some() && info.track == info.title {
        info.track = None;
    }
    let uploader = info.uploader.clone();
    let echoes_uploader = |name: &str| Some(name) == uploader.as_deref();
    if info
        .artists
        .as_deref()
        .is_some_and(|names| names.len() == 1 && echoes_uploader(&names[0]))
    {
        info.artists = None;
    }
    if info.artist.as_deref().is_some_and(echoes_uploader) {
        info.artist = None;
    }
}

fn summarize(mut entry: YtInfo) -> Option<TrackSummary> {
    let url = track_url(&entry)?;
    demote_echoed_fields(&mut entry);
    let mut summary = summary_from_entry(KIND, &entry, url, entry.uploader.as_deref());
    summary.artist_links = uploader_link(&entry).into_iter().collect();
    // SoundCloud's own release date is not passed on by yt-dlp, so this is
    // usually the upload time.
    summary.year = summary.year.or_else(|| {
        let date = ReleaseDate::from_unix_timestamp(entry.timestamp?)?;
        Some(SummaryYear {
            year: date.year,
            basis: YearBasis::Upload,
        })
    });
    // The label is not a field either; labels upload their own releases
    // or name themselves in the tags ("Ninja Tune") or the description.
    summary.label_hints = entry
        .tags
        .take()
        .unwrap_or_default()
        .iter()
        .chain(entry.uploader.as_ref())
        .chain(entry.description.as_ref())
        .filter_map(|text| clean_text(text))
        .collect();
    Some(summary)
}

#[async_trait]
impl MusicProvider for SoundCloudProvider {
    fn kind(&self) -> ProviderKind {
        KIND
    }

    fn classify(&self, url: &Url) -> Option<UrlKind> {
        parse(url).map(|(kind, _)| kind)
    }

    fn is_artist_url(&self, url: &Url) -> bool {
        parse_user(url).is_some()
    }

    async fn artist(
        &self,
        url: &Url,
        max_tracks: usize,
        include_reposts: bool,
    ) -> Result<ArtistPage, ProviderError> {
        let user = parse_user(url).ok_or(ProviderError::UnsupportedUrl)?;
        let profile = self.profile(&user).await.unwrap_or_default();
        let name = profile.name.clone().unwrap_or_else(|| user.clone());
        let list = match self.api_tracks(&profile, max_tracks, include_reposts).await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(error = %e, include_reposts, "SoundCloud track list failed; listing with yt-dlp");
                self.ytdlp_tracks(&user, &name, max_tracks, include_reposts)
                    .await?
            }
        };
        Ok(ArtistPage {
            provider: KIND,
            url: profile_url(&user),
            name,
            biography: profile.biography,
            image_url: profile.image_url,
            tracks: list.tracks,
            truncated: list.truncated,
            reposts: Some(include_reposts),
            too_long: None,
        })
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        _detail: SearchDetail,
    ) -> Result<Vec<TrackSummary>, ProviderError> {
        let info = self
            .ytdlp
            .search_flat(&format!("scsearch{limit}:{query}"), limit)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        Ok(info
            .into_entries()
            .filter_map(summarize)
            .take(limit)
            .collect())
    }

    async fn resolve(&self, url: &Url, max_tracks: usize) -> Result<Resolved, ProviderError> {
        let (_, canonical) = parse(url).ok_or(ProviderError::UnsupportedUrl)?;
        let mut info = self
            .ytdlp
            .resolve_flat(&canonical, max_tracks)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        if info.is_playlist() {
            return Ok(Resolved::Collection(Collection {
                provider: KIND,
                url: canonical,
                title: info.title.as_deref().and_then(clean_text),
                tracks: info
                    .into_entries()
                    .filter_map(summarize)
                    .take(max_tracks)
                    .collect(),
                too_long: None,
            }));
        }
        let url = track_url(&info).unwrap_or(canonical);
        demote_echoed_fields(&mut info);
        Ok(Resolved::Track(summary_from_entry(
            KIND,
            &info,
            url,
            info.uploader.as_deref(),
        )))
    }

    async fn track_details(&self, url: &Url) -> Result<TrackDetails, ProviderError> {
        let Some((UrlKind::Track, canonical)) = parse(url) else {
            return Err(ProviderError::UnsupportedUrl);
        };
        let mut info = self
            .ytdlp
            .info(&canonical)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        let source_url = track_url(&info).unwrap_or(canonical);
        demote_echoed_fields(&mut info);
        let metadata = base_metadata(KIND, &info, source_url, info.uploader.as_deref())?;
        Ok(TrackDetails {
            metadata,
            formats: info.audio_formats(),
            artwork_needs_square_crop: false,
        })
    }

    async fn stream_source(&self, url: &Url) -> Result<StreamSource, ProviderError> {
        let Some((UrlKind::Track, canonical)) = parse(url) else {
            return Err(ProviderError::UnsupportedUrl);
        };
        let info = self
            .ytdlp
            .info(&canonical)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        pick_stream(KIND, &info)
    }

    async fn download(
        &self,
        details: &TrackDetails,
        format: &AudioFormat,
        work_dir: &Path,
        on_progress: &(dyn Fn(DownloadProgress) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<DownloadedMedia, ProviderError> {
        download_with_ytdlp(
            &self.ytdlp,
            KIND,
            &details.metadata.source_url,
            format,
            work_dir,
            on_progress,
            cancel,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_results_get_upload_year_and_label_hints() {
        let summary = summarize(YtInfo {
            id: Some("123".into()),
            title: Some("BICEP | GLUE // CLIP".into()),
            uploader: Some("BICEP".into()),
            webpage_url: Some("https://soundcloud.com/feelmybicep/glue".into()),
            timestamp: Some(1_501_753_117),
            tags: Some(vec!["Ninja Tune".into(), "Single".into()]),
            description: Some("New album out on Ninja Tune".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            summary.year,
            Some(SummaryYear {
                year: 2017,
                basis: YearBasis::Upload
            })
        );
        assert_eq!(summary.label, None);
        assert_eq!(
            summary.label_hints,
            [
                "Ninja Tune",
                "Single",
                "BICEP",
                "New album out on Ninja Tune"
            ]
        );
    }

    fn classify(s: &str) -> Option<(UrlKind, String)> {
        parse(&Url::parse(s).unwrap())
    }

    #[test]
    fn recognizes_tracks_and_sets() {
        assert_eq!(
            classify("https://soundcloud.com/bicep/glue?si=abc&utm_source=x"),
            Some((UrlKind::Track, "https://soundcloud.com/bicep/glue".into()))
        );
        assert_eq!(
            classify("https://m.soundcloud.com/bicep/glue/s-AbC123"),
            Some((
                UrlKind::Track,
                "https://soundcloud.com/bicep/glue/s-AbC123".into()
            ))
        );
        assert_eq!(
            classify("https://soundcloud.com/bicep/sets/isles").map(|(k, _)| k),
            Some(UrlKind::Collection)
        );
        assert_eq!(
            classify("https://on.soundcloud.com/AbCd").map(|(k, _)| k),
            Some(UrlKind::Collection)
        );
        assert_eq!(
            classify("https://api.soundcloud.com/tracks/123456").map(|(k, _)| k),
            Some(UrlKind::Track)
        );
    }

    #[test]
    fn echoed_fields_fall_back_to_title_parsing() {
        let mut info: YtInfo = serde_json::from_str(
            r#"{"id":"1","title":"Bicep - Glue (SCAR EDIT)","track":"Bicep - Glue (SCAR EDIT)",
                "artist":"SCAR","artists":["SCAR"],"uploader":"SCAR"}"#,
        )
        .unwrap();
        demote_echoed_fields(&mut info);
        let meta = base_metadata(KIND, &info, "u".into(), info.uploader.as_deref()).unwrap();
        assert_eq!(meta.title, "Glue (SCAR EDIT)");
        assert_eq!(meta.artists, vec!["Bicep"]);
    }

    #[test]
    fn distinct_publisher_artist_is_kept() {
        let mut info: YtInfo = serde_json::from_str(
            r#"{"id":"1","title":"Glue","track":"Glue","artists":["Bicep"],"uploader":"Ninja Tune"}"#,
        )
        .unwrap();
        demote_echoed_fields(&mut info);
        let meta = base_metadata(KIND, &info, "u".into(), info.uploader.as_deref()).unwrap();
        assert_eq!(meta.artists, vec!["Bicep"]);
        assert!(meta.inferred.is_empty());
    }

    #[test]
    fn recognizes_profile_pages() {
        let user = |s: &str| parse_user(&Url::parse(s).unwrap());
        assert_eq!(
            user("https://soundcloud.com/feelmybicep").as_deref(),
            Some("feelmybicep")
        );
        assert_eq!(
            user("https://m.soundcloud.com/feelmybicep/tracks?x=1").as_deref(),
            Some("feelmybicep")
        );
        assert_eq!(user("https://soundcloud.com/feelmybicep/glue"), None);
        assert_eq!(user("https://soundcloud.com/discover"), None);
        assert_eq!(user("https://soundcloud.com/%2e%2e"), None);
        assert_eq!(user("https://soundcloud.com.evil.example/bicep"), None);
    }

    #[test]
    fn search_results_link_the_uploader() {
        let summary = summarize(YtInfo {
            title: Some("Bicep - Glue (SCAR EDIT)".into()),
            uploader: Some("SCAR".into()),
            uploader_url: Some("https://soundcloud.com/scar-music".into()),
            webpage_url: Some("https://soundcloud.com/scar-music/glue-edit".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            summary.artist_links,
            [ArtistLink {
                name: "SCAR".into(),
                url: "https://soundcloud.com/scar-music".into()
            }]
        );
        let foreign = summarize(YtInfo {
            uploader: Some("SCAR".into()),
            uploader_url: Some("https://evil.example/scar".into()),
            webpage_url: Some("https://soundcloud.com/scar-music/glue-edit".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(foreign.artist_links.is_empty());
    }

    #[test]
    fn reads_profile_from_page_data() {
        let html = r#"<script>window.__sc_hydration = [{"hydratable":"anonymousId","data":"x"},
            {"hydratable":"apiClient","data":{"id":"QhVEWrrJKvW4","isExpiring":true}},
            {"hydratable":"user","data":{"id":92661,"username":"BICEP","full_name":"Bicep Duo",
            "description":"BOOKING\n\nworld","avatar_url":"https://i1.sndcdn.com/avatars-abc-large.jpg"}}];</script>"#;
        assert_eq!(
            parse_profile(html),
            Some(Profile {
                name: Some("BICEP".into()),
                biography: Some(
                    "BOOKING

world"
                        .into()
                ),
                image_url: Some("https://i1.sndcdn.com/avatars-abc-t500x500.jpg".into()),
                user_id: Some(92661),
                client_id: Some("QhVEWrrJKvW4".into()),
            })
        );
        assert_eq!(parse_profile("<html>no data</html>"), None);
        assert_eq!(
            parse_profile(r#"window.__sc_hydration = [{"hydratable":"user"}];</script>"#),
            None
        );
    }

    #[test]
    fn api_tracks_keep_artwork_duration_and_dates() {
        let track = serde_json::json!({
            "kind": "track", "id": 336067387, "title": "BICEP | GLUE // CLIP",
            "permalink_url": "https://soundcloud.com/feelmybicep/bicep-glue-clip",
            "artwork_url": null, "duration": 103430, "created_at": "2017-08-03T09:38:37Z",
            "publisher_metadata": {"artist": "Bicep"},
            "user": {"username": "BICEP", "permalink_url": "https://soundcloud.com/feelmybicep",
                     "avatar_url": "https://i1.sndcdn.com/avatars-x-large.jpg"}
        });
        let summary = api_track(&track).unwrap();
        assert_eq!(
            summary.url,
            "https://soundcloud.com/feelmybicep/bicep-glue-clip"
        );
        assert_eq!(summary.artists, ["Bicep"]);
        assert_eq!(summary.duration_secs, Some(103.43));
        assert_eq!(
            summary.thumbnail_url.as_deref(),
            Some("https://i1.sndcdn.com/avatars-x-t500x500.jpg")
        );
        assert_eq!(
            summary.year,
            Some(SummaryYear {
                year: 2017,
                basis: YearBasis::Upload
            })
        );
        assert_eq!(
            summary.artist_links[0].url,
            "https://soundcloud.com/feelmybicep"
        );

        let playlist = serde_json::json!({"kind": "playlist", "permalink_url": "https://soundcloud.com/a/sets/b"});
        assert!(api_track(&playlist).is_none());
    }

    #[test]
    fn stream_lists_posted_and_reposted_tracks_only() {
        let track = |user: &str, slug: &str| {
            serde_json::json!({
                "kind": "track", "id": 1, "title": slug,
                "permalink_url": format!("https://soundcloud.com/{user}/{slug}"),
                "user": {"username": user, "permalink_url": format!("https://soundcloud.com/{user}")}
            })
        };
        let post = serde_json::json!({"type": "track", "track": track("feelmybicep", "glue")});
        let repost = serde_json::json!({"type": "track-repost", "track": track("ra", "ra-1000")});
        let playlist = serde_json::json!({"type": "playlist-repost",
            "playlist": {"kind": "playlist", "permalink_url": "https://soundcloud.com/a/sets/b"}});

        let post = stream_item(&post).unwrap();
        assert!(!post.reposted);
        let repost = stream_item(&repost).unwrap();
        assert!(repost.reposted);
        // The reposted track links to whoever posted it.
        assert_eq!(repost.artist_links[0].url, "https://soundcloud.com/ra");
        assert!(stream_item(&playlist).is_none());
        assert!(stream_item(&serde_json::json!({"type": "track-repost"})).is_none());
    }

    #[test]
    fn tells_own_tracks_from_reposts_by_url() {
        assert!(posted_by(
            "https://soundcloud.com/feelmybicep/glue",
            "FeelMyBicep"
        ));
        assert!(!posted_by(
            "https://soundcloud.com/ra/ra-1000",
            "feelmybicep"
        ));
        assert!(!posted_by(
            "https://soundcloud.com/feelmybicep-2/x",
            "feelmybicep"
        ));
        assert!(!posted_by("https://api.soundcloud.com/tracks/1", "tracks"));
    }

    #[test]
    fn client_id_is_set_once() {
        let url =
            Url::parse("https://api-v2.soundcloud.com/users/1/tracks?offset=x%2Cy&client_id=old")
                .unwrap();
        let out = with_client_id(&url, "new");
        assert_eq!(
            out.query_pairs().filter(|(k, _)| k == "client_id").count(),
            1
        );
        assert!(out.as_str().ends_with("offset=x%2Cy&client_id=new"));
    }

    #[test]
    fn rejects_non_track_pages() {
        assert_eq!(classify("https://soundcloud.com/bicep"), None);
        assert_eq!(classify("https://soundcloud.com/bicep/likes"), None);
        assert_eq!(classify("https://soundcloud.com/discover/sets"), None);
        assert_eq!(classify("https://soundcloud.com.evil.example/a/b"), None);
        assert_eq!(classify("https://soundcloud.com/a/%2e%2e"), None);
    }
}
