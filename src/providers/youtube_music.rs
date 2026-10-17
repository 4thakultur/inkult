//! YouTube Music song search and video descriptions via the web clients'
//! internal endpoints.
//!
//! yt-dlp's YouTube Music search only returns titles and ids in flat mode,
//! and full extraction per result is far too slow for interactive search.
//! The web client endpoint returns artist, album, duration and square cover
//! art in one request. This is an unofficial interface: any failure here is
//! non-fatal and the caller falls back to regular YouTube search via yt-dlp.
//!
//! Release year and label are not part of a search listing. The `player`
//! endpoint returns a video's description (which for art tracks names the
//! label and release date) in a few kilobytes, so search filters on them
//! cost one small request per result instead of a yt-dlp run each.
//!
//! Artist pages come from the `browse` endpoint too. Their "Top songs" shelf
//! links to an auto-generated playlist of every song YouTube Music credits
//! to the artist, which is paged 100 songs at a time.

use std::time::Duration;

use serde_json::{Value, json};

use super::common::read_body_capped;
use crate::metadata::{clean_artists, clean_multiline, clean_text};

const ENDPOINT: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
const BROWSE_ENDPOINT: &str = "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false";
/// The YouTube Music client's `player` response leaves out the description;
/// the regular web client's does not.
const PLAYER_ENDPOINT: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const PLAYER_CLIENT_NAME: &str = "WEB";
const PLAYER_CLIENT_VERSION: &str = "2.20250101.01.00";
/// Web client identity; older versions keep being accepted for a long time.
const CLIENT_NAME: &str = "WEB_REMIX";
const CLIENT_VERSION: &str = "1.20250101.01.00";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
/// Search filter selecting the "Songs" section (official audio tracks).
const SONGS_FILTER: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PLAYER_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Cover size requested from the image CDN (its native maximum for covers).
const COVER_SIZE: &str = "=w544-h544-l90-rj";
/// Bounds the requests made for one artist's song list.
const MAX_SONG_PAGES: usize = 50;

#[derive(Debug, Clone, PartialEq)]
pub struct Song {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_secs: Option<f64>,
    pub thumbnail_url: Option<String>,
    /// Credited artists that have a YouTube Music page: (name, channel id).
    pub artist_channels: Vec<(String, String)>,
}

/// What an artist's YouTube Music page says about them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArtistInfo {
    pub name: Option<String>,
    pub biography: Option<String>,
    pub image_url: Option<String>,
    /// Browse id (`VL...`) of the playlist of all the artist's songs. Absent
    /// for channels that are not artists (podcasts, uploaders).
    pub songs_playlist: Option<String>,
}

/// Songs of a playlist, up to a limit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SongList {
    pub songs: Vec<Song>,
    /// The limit cut the list short.
    pub truncated: bool,
}

/// Description and publish date of one video.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoAbout {
    pub description: Option<String>,
    /// When the video went online, `YYYY-MM-DD`.
    pub publish_date: Option<String>,
}

/// Cheap to clone: the HTTP client is shared.
#[derive(Clone)]
pub struct YouTubeWebClient {
    client: reqwest::Client,
}

impl YouTubeWebClient {
    pub fn new() -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client })
    }

    fn context() -> Value {
        json!({ "client": {
            "clientName": CLIENT_NAME, "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US"
        }})
    }

    async fn browse(&self, body: Value) -> Result<Value, String> {
        let request = self
            .client
            .post(BROWSE_ENDPOINT)
            .header("Origin", "https://music.youtube.com")
            .json(&body);
        fetch_json(request, MAX_RESPONSE_BYTES).await
    }

    /// `channel_id` must already be validated with [`is_channel_id`].
    pub async fn artist(&self, channel_id: &str) -> Result<ArtistInfo, String> {
        let json = self
            .browse(json!({ "context": Self::context(), "browseId": channel_id }))
            .await?;
        Ok(parse_artist(&json))
    }

    /// Every song of a playlist browse id (`VL...`), at most `limit`.
    pub async fn playlist_songs(&self, browse_id: &str, limit: usize) -> Result<SongList, String> {
        let mut json = self
            .browse(json!({ "context": Self::context(), "browseId": browse_id }))
            .await?;
        let mut songs = Vec::new();
        for _ in 0..MAX_SONG_PAGES {
            songs.extend(parse_songs(&json));
            let next = continuation_token(&json);
            if songs.len() >= limit || next.is_none() {
                let truncated = songs.len() > limit || next.is_some();
                songs.truncate(limit);
                return Ok(SongList { songs, truncated });
            }
            json = self
                .browse(json!({ "context": Self::context(), "continuation": next }))
                .await?;
        }
        songs.truncate(limit);
        Ok(SongList {
            songs,
            truncated: true,
        })
    }

    pub async fn search_songs(&self, query: &str, limit: usize) -> Result<Vec<Song>, String> {
        let body = json!({
            "context": Self::context(),
            "query": query,
            "params": SONGS_FILTER,
        });
        let request = self
            .client
            .post(ENDPOINT)
            .header("Origin", "https://music.youtube.com")
            .json(&body);
        let json = fetch_json(request, MAX_RESPONSE_BYTES).await?;
        Ok(parse_songs(&json).into_iter().take(limit).collect())
    }

    /// `video_id` must already be validated (11 URL-safe characters).
    pub async fn video_about(&self, video_id: &str) -> Result<VideoAbout, String> {
        let body = json!({
            "context": { "client": {
                "clientName": PLAYER_CLIENT_NAME, "clientVersion": PLAYER_CLIENT_VERSION,
                "hl": "en", "gl": "US"
            }},
            "videoId": video_id,
        });
        let request = self.client.post(PLAYER_ENDPOINT).json(&body);
        let json = fetch_json(request, MAX_PLAYER_RESPONSE_BYTES).await?;
        Ok(parse_video_about(&json))
    }
}

