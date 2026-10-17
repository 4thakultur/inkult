//! YouTube and YouTube Music.
//!
//! Search goes through YouTube Music's "songs" section first because its
//! results are official audio uploads ("art tracks") with structured
//! metadata, rather than music videos with intros, sound effects or
//! re-encoded audio. Regular YouTube search (via yt-dlp) is the fallback.
//!
//! Art tracks carry an auto-generated description ("Provided to YouTube by
//! <label> ... ℗ ... Released on: ...") which we parse for label, copyright,
//! release date and the full artist list.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::common::{
    base_metadata, classify_error, download_with_ytdlp, pick_stream, summary_from_entry,
};
use super::youtube_music::{VideoAbout, YouTubeWebClient, is_channel_id};
use super::{
    ArtistLink, ArtistPage, Collection, DownloadedMedia, MusicProvider, ProviderError, Resolved,
    SearchDetail, StreamSource, SummaryYear, TrackDetails, TrackSummary, UrlKind, YearBasis,
};
use crate::domain::DownloadProgress;
use crate::domain::metadata::{MetadataField, ReleaseDate, TrackMetadata};
use crate::domain::provider::ProviderKind;
use crate::domain::quality::AudioFormat;
use crate::metadata::{clean_artists, clean_multiline, clean_text};
use crate::tools::ytdlp::{YtDlp, YtInfo};

const KIND: ProviderKind = ProviderKind::YouTube;
/// Concurrent description lookups when search results need release info.
const RELEASE_LOOKUPS: usize = 6;

pub struct YouTubeProvider {
    ytdlp: Arc<YtDlp>,
    /// `None` if the HTTP client could not be built; search then uses yt-dlp
    /// and results carry no release info.
    web: Option<YouTubeWebClient>,
}

impl YouTubeProvider {
    pub fn new(ytdlp: Arc<YtDlp>) -> Self {
        let web = YouTubeWebClient::new()
            .inspect_err(|e| tracing::warn!(error = %e, "YouTube Music search unavailable"))
            .ok();
        Self { ytdlp, web }
    }

    async fn search_music(&self, query: &str, limit: usize) -> Option<Vec<TrackSummary>> {
        let songs = match self.web.as_ref()?.search_songs(query, limit).await {
            Ok(songs) => songs,
            Err(e) => {
                tracing::warn!(error = %e, "YouTube Music search failed; falling back to YouTube search");
                return None;
            }
        };
        let results = songs_to_summaries(songs);
        (!results.is_empty()).then_some(results)
    }

    /// An artist's YouTube Music page and the full list of their songs.
    /// `None` if the channel is not a YouTube Music artist (or the
    /// unofficial endpoint failed); the caller then lists channel uploads.
    async fn music_artist(&self, channel_id: &str, max_tracks: usize) -> Option<ArtistPage> {
        let web = self.web.as_ref()?;
        let info = web
            .artist(channel_id)
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "YouTube Music artist page failed"))
            .ok()?;
        let playlist = info.songs_playlist?;
        let list = web
            .playlist_songs(&playlist, max_tracks)
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "YouTube Music artist songs failed"))
            .ok()?;
        let tracks = songs_to_summaries(list.songs);
        Some(ArtistPage {
            provider: KIND,
            url: format!("https://music.youtube.com/channel/{channel_id}"),
            name: info
                .name
                .or_else(|| tracks.first()?.artists.first().cloned())
                .unwrap_or_else(|| channel_id.to_owned()),
            biography: info.biography,
            image_url: info.image_url,
            tracks,
            truncated: list.truncated,
            reposts: None,
            too_long: None,
        })
    }

    /// A regular channel: its uploads, newest first, and its description.
    async fn channel_uploads(
        &self,
        channel_id: &str,
        max_tracks: usize,
    ) -> Result<ArtistPage, ProviderError> {
        let url = format!("https://www.youtube.com/channel/{channel_id}");
        let info = self
            .ytdlp
            .resolve_flat(&format!("{url}/videos"), max_tracks)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        let name = channel_artist(&info)
            .and_then(|n| clean_text(&n))
            .unwrap_or_else(|| channel_id.to_owned());
        let tracks: Vec<TrackSummary> = info
            .entries()
            .filter_map(|entry| {
                // Flat channel listings leave out the channel on each entry.
                let mut summary = summarize(entry)?;
                if summary.artists.is_empty() {
                    summary.artists = vec![name.clone()];
                }
                Some(summary)
            })
            .take(max_tracks)
            .collect();
        Ok(ArtistPage {
            provider: KIND,
            url,
            biography: info.description.as_deref().and_then(clean_multiline),
            image_url: info.best_thumbnail(),
            truncated: info.entries().count() >= max_tracks,
            name,
            tracks,
            reposts: None,
            too_long: None,
        })
    }

    /// Look up each result's description for its label and release date.
    /// Results whose lookup fails are left as they are.
    async fn add_release_info(&self, results: &mut [TrackSummary]) {
        let Some(web) = &self.web else {
            return;
        };
        let slots = Arc::new(tokio::sync::Semaphore::new(RELEASE_LOOKUPS));
        let mut tasks = tokio::task::JoinSet::new();
        for (index, summary) in results.iter().enumerate() {
            let Some(YouTubeUrl::Video(id)) = Url::parse(&summary.url)
                .ok()
                .as_ref()
                .and_then(YouTubeUrl::parse)
            else {
                continue;
            };
            let (web, slots) = (web.clone(), Arc::clone(&slots));
            tasks.spawn(async move {
                let _slot = slots.acquire_owned().await.ok()?;
                Some((index, web.video_about(&id).await))
            });
        }
        let mut failures = 0usize;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Some((index, Ok(about)))) => apply_video_about(&mut results[index], about),
                Ok(Some((_, Err(e)))) => {
                    failures += 1;
                    tracing::debug!(error = %e, "video description lookup failed");
                }
                Ok(None) => {}
                Err(e) => tracing::error!(error = %e, "description lookup task panicked"),
            }
        }
        if failures > 0 {
            tracing::warn!(
                failures,
                total = results.len(),
                "could not look up release info for some YouTube results"
            );
        }
    }
}

