//! Fetching an HLS rendition's segments in parallel.
//!
//! ffmpeg's HLS reader downloads one segment after another, so capturing a
//! track takes one CDN round trip per ~10 s of audio: several seconds for
//! a typical track, which is how long its waveform took to appear. Fetching
//! a few segments at once, and handing them to ffmpeg in order, removes
//! most of that wait.
//!
//! Only plain media playlists are handled: unencrypted segments, each a
//! whole file, on the playlist's own host over HTTPS. Anything else
//! (encryption, byte ranges, master playlists, other hosts) is left to
//! ffmpeg's HLS reader.

use std::collections::VecDeque;

use tokio::io::AsyncWriteExt;
use url::Url;

use crate::tools::AbortOnDrop;

/// Segments fetched at once.
const PARALLEL_SEGMENTS: usize = 6;
/// Largest playlist accepted.
const MAX_PLAYLIST_BYTES: usize = 1024 * 1024;

/// A media playlist whose segments can simply be concatenated.
#[derive(Debug, PartialEq, Eq)]
pub struct MediaPlaylist {
    /// Initialization section (fMP4 `EXT-X-MAP`), sent before the segments.
    pub init: Option<Url>,
    pub segments: Vec<Url>,
}

impl MediaPlaylist {
    /// Demuxer for the concatenated segments.
    pub fn demuxer(&self) -> &'static str {
        // fMP4 segments need their init section; MPEG-TS ones do not.
        if self.init.is_some() { "mov" } else { "mpegts" }
    }
}

/// Why a playlist is left to ffmpeg's HLS reader.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Unsupported {
    #[error("not a media playlist")]
    NotMedia,
    #[error("playlist uses {0}")]
    Feature(&'static str),
    #[error("segment URL is not on the playlist's host over HTTPS")]
    ForeignSegment,
}

/// Parse a media playlist fetched from `base`.
pub fn parse(text: &str, base: &Url) -> Result<MediaPlaylist, Unsupported> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(Unsupported::NotMedia);
    }
    let resolve = |reference: &str| -> Result<Url, Unsupported> {
        let url = base
            .join(reference)
            .map_err(|_| Unsupported::ForeignSegment)?;
        if url.scheme() != "https" || url.host_str() != base.host_str() || url.port() != base.port()
        {
            return Err(Unsupported::ForeignSegment);
        }
        Ok(url)
    };
    let mut init = None;
    let mut segments = Vec::new();
    let mut ended = false;
    for line in lines {
        if let Some(tag) = line.strip_prefix('#') {
            let name = tag.split(':').next().unwrap_or(tag);
            match name {
                "EXT-X-MAP" => {
                    if tag.contains("BYTERANGE") || init.is_some() {
                        return Err(Unsupported::Feature("byte-range or repeated init sections"));
                    }
                    let uri = quoted_attribute(tag, "URI").ok_or(Unsupported::NotMedia)?;
                    init = Some(resolve(uri)?);
                }
                "EXT-X-KEY" | "EXT-X-SESSION-KEY" => {
                    // METHOD=NONE is the only key that needs nothing.
                    if !tag.contains("METHOD=NONE") {
                        return Err(Unsupported::Feature("encryption"));
                    }
                }
                "EXT-X-BYTERANGE" => return Err(Unsupported::Feature("byte ranges")),
                "EXT-X-DISCONTINUITY" => return Err(Unsupported::Feature("discontinuities")),
                "EXT-X-STREAM-INF" | "EXT-X-I-FRAME-STREAM-INF" | "EXT-X-MEDIA" => {
                    return Err(Unsupported::NotMedia);
                }
                "EXT-X-ENDLIST" => ended = true,
                // Durations, versions, sequence numbers: not needed.
                _ => {}
            }
        } else {
            segments.push(resolve(line)?);
        }
    }
    // A live playlist without an end would grow while being read.
    let vod = ended || text.contains("#EXT-X-PLAYLIST-TYPE:VOD");
    if !vod {
        return Err(Unsupported::Feature("live playback"));
    }
    if segments.is_empty() {
        return Err(Unsupported::NotMedia);
    }
    Ok(MediaPlaylist { init, segments })
}

/// `KEY="value"` from an attribute list.
fn quoted_attribute<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let start = tag.find(&format!("{key}=\""))? + key.len() + 2;
    let len = tag[start..].find('"')?;
    Some(&tag[start..start + len])
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}")]
    Unsupported(#[from] Unsupported),
    #[error("CDN request failed: {0}")]
    Http(String),
    #[error("CDN answered {0}")]
    Status(reqwest::StatusCode),
    #[error("the track is too long to keep in memory")]
    TooLarge,
    #[error("ffmpeg stopped reading")]
    Closed,
}

impl FetchError {
    /// The signed URL was refused, most likely because it expired.
    pub fn is_expired(&self) -> bool {
        use reqwest::StatusCode;
        matches!(
            self,
            FetchError::Status(
                StatusCode::UNAUTHORIZED
                    | StatusCode::FORBIDDEN
                    | StatusCode::NOT_FOUND
                    | StatusCode::GONE
            )
        )
    }
}

fn request(
    http: &reqwest::Client,
    url: &Url,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    let mut request = http.get(url.clone());
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    request
}