async fn fetch_json(request: reqwest::RequestBuilder, max_bytes: usize) -> Result<Value, String> {
    let response = request.send().await.map_err(|e| e.to_string())?;
    let bytes = read_body_capped(response, max_bytes).await?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// YouTube channel ids: "UC" and 22 URL-safe base64 characters.
pub fn is_channel_id(id: &str) -> bool {
    id.len() == 24
        && id.starts_with("UC")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Text of a `runs` array. Links are shortened for display ("https://en.
/// wikipedia.org/wiki/Bicep_(..."); their full target is used instead.
fn runs_text(runs: &Value) -> String {
    runs.as_array()
        .into_iter()
        .flatten()
        .map(|run| {
            run.pointer("/navigationEndpoint/urlEndpoint/url")
                .and_then(Value::as_str)
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                .unwrap_or_else(|| run_text(run))
        })
        .collect()
}

/// The first object stored under `key` anywhere in `value`.
fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|child| find_key(child, key))),
        Value::Array(items) => items.iter().find_map(|child| find_key(child, key)),
        _ => None,
    }
}

fn largest_image(thumbnails: Option<&Value>) -> Option<String> {
    thumbnails?
        .as_array()?
        .iter()
        .max_by_key(|t| t.get("width").and_then(Value::as_u64).unwrap_or(0))?
        .get("url")?
        .as_str()
        .filter(|url| url.starts_with("https://"))
        .map(str::to_owned)
}

fn parse_artist(json: &Value) -> ArtistInfo {
    // Artists have an "immersive" header, other channels a "visual" one.
    let header = json
        .get("header")
        .and_then(Value::as_object)
        .and_then(|h| h.values().next());
    let name = header
        .and_then(|h| h.pointer("/title/runs"))
        .map(runs_text)
        .and_then(|t| clean_text(&t));
    let biography = header
        .and_then(|h| h.pointer("/description/runs"))
        .or_else(|| {
            find_key(json, "musicDescriptionShelfRenderer")
                .and_then(|shelf| shelf.pointer("/description/runs"))
        })
        .map(runs_text)
        .and_then(|t| clean_multiline(&t));
    // The microformat image is the square profile picture; the header's is
    // a wide banner.
    let image_url =
        largest_image(json.pointer("/microformat/microformatDataRenderer/thumbnail/thumbnails"))
            .or_else(|| {
                largest_image(header.and_then(|h| {
                    h.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
                }))
            });
    let songs_playlist = find_key(json, "musicShelfRenderer").and_then(|shelf| {
        [
            "/title/runs/0/navigationEndpoint/browseEndpoint/browseId",
            "/bottomEndpoint/browseEndpoint/browseId",
        ]
        .iter()
        .find_map(|pointer| shelf.pointer(pointer).and_then(Value::as_str))
        .filter(|id| id.starts_with("VL") && is_browse_id(id))
        .map(str::to_owned)
    });
    ArtistInfo {
        name,
        biography,
        image_url,
        songs_playlist,
    }
}