fn songs_to_summaries(songs: Vec<super::youtube_music::Song>) -> Vec<TrackSummary> {
    songs
        .into_iter()
        .filter(|song| is_video_id(&song.video_id))
        .map(|song| TrackSummary {
            provider: KIND,
            url: video_url(&song.video_id),
            title: song.title,
            artists: song.artists,
            album: song.album,
            duration_secs: song.duration_secs,
            thumbnail_url: song.thumbnail_url,
            track_number: None,
            year: None,
            label: None,
            label_hints: Vec::new(),
            artist_links: song
                .artist_channels
                .into_iter()
                .map(|(name, id)| ArtistLink {
                    name,
                    url: channel_url(&id),
                })
                .collect(),
            reposted: false,
        })
        .collect()
}

/// Release year and label hints from a video description: art tracks state
/// both; other videos only have their upload date.
fn apply_video_about(summary: &mut TrackSummary, about: VideoAbout) {
    let art = about
        .description
        .as_deref()
        .and_then(parse_art_track_description);
    let released = art.as_ref().and_then(|art| art.release_date);
    let uploaded = about.publish_date.as_deref().and_then(ReleaseDate::parse);
    if summary.year.is_none_or(|y| y.basis == YearBasis::Upload) {
        summary.year = match (released, uploaded) {
            (Some(date), _) => Some(SummaryYear {
                year: date.year,
                basis: YearBasis::Release,
            }),
            (None, Some(date)) => Some(SummaryYear {
                year: date.year,
                basis: YearBasis::Upload,
            }),
            (None, None) => summary.year,
        };
    }
    match art {
        Some(art) => {
            // "Provided to YouTube by" often names the distributor; the
            // "℗ 2017 Ninja Tune" line names the label that owns it.
            summary.label_hints = art.label.iter().chain(&art.copyright).cloned().collect();
            summary.label = art
                .copyright
                .as_deref()
                .and_then(phonogram_owner)
                .or(art.label);
            if summary.album.is_none() {
                summary.album = art.album;
            }
        }
        None => {
            summary.label_hints = about
                .description
                .as_deref()
                .and_then(clean_text)
                .into_iter()
                .collect();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum YouTubeUrl {
    Video(String),
    Playlist(String),
    /// YouTube Music album/release page (`/browse/MPREb_...`).
    Album(String),
    /// Artist or uploader channel (`/channel/UC...`).
    Channel(String),
}

impl YouTubeUrl {
    fn parse(url: &Url) -> Option<Self> {
        let host = url.host_str()?.to_ascii_lowercase();
        let mut segments = url
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty());
        let first = segments.next();
        let second = segments.next();
        let query = |key: &str| {
            url.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };

        match host.as_str() {
            "youtu.be" => first
                .filter(|id| is_video_id(id))
                .map(|id| Self::Video(id.to_owned())),
            "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
                match (first, second) {
                    (Some("watch"), _) => query("v").filter(|id| is_video_id(id)).map(Self::Video),
                    (Some("shorts" | "live" | "embed"), Some(id)) if is_video_id(id) => {
                        Some(Self::Video(id.to_owned()))
                    }
                    (Some("playlist"), _) => query("list")
                        .filter(|id| is_list_id(id))
                        .map(Self::Playlist),
                    (Some("browse"), Some(id)) if host == "music.youtube.com" && is_list_id(id) => {
                        Some(Self::Album(id.to_owned()))
                    }
                    (Some("channel"), Some(id)) if is_channel_id(id) => {
                        Some(Self::Channel(id.to_owned()))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn canonical(&self) -> String {
        match self {
            Self::Video(id) => video_url(id),
            Self::Playlist(id) => format!("https://www.youtube.com/playlist?list={id}"),
            Self::Album(id) => format!("https://music.youtube.com/browse/{id}"),
            Self::Channel(id) => channel_url(id),
        }
    }
}

/// Artist links point at YouTube Music, where artist channels have their
/// biography and song list; other channels are served from there too.
fn channel_url(id: &str) -> String {
    format!("https://music.youtube.com/channel/{id}")
}

fn video_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_list_id(id: &str) -> bool {
    (2..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Auto-generated channels are named "<Artist> - Topic".
fn channel_artist(info: &YtInfo) -> Option<String> {
    let channel = info.channel.as_deref().or(info.uploader.as_deref())?;
    Some(
        channel
            .strip_suffix(" - Topic")
            .unwrap_or(channel)
            .to_owned(),
    )
}

fn summarize(entry: &YtInfo) -> Option<TrackSummary> {
    // Search results can include channels and playlists; keep videos only.
    let id = entry.id.as_deref().filter(|id| is_video_id(id))?;
    if entry
        .url
        .as_deref()
        .and_then(|u| Url::parse(u).ok())
        .is_some_and(|u| !matches!(YouTubeUrl::parse(&u), Some(YouTubeUrl::Video(_))))
    {
        return None;
    }
    let artist = channel_artist(entry);
    let mut summary = summary_from_entry(KIND, entry, video_url(id), artist.as_deref());
    if let (Some(name), Some(channel)) = (
        artist.as_deref().and_then(clean_text),
        entry.channel_id.as_deref().filter(|id| is_channel_id(id)),
    ) {
        summary.artist_links = vec![ArtistLink {
            name,
            url: channel_url(channel),
        }];
    }
    Some(summary)
}

/// Data parsed from an auto-generated art-track description.
#[derive(Debug, Default, PartialEq)]
struct ArtTrackInfo {
    label: Option<String>,
    title: Option<String>,
    artists: Vec<String>,
    album: Option<String>,
    copyright: Option<String>,
    release_date: Option<ReleaseDate>,
}

fn parse_art_track_description(description: &str) -> Option<ArtTrackInfo> {
    let paragraphs: Vec<&str> = description
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let label = paragraphs.first()?.strip_prefix("Provided to YouTube by")?;

    let mut info = ArtTrackInfo {
        label: clean_text(label),
        ..Default::default()
    };
    if let Some(credits) = paragraphs.get(1).filter(|p| p.contains(" · ")) {
        let mut parts = credits.split(" · ");
        info.title = parts.next().and_then(clean_text);
        info.artists = clean_artists(parts);
    }
    if let Some(album) = paragraphs
        .get(2)
        .filter(|p| !p.starts_with('℗') && !p.starts_with("Released on:") && !p.contains('\n'))
    {
        info.album = clean_text(album);
    }
    for paragraph in &paragraphs {
        if paragraph.starts_with('℗') {
            info.copyright = clean_text(paragraph);
        } else if let Some(date) = paragraph.strip_prefix("Released on:") {
            info.release_date = ReleaseDate::parse(date.trim());
        }
    }
    Some(info)
}

/// The rights owner of a "℗ 2017 Ninja Tune" line: the label, unlike the
/// "Provided to YouTube by" name, which is often a distributor.
fn phonogram_owner(copyright: &str) -> Option<String> {
    let rest = copyright.strip_prefix('℗')?.trim_start();
    let year_len = rest.chars().take_while(char::is_ascii_digit).count();
    let owner = if year_len == 4 { &rest[4..] } else { rest };
    // "Warp Records under exclusive licence from ..." names the label first.
    // ASCII lowercasing keeps byte offsets, so `end` is valid in `owner`.
    let owner = match owner.to_ascii_lowercase().find(" under exclusive licen") {
        Some(end) => &owner[..end],
        None => owner,
    };
    clean_text(owner)
}

/// Fill gaps (and replace inferred guesses) with art-track description data.
fn apply_art_track(meta: &mut TrackMetadata, art: ArtTrackInfo) {
    let was_inferred = |meta: &TrackMetadata, field| meta.inferred.contains(&field);

    if let Some(title) = art.title
        && was_inferred(meta, MetadataField::Title)
    {
        meta.title = title;
        meta.inferred.retain(|f| *f != MetadataField::Title);
    }
    if !art.artists.is_empty()
        && (was_inferred(meta, MetadataField::Artists) || art.artists.len() > meta.artists.len())
    {
        meta.artists = art.artists;
        meta.inferred.retain(|f| *f != MetadataField::Artists);
    }
    if meta.album.is_none() {
        meta.album = art.album;
    }
    if let Some(date) = art.release_date
        && (meta.release_date.is_none() || was_inferred(meta, MetadataField::ReleaseDate))
    {
        meta.release_date = Some(date);
        meta.inferred.retain(|f| *f != MetadataField::ReleaseDate);
    }
    meta.label = meta.label.take().or(art.label);
    meta.copyright = meta.copyright.take().or(art.copyright);
}

#[async_trait]
impl MusicProvider for YouTubeProvider {
    fn kind(&self) -> ProviderKind {
        KIND
    }

    fn classify(&self, url: &Url) -> Option<UrlKind> {
        YouTubeUrl::parse(url).and_then(|parsed| match parsed {
            YouTubeUrl::Video(_) => Some(UrlKind::Track),
            YouTubeUrl::Playlist(_) | YouTubeUrl::Album(_) => Some(UrlKind::Collection),
            YouTubeUrl::Channel(_) => None,
        })
    }

    fn is_artist_url(&self, url: &Url) -> bool {
        matches!(YouTubeUrl::parse(url), Some(YouTubeUrl::Channel(_)))
    }

    async fn artist(
        &self,
        url: &Url,
        max_tracks: usize,
        _include_reposts: bool,
    ) -> Result<ArtistPage, ProviderError> {
        let Some(YouTubeUrl::Channel(id)) = YouTubeUrl::parse(url) else {
            return Err(ProviderError::UnsupportedUrl);
        };
        match self.music_artist(&id, max_tracks).await {
            Some(page) => Ok(page),
            None => self.channel_uploads(&id, max_tracks).await,
        }
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        detail: SearchDetail,
    ) -> Result<Vec<TrackSummary>, ProviderError> {
        let mut results = match self.search_music(query, limit).await {
            Some(results) => results,
            None => self
                .ytdlp
                .search_flat(&format!("ytsearch{limit}:{query}"), limit)
                .await
                .map_err(|e| classify_error(KIND, e))?
                .entries()
                .filter_map(summarize)
                .take(limit)
                .collect(),
        };
        if detail == SearchDetail::Release {
            self.add_release_info(&mut results).await;
        }
        Ok(results)
    }

    async fn resolve(&self, url: &Url, max_tracks: usize) -> Result<Resolved, ProviderError> {
        let parsed = YouTubeUrl::parse(url).ok_or(ProviderError::UnsupportedUrl)?;
        let canonical = parsed.canonical();
        if let YouTubeUrl::Video(_) = parsed {
            let info = self
                .ytdlp
                .resolve_flat(&canonical, 1)
                .await
                .map_err(|e| classify_error(KIND, e))?;
            let artist = channel_artist(&info);
            return Ok(Resolved::Track(summary_from_entry(
                KIND,
                &info,
                canonical,
                artist.as_deref(),
            )));
        }
        let info = self
            .ytdlp
            .resolve_flat(&canonical, max_tracks)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        let is_album = matches!(parsed, YouTubeUrl::Album(_));
        let tracks = info
            .entries()
            .enumerate()
            .filter_map(|(index, entry)| {
                let mut summary = summarize(entry)?;
                // Album pages list tracks in release order; per-video
                // metadata carries no track number at all.
                if is_album {
                    summary.track_number = u32::try_from(index + 1).ok();
                }
                Some(summary)
            })
            .take(max_tracks)
            .collect();
        Ok(Resolved::Collection(Collection {
            provider: KIND,
            url: canonical,
            title: info
                .title
                .as_deref()
                .map(|t| t.strip_prefix("Album - ").unwrap_or(t))
                .and_then(clean_text),
            tracks,
            too_long: None,
        }))
    }

    async fn track_details(&self, url: &Url) -> Result<TrackDetails, ProviderError> {
        let Some(YouTubeUrl::Video(id)) = YouTubeUrl::parse(url) else {
            return Err(ProviderError::UnsupportedUrl);
        };
        let canonical = video_url(&id);
        let info = self
            .ytdlp
            .info(&canonical)
            .await
            .map_err(|e| classify_error(KIND, e))?;
        let artist = channel_artist(&info);
        let mut metadata = base_metadata(KIND, &info, canonical, artist.as_deref())?;

        let art_track = info
            .description
            .as_deref()
            .and_then(parse_art_track_description);
        let is_art_track = art_track.is_some();
        if let Some(art) = art_track {
            apply_art_track(&mut metadata, art);
        }
        // Long video descriptions (links, tour dates) are noise in a music
        // library; art-track descriptions are fully captured in tags already.
        if is_art_track {
            metadata.description = None;
        }

        Ok(TrackDetails {
            metadata,
            formats: info.audio_formats(),
            artwork_needs_square_crop: is_art_track,
        })
    }

    async fn stream_source(&self, url: &Url) -> Result<StreamSource, ProviderError> {
        let Some(YouTubeUrl::Video(id)) = YouTubeUrl::parse(url) else {
            return Err(ProviderError::UnsupportedUrl);
        };
        let info = self
            .ytdlp
            .info(&video_url(&id))
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
    fn reads_label_from_phonogram_line() {
        assert_eq!(
            phonogram_owner("℗ 1999 Warp Records").as_deref(),
            Some("Warp Records")
        );
        assert_eq!(
            phonogram_owner("℗ Ninja Tune").as_deref(),
            Some("Ninja Tune")
        );
        assert_eq!(phonogram_owner("℗ 2017"), None);
        assert_eq!(
            phonogram_owner("℗ 1995 Warp Records Under Exclusive Licence From Evolution")
                .as_deref(),
            Some("Warp Records")
        );
        assert_eq!(phonogram_owner("© 2017 Someone"), None);
    }

    #[test]
    fn search_results_get_label_and_release_year() {
        let mut summary = summarize(&YtInfo {
            id: Some("lXc8EHmFXOM".into()),
            title: Some("Glue".into()),
            ..Default::default()
        })
        .unwrap();
        apply_video_about(
            &mut summary,
            VideoAbout {
                description: Some(
                    "Provided to YouTube by IIP-DDS\n\nGlue · Bicep\n\nBicep\n\n℗ 2017 Ninja Tune\n\nReleased on: 2017-09-01".into(),
                ),
                publish_date: Some("2017-08-31".into()),
            },
        );
        assert_eq!(summary.label.as_deref(), Some("Ninja Tune"));
        assert_eq!(summary.label_hints, ["IIP-DDS", "℗ 2017 Ninja Tune"]);
        assert_eq!(
            summary.year,
            Some(SummaryYear {
                year: 2017,
                basis: YearBasis::Release
            })
        );

        let mut video = summary.clone();
        video.year = None;
        video.label = None;
        apply_video_about(
            &mut video,
            VideoAbout {
                description: Some("Official video. Out now on Warp".into()),
                publish_date: Some("2019-03-01".into()),
            },
        );
        assert_eq!(video.label, None);
        assert_eq!(video.label_hints, ["Official video. Out now on Warp"]);
        assert_eq!(
            video.year,
            Some(SummaryYear {
                year: 2019,
                basis: YearBasis::Upload
            })
        );
    }

    fn parse(s: &str) -> Option<YouTubeUrl> {
        YouTubeUrl::parse(&Url::parse(s).unwrap())
    }

    #[test]
    fn recognizes_video_urls() {
        let video = Some(YouTubeUrl::Video("dQw4w9WgXcQ".into()));
        assert_eq!(parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), video);
        assert_eq!(
            parse("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM"),
            video
        );
        assert_eq!(parse("https://youtu.be/dQw4w9WgXcQ?t=10"), video);
        assert_eq!(parse("https://m.youtube.com/shorts/dQw4w9WgXcQ"), video);
    }

    #[test]
    fn recognizes_collections() {
        assert_eq!(
            parse("https://www.youtube.com/playlist?list=PLabc_123-x"),
            Some(YouTubeUrl::Playlist("PLabc_123-x".into()))
        );
        assert_eq!(
            parse("https://music.youtube.com/browse/MPREb_abc123"),
            Some(YouTubeUrl::Album("MPREb_abc123".into()))
        );
    }

    #[test]
    fn rejects_foreign_or_malformed_urls() {
        assert_eq!(parse("https://evil.example/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(
            parse("https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ"),
            None
        );
        assert_eq!(parse("https://www.youtube.com/watch?v=../../etc"), None);
        assert_eq!(parse("https://www.youtube.com/@channel"), None);
        assert_eq!(parse("https://www.youtube.com/channel/UCshort"), None);
        assert_eq!(parse("https://www.youtube.com/browse/MPREb_abc"), None);
    }

    #[test]
    fn recognizes_channels_as_artist_pages() {
        let channel = Some(YouTubeUrl::Channel("UCgapGYYYntw8Wg57E1DI7NA".into()));
        assert_eq!(
            parse("https://music.youtube.com/channel/UCgapGYYYntw8Wg57E1DI7NA"),
            channel
        );
        assert_eq!(
            parse("https://www.youtube.com/channel/UCgapGYYYntw8Wg57E1DI7NA/videos"),
            channel
        );
    }

    #[test]
    fn video_results_link_their_channel() {
        let summary = summarize(&YtInfo {
            id: Some("A7ZxRs45tTg".into()),
            title: Some("BICEP | GLUE (Official Video)".into()),
            channel: Some("BICEP".into()),
            channel_id: Some("UCreSupkPVEJoDjoIV-9GG5w".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            summary.artist_links,
            [ArtistLink {
                name: "BICEP".into(),
                url: "https://music.youtube.com/channel/UCreSupkPVEJoDjoIV-9GG5w".into()
            }]
        );

        let no_channel = summarize(&YtInfo {
            id: Some("A7ZxRs45tTg".into()),
            channel: Some("BICEP".into()),
            channel_id: Some("not-a-channel".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(no_channel.artist_links.is_empty());
    }

    const ART_TRACK: &str = "Provided to YouTube by Ninja Tune\n\nGlue · Bicep · Andy Ferguson\n\nBicep\n\n℗ 2017 Ninja Tune\n\nReleased on: 2017-09-01\n\nProducer: Bicep\n\nAuto-generated by YouTube.";

    #[test]
    fn parses_art_track_description() {
        let info = parse_art_track_description(ART_TRACK).unwrap();
        assert_eq!(info.label.as_deref(), Some("Ninja Tune"));
        assert_eq!(info.title.as_deref(), Some("Glue"));
        assert_eq!(info.artists, vec!["Bicep", "Andy Ferguson"]);
        assert_eq!(info.album.as_deref(), Some("Bicep"));
        assert_eq!(info.copyright.as_deref(), Some("℗ 2017 Ninja Tune"));
        assert_eq!(info.release_date.unwrap().to_string(), "2017-09-01");
        assert!(parse_art_track_description("Check out my channel!").is_none());
    }

    #[test]
    fn art_track_data_fills_gaps_without_overriding_structured_fields() {
        let json = r#"{"id":"dQw4w9WgXcQ","title":"Glue","track":"Glue","artists":["Bicep"],"album":"Bicep (Deluxe)","upload_date":"20170902"}"#;
        let info: YtInfo = serde_json::from_str(json).unwrap();
        let mut meta = base_metadata(KIND, &info, "u".into(), None).unwrap();
        apply_art_track(&mut meta, parse_art_track_description(ART_TRACK).unwrap());

        assert_eq!(meta.album.as_deref(), Some("Bicep (Deluxe)"));
        assert_eq!(meta.artists, vec!["Bicep", "Andy Ferguson"]);
        assert_eq!(meta.release_date.unwrap().to_string(), "2017-09-01");
        assert_eq!(meta.label.as_deref(), Some("Ninja Tune"));
        assert!(meta.inferred.is_empty(), "{:?}", meta.inferred);
    }

    #[test]
    fn strips_topic_suffix_from_channel() {
        let info: YtInfo = serde_json::from_str(r#"{"channel":"Bicep - Topic"}"#).unwrap();
        assert_eq!(channel_artist(&info).as_deref(), Some("Bicep"));
    }
}