async fn fetch(
    http: reqwest::Client,
    url: Url,
    headers: Vec<(String, String)>,
    max: usize,
) -> Result<Vec<u8>, FetchError> {
    // `without_url`: signed CDN URLs must not reach logs or the UI.
    let http_error = |e: reqwest::Error| FetchError::Http(e.without_url().to_string());
    let mut response = request(&http, &url, &headers)
        .send()
        .await
        .map_err(http_error)?;
    if !response.status().is_success() {
        return Err(FetchError::Status(response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        if body.len() + chunk.len() > max {
            return Err(FetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Fetch the playlist at `url`.
pub async fn fetch_playlist(
    http: &reqwest::Client,
    url: &Url,
    headers: &[(String, String)],
) -> Result<String, FetchError> {
    let body = fetch(
        http.clone(),
        url.clone(),
        headers.to_vec(),
        MAX_PLAYLIST_BYTES,
    )
    .await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Write the init section and every segment to `out`, in order, fetching
/// several ahead. At most `max_bytes` are written.
pub async fn write_segments(
    http: &reqwest::Client,
    playlist: &MediaPlaylist,
    headers: &[(String, String)],
    max_bytes: usize,
    mut out: impl tokio::io::AsyncWrite + Unpin,
) -> Result<(), FetchError> {
    let urls = playlist.init.iter().chain(&playlist.segments);
    let mut pending = urls.map(|url| {
        tokio::spawn(fetch(
            http.clone(),
            url.clone(),
            headers.to_vec(),
            max_bytes,
        ))
    });
    // Aborted when dropped, so an error or a cancelled capture stops the
    // fetches still running.
    let mut in_flight: VecDeque<AbortOnDrop<_>> = VecDeque::new();
    in_flight.extend(pending.by_ref().take(PARALLEL_SEGMENTS).map(AbortOnDrop));
    let mut written = 0usize;
    while let Some(mut next) = in_flight.pop_front() {
        let segment = (&mut next.0)
            .await
            .map_err(|e| FetchError::Http(e.to_string()))??;
        written += segment.len();
        if written > max_bytes {
            return Err(FetchError::TooLarge);
        }
        out.write_all(&segment)
            .await
            .map_err(|_| FetchError::Closed)?;
        in_flight.extend(pending.next().map(AbortOnDrop));
    }
    out.shutdown().await.map_err(|_| FetchError::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/track/aac/playlist.m3u8?sig=1").unwrap()
    }

    const SOUNDCLOUD: &str = "#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-MAP:URI=\"https://cdn.example/track/aac/init.mp4?sig=1\"
#EXTINF:10.007800,
https://cdn.example/track/aac/data000.m4s?sig=1
#EXTINF:9.984580,
data001.m4s?sig=2
#EXT-X-ENDLIST
";

    #[test]
    fn parses_soundcloud_style_playlists() {
        let playlist = parse(SOUNDCLOUD, &base()).unwrap();
        assert_eq!(
            playlist.init.unwrap().as_str(),
            "https://cdn.example/track/aac/init.mp4?sig=1"
        );
        let segments: Vec<_> = playlist.segments.iter().map(Url::as_str).collect();
        assert_eq!(
            segments,
            [
                "https://cdn.example/track/aac/data000.m4s?sig=1",
                "https://cdn.example/track/aac/data001.m4s?sig=2"
            ]
        );
        let ts = parse("#EXTM3U\n#EXTINF:10,\na.ts\n#EXT-X-ENDLIST\n", &base()).unwrap();
        assert_eq!(ts.demuxer(), "mpegts");
    }

    #[test]
    fn leaves_anything_unusual_to_ffmpeg() {
        let with = |extra: &str| parse(&SOUNDCLOUD.replace("#EXT-X-VERSION:7", extra), &base());
        assert_eq!(
            with("#EXT-X-KEY:METHOD=AES-128,URI=\"k\""),
            Err(Unsupported::Feature("encryption"))
        );
        assert!(with("#EXT-X-KEY:METHOD=NONE").is_ok());
        assert_eq!(
            with("#EXT-X-BYTERANGE:100@0"),
            Err(Unsupported::Feature("byte ranges"))
        );
        assert_eq!(
            parse(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlow.m3u8\n",
                &base()
            ),
            Err(Unsupported::NotMedia)
        );
        assert_eq!(
            parse("#EXTM3U\n#EXTINF:10,\na.ts\n", &base()),
            Err(Unsupported::Feature("live playback"))
        );
        assert_eq!(parse("<html>", &base()), Err(Unsupported::NotMedia));
    }

    #[test]
    fn segments_must_stay_on_the_playlist_host() {
        for segment in [
            "https://elsewhere.example/a.m4s",
            "http://cdn.example/a.m4s",
            "https://cdn.example:8443/a.m4s",
            "file:///etc/passwd",
        ] {
            let text = format!("#EXTM3U\n#EXTINF:10,\n{segment}\n#EXT-X-ENDLIST\n");
            assert_eq!(
                parse(&text, &base()),
                Err(Unsupported::ForeignSegment),
                "{segment}"
            );
        }
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"https://127.0.0.1/init.mp4\"\n#EXTINF:10,\na.m4s\n#EXT-X-ENDLIST\n";
        assert_eq!(parse(text, &base()), Err(Unsupported::ForeignSegment));
    }
}