fn is_browse_id(id: &str) -> bool {
    (2..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn continuation_token(json: &Value) -> Option<String> {
    find_key(json, "continuationItemRenderer")?
        .pointer("/continuationEndpoint/continuationCommand/token")?
        .as_str()
        .filter(|token| !token.is_empty() && token.len() <= 4096)
        .map(str::to_owned)
}

fn parse_video_about(json: &Value) -> VideoAbout {
    let text = |pointer: &str| json.pointer(pointer).and_then(Value::as_str);
    VideoAbout {
        description: text("/videoDetails/shortDescription")
            .filter(|d| !d.trim().is_empty())
            .map(str::to_owned),
        // ISO 8601 with time and offset, e.g. "2017-08-31T04:32:11-07:00".
        publish_date: text("/microformat/playerMicroformatRenderer/publishDate")
            .and_then(|d| d.get(..10))
            .map(str::to_owned),
    }
}

/// Collect every `musicResponsiveListItemRenderer` in document order.
fn list_items<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if let Some(item) = map.get("musicResponsiveListItemRenderer") {
                out.push(item);
            }
            for (key, child) in map {
                if key != "musicResponsiveListItemRenderer" {
                    list_items(child, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|child| list_items(child, out)),
        _ => {}
    }
}

fn parse_songs(json: &Value) -> Vec<Song> {
    let mut items = Vec::new();
    list_items(json, &mut items);
    items.into_iter().filter_map(parse_item).collect()
}

/// Runs of every column after the title: flex columns hold artists and
/// album, and in playlists a fixed column holds the duration.
fn detail_runs(item: &Value) -> Vec<&Value> {
    let flex = item
        .get("flexColumns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .skip(1)
        .filter_map(|c| c.pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs"));
    let fixed = item
        .get("fixedColumns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.pointer("/musicResponsiveListItemFixedColumnRenderer/text/runs"));
    flex.chain(fixed)
        .filter_map(Value::as_array)
        .flatten()
        .collect()
}

fn column_runs(item: &Value, index: usize) -> &[Value] {
    item.pointer(&format!(
        "/flexColumns/{index}/musicResponsiveListItemFlexColumnRenderer/text/runs"
    ))
    .and_then(Value::as_array)
    .map(Vec::as_slice)
    .unwrap_or_default()
}

fn page_type(run: &Value) -> Option<&str> {
    run.pointer(
        "/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/\
         browseEndpointContextMusicConfig/pageType",
    )
    .and_then(Value::as_str)
}

fn run_text(run: &Value) -> &str {
    run.get("text").and_then(Value::as_str).unwrap_or_default()
}

fn parse_duration(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    parts
        .iter()
        .try_fold(0u64, |acc, p| Some(acc * 60 + p.parse::<u64>().ok()?))
        .map(|secs| secs as f64)
}

fn parse_item(item: &Value) -> Option<Song> {
    let video_id = item
        .pointer("/playlistItemData/videoId")
        .or_else(|| {
            column_runs(item, 0)
                .first()?
                .pointer("/navigationEndpoint/watchEndpoint/videoId")
        })
        .and_then(Value::as_str)?
        .to_owned();
    let title = clean_text(
        &column_runs(item, 0)
            .iter()
            .map(run_text)
            .collect::<String>(),
    )?;

    // Second column: "Artist[ & Artist] • Album • 4:30", groups split by " • ".
    let details = column_runs(item, 1);
    let groups: Vec<Vec<&Value>> = details
        .split(|run| run_text(run).trim() == "•")
        .map(|group| group.iter().collect())
        .collect();

    let artist_runs: Vec<&Value> = details
        .iter()
        .filter(|run| page_type(run) == Some("MUSIC_PAGE_TYPE_ARTIST"))
        .collect();
    let typed_artists: Vec<&str> = artist_runs.iter().map(|run| run_text(run)).collect();
    let artist_channels = artist_runs
        .iter()
        .filter_map(|run| {
            let id = run
                .pointer("/navigationEndpoint/browseEndpoint/browseId")
                .and_then(Value::as_str)
                .filter(|id| is_channel_id(id))?;
            Some((clean_text(run_text(run))?, id.to_owned()))
        })
        .collect();
    let artists = if typed_artists.is_empty() {
        // Artists without a YouTube Music page have no link; take the first
        // group, skipping a leading type label in unfiltered results.
        let first = groups
            .iter()
            .find(|g| {
                !matches!(
                    g.iter().map(|r| run_text(r)).collect::<String>().trim(),
                    "Song" | "Video"
                )
            })
            .cloned()
            .unwrap_or_default();
        clean_artists(
            first
                .into_iter()
                .map(run_text)
                .filter(|t| !matches!(t.trim(), "," | "&" | "and" | "x" | "")),
        )
    } else {
        clean_artists(typed_artists)
    };

    let all_details = detail_runs(item);
    let album = all_details
        .iter()
        .find(|run| page_type(run) == Some("MUSIC_PAGE_TYPE_ALBUM"))
        .and_then(|run| clean_text(run_text(run)));
    let duration_secs = all_details
        .iter()
        .rev()
        .find_map(|run| parse_duration(run_text(run)));

    Some(Song {
        video_id,
        title,
        artists,
        album,
        duration_secs,
        thumbnail_url: best_thumbnail(item),
        artist_channels,
    })
}

fn best_thumbnail(item: &Value) -> Option<String> {
    let url = item
        .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")?
        .as_array()?
        .iter()
        .max_by_key(|t| t.get("width").and_then(Value::as_u64).unwrap_or(0))?
        .get("url")?
        .as_str()?;
    if !url.starts_with("https://") {
        return None;
    }
    // Cover URLs end in a size directive ("=w120-h120-l90-rj"); request the
    // large version instead of the 120px list thumbnail.
    Some(match url.rfind('=') {
        Some(idx) if url.contains("googleusercontent.com") => {
            format!("{}{COVER_SIZE}", &url[..idx])
        }
        _ => url.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, page: Option<&str>) -> Value {
        match page {
            Some(page) => json!({"text": text, "navigationEndpoint": {"browseEndpoint": {
                "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": page}}}}}),
            None => json!({"text": text}),
        }
    }

    fn item(columns: Vec<Vec<Value>>) -> Value {
        json!({"musicResponsiveListItemRenderer": {
            "playlistItemData": {"videoId": "lXc8EHmFXOM"},
            "flexColumns": columns.into_iter().map(|runs| json!({
                "musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": runs}}
            })).collect::<Vec<_>>(),
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                {"url": "https://lh3.googleusercontent.com/abc=w60-h60-l90-rj", "width": 60},
                {"url": "https://lh3.googleusercontent.com/abc=w120-h120-l90-rj", "width": 120}
            ]}}}
        }})
    }

    #[test]
    fn parses_song_rows() {
        let doc = json!({"contents": [item(vec![
            vec![run("Glue", None)],
            vec![
                run("Bicep", Some("MUSIC_PAGE_TYPE_ARTIST")), run(" & ", None),
                run("Someone", Some("MUSIC_PAGE_TYPE_ARTIST")), run(" • ", None),
                run("Bicep", Some("MUSIC_PAGE_TYPE_ALBUM")), run(" • ", None), run("4:30", None),
            ],
            vec![run("114M plays", None)],
        ])]});
        let songs = parse_songs(&doc);
        assert_eq!(songs.len(), 1);
        let song = &songs[0];
        assert_eq!(song.video_id, "lXc8EHmFXOM");
        assert_eq!(song.title, "Glue");
        assert_eq!(song.artists, vec!["Bicep", "Someone"]);
        assert_eq!(song.album.as_deref(), Some("Bicep"));
        assert_eq!(song.duration_secs, Some(270.0));
        assert_eq!(
            song.thumbnail_url.as_deref(),
            Some("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj")
        );
    }

    #[test]
    fn search_rows_carry_artist_channels() {
        let mut linked = run("Bicep", Some("MUSIC_PAGE_TYPE_ARTIST"));
        linked["navigationEndpoint"]["browseEndpoint"]["browseId"] =
            json!("UCgapGYYYntw8Wg57E1DI7NA");
        let mut bogus = run("Other", Some("MUSIC_PAGE_TYPE_ARTIST"));
        bogus["navigationEndpoint"]["browseEndpoint"]["browseId"] = json!("../../x");
        let doc = item(vec![
            vec![run("Glue", None)],
            vec![linked, run(" & ", None), bogus],
        ]);
        let song = parse_songs(&doc).remove(0);
        assert_eq!(song.artists, vec!["Bicep", "Other"]);
        assert_eq!(
            song.artist_channels,
            vec![("Bicep".to_owned(), "UCgapGYYYntw8Wg57E1DI7NA".to_owned())]
        );
    }

    #[test]
    fn playlist_rows_read_album_and_fixed_duration_columns() {
        let mut doc = item(vec![
            vec![run("Glue", None)],
            vec![run("Bicep", Some("MUSIC_PAGE_TYPE_ARTIST"))],
            vec![run("114M plays", None)],
            vec![run("Bicep", Some("MUSIC_PAGE_TYPE_ALBUM"))],
        ]);
        doc["musicResponsiveListItemRenderer"]["fixedColumns"] = json!([
            {"musicResponsiveListItemFixedColumnRenderer": {"text": {"runs": [{"text": "4:30"}]}}}
        ]);
        let song = parse_songs(&doc).remove(0);
        assert_eq!(song.album.as_deref(), Some("Bicep"));
        assert_eq!(song.duration_secs, Some(270.0));
    }

    #[test]
    fn parses_artist_pages() {
        let doc = json!({
            "header": {"musicImmersiveHeaderRenderer": {
                "title": {"runs": [{"text": "Bicep"}]},
                "description": {"runs": [
                    {"text": "Belfast duo.\n\nFrom Wikipedia ("},
                    {"text": "https://en.wikipedia.org/wiki/Bicep_(...", "navigationEndpoint":
                        {"urlEndpoint": {"url": "https://en.wikipedia.org/wiki/Bicep_(duo)"}}},
                    {"text": ")"}
                ]},
                "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                    {"url": "https://lh3.googleusercontent.com/banner=w1440-h600", "width": 1440}
                ]}}}
            }},
            "contents": {"sectionListRenderer": {"contents": [
                {"musicShelfRenderer": {"title": {"runs": [{"text": "Top songs", "navigationEndpoint":
                    {"browseEndpoint": {"browseId": "VLOLAK5uy_n2kszK6KKvmJ4g8ONn8etxIDJhMpD8OOc"}}}]}}}
            ]}},
            "microformat": {"microformatDataRenderer": {"thumbnail": {"thumbnails": [
                {"url": "https://lh3.googleusercontent.com/face=w544-h544", "width": 544}
            ]}}}
        });
        let artist = parse_artist(&doc);
        assert_eq!(artist.name.as_deref(), Some("Bicep"));
        assert_eq!(
            artist.biography.as_deref(),
            Some("Belfast duo.\n\nFrom Wikipedia (https://en.wikipedia.org/wiki/Bicep_(duo))")
        );
        assert_eq!(
            artist.image_url.as_deref(),
            Some("https://lh3.googleusercontent.com/face=w544-h544")
        );
        assert_eq!(
            artist.songs_playlist.as_deref(),
            Some("VLOLAK5uy_n2kszK6KKvmJ4g8ONn8etxIDJhMpD8OOc")
        );

        // A channel that is not an artist has no song list.
        let channel = json!({"header": {"musicVisualHeaderRenderer": {
            "title": {"runs": [{"text": "YouTube"}]}}}});
        let info = parse_artist(&channel);
        assert_eq!(info.name.as_deref(), Some("YouTube"));
        assert_eq!(info.songs_playlist, None);
        assert_eq!(info.biography, None);
    }

    #[test]
    fn finds_continuation_tokens() {
        let doc = json!({"contents": [{"continuationItemRenderer": {"continuationEndpoint":
            {"continuationCommand": {"token": "abc"}}}}]});
        assert_eq!(continuation_token(&doc).as_deref(), Some("abc"));
        assert_eq!(continuation_token(&json!({"contents": []})), None);
    }

    #[test]
    fn validates_channel_ids() {
        assert!(is_channel_id("UCgapGYYYntw8Wg57E1DI7NA"));
        assert!(!is_channel_id("UCgapGYYYntw8Wg57E1DI7N"));
        assert!(!is_channel_id("MPREb_HVXgc9Epq1Mxxxxxxx"));
        assert!(!is_channel_id("UC/../../../../../../etc"));
    }

    #[test]
    fn handles_unlinked_artists() {
        let doc = item(vec![
            vec![run("Track", None)],
            vec![
                run("Indie Person", None),
                run(" • ", None),
                run("3:05", None),
            ],
        ]);
        let song = parse_songs(&doc).remove(0);
        assert_eq!(song.artists, vec!["Indie Person"]);
        assert_eq!(song.album, None);
        assert_eq!(song.duration_secs, Some(185.0));
    }

    #[test]
    fn parses_player_descriptions() {
        let doc = json!({
            "videoDetails": {"shortDescription": "Provided to YouTube by Ninja Tune\n\nGlue · Bicep"},
            "microformat": {"playerMicroformatRenderer": {"publishDate": "2017-08-31T04:32:11-07:00"}}
        });
        let about = parse_video_about(&doc);
        assert_eq!(about.publish_date.as_deref(), Some("2017-08-31"));
        assert!(
            about
                .description
                .unwrap()
                .starts_with("Provided to YouTube by")
        );
        assert_eq!(parse_video_about(&json!({})), VideoAbout::default());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("1:02:03"), Some(3723.0));
        assert_eq!(parse_duration("114M plays"), None);
        assert_eq!(parse_duration("4:"), None);
    }
}
