//! Turning a file received from a Soulseek user into a library track.
//!
//! Files on Soulseek are other people's library files, so their own tags
//! are usually the best metadata available. Precedence, highest first:
//!
//! ```text
//! tags in the file  →  guessed from the folder and file name (flagged as inferred)
//! ```
//!
//! Inkult never trusts the peer's claims about the file: the codec and
//! quality are measured from the received file, and a file that does not
//! parse as audio is rejected. A lossless codec only means the file is
//! lossless *now*; Soulseek uploads are sometimes transcoded from lossy
//! sources, which no tag or container can reveal.

use std::collections::BTreeSet;
use std::path::PathBuf;

use lofty::picture::MimeType;

use super::split_path;
use crate::auth::UserId;
use crate::domain::length::{MaxTrackLength, TooLong};
use crate::domain::metadata::{MetadataField, ReleaseDate, TrackMetadata};
use crate::domain::provider::TrackKey;
use crate::domain::quality::OutputFormat;
use crate::library::{Library, LibraryEntry, LibraryError};
use crate::metadata::tagging::{self, ExistingTags};
use crate::metadata::{clean_artists, clean_text, split_artist_title};

const MAX_COVER_BYTES: u64 = 16 * 1024 * 1024;

pub(super) struct ImportRequest {
    pub peer: String,
    /// The peer's path of the file.
    pub path: String,
    pub key: TrackKey,
    /// The complete file in the staging directory.
    pub staged: PathBuf,
    /// The folder's cover image, if it was downloaded.
    pub folder_cover: Option<PathBuf>,
    pub max_track_length: MaxTrackLength,
    pub owners: BTreeSet<UserId>,
}

pub(super) struct ImportOutcome {
    pub entry: LibraryEntry,
    /// Where the embedded artwork came from: "file", "folder" or "none".
    pub artwork: &'static str,
    pub inferred: Vec<String>,
    pub note: Option<String>,
    /// The track was already in the library; only ownership was added.
    pub already_present: bool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ImportError {
    #[error("the received file is not playable audio: {0}")]
    InvalidMedia(String),
    #[error("could not write metadata: {0}")]
    Tagging(String),
    #[error(transparent)]
    TooLong(#[from] TooLong),
    #[error("could not add the file to the library: {0}")]
    Library(#[from] LibraryError),
    #[error("{0}")]
    Io(String),
}

pub(super) async fn import(
    library: &Library,
    request: ImportRequest,
) -> Result<ImportOutcome, ImportError> {
    // The same file may have been imported meanwhile (by another user).
    if library
        .find(&request.key, OutputFormat::Original)
        .await
        .is_some()
        && let Some(entry) = library
            .add_owners(&request.key, OutputFormat::Original, &request.owners)
            .await
    {
        let _ = tokio::fs::remove_file(&request.staged).await;
        return Ok(ImportOutcome {
            entry,
            artwork: "file",
            inferred: Vec::new(),
            note: Some("Already in the library; added to yours".into()),
            already_present: true,
        });
    }

    let extension: String = super::extension(&request.path)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    // Tag libraries recognise formats partly by extension.
    let staged = request.staged.with_extension(&extension);
    tokio::fs::rename(&request.staged, &staged)
        .await
        .map_err(|e| ImportError::Io(format!("cannot prepare the file: {e}")))?;

    let cover = match &request.folder_cover {
        Some(path) => read_cover(path).await,
        None => None,
    };
    let source_url = format!(
        "slsk://{}/{}",
        request.peer,
        request.path.replace('\\', "/")
    );
    let guess = guess_from_path(&request.path);
    let key = request.key.clone();
    let audio = staged.clone();
    let max_track_length = request.max_track_length;
    let prepared = tokio::task::spawn_blocking(move || {
        let codec =
            tagging::detect_codec(&audio).map_err(|e| ImportError::InvalidMedia(e.to_string()))?;
        // Search replies without a length can't be screened: the file is.
        max_track_length.check(tagging::probe_duration(&audio).map(|d| d.as_secs_f64()))?;
        let quality = tagging::probe_quality(&audio, codec)
            .map_err(|e| ImportError::InvalidMedia(e.to_string()))?;
        let existing = tagging::read_existing_tags(&audio)
            .map_err(|e| ImportError::InvalidMedia(e.to_string()))?;
        let meta = build_metadata(&key, source_url, &existing, &guess);
        let (artwork, artwork_source) = match (existing.has_front_cover, cover) {
            (true, _) => (None, "file"),
            (false, Some(cover)) => (Some(cover), "folder"),
            (false, None) => (None, "none"),
        };
        tagging::write_tags(&audio, &meta, artwork)
            .map_err(|e| ImportError::Tagging(e.to_string()))?;
        tagging::probe_quality(&audio, codec)
            .map_err(|e| ImportError::InvalidMedia(format!("after tagging: {e}")))?;
        Ok::<_, ImportError>((meta, quality, artwork_source))
    })
    .await
    .map_err(|e| ImportError::Io(e.to_string()));
    let (meta, quality, artwork) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(e)) | Err(e) => {
            let _ = tokio::fs::remove_file(&staged).await;
            return Err(e);
        }
    };

    let entry = library
        .import(
            staged.clone(),
            &meta,
            quality,
            OutputFormat::Original,
            &extension,
            request.owners,
        )
        .await;
    let entry = match entry {
        Ok(entry) => entry,
        Err(e) => {
            let _ = tokio::fs::remove_file(&staged).await;
            return Err(e.into());
        }
    };
    let note = entry.quality.lossless.then(|| {
        "Lossless file. Soulseek uploads are sometimes transcoded from lossy sources; \
         check the spectrum if it matters"
            .to_owned()
    });
    Ok(ImportOutcome {
        inferred: meta
            .inferred
            .iter()
            .map(|f| {
                serde_json::to_value(f)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            })
            .collect(),
        entry,
        artwork,
        note,
        already_present: false,
    })
}

async fn read_cover(path: &std::path::Path) -> Option<(Vec<u8>, MimeType)> {
    let size = tokio::fs::metadata(path).await.ok()?.len();
    if size == 0 || size > MAX_COVER_BYTES {
        return None;
    }
    let data = tokio::fs::read(path).await.ok()?;
    let mime = tagging::artwork_mime(&data)?;
    Some((data, mime))
}

/// What a peer's path suggests about a track.
#[derive(Debug, Default, PartialEq)]
pub(super) struct PathGuess {
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<u16>,
}

/// Folder names that say nothing about artist or album.
const GENERIC_DIRS: &[&str] = &[
    "music",
    "my music",
    "music library",
    "mp3",
    "mp3s",
    "flac",
    "audio",
    "media",
    "downloads",
    "download",
    "complete",
    "completed",
    "incoming",
    "shared",
    "share",
    "shares",
    "soulseek",
    "slsk",
    "albums",
    "album",
    "singles",
    "single",
    "tracks",
    "misc",
    "various",
    "unsorted",
    "new",
    "dj",
    "vinyl rips",
    "lossless",
    "music collection",
    "collection",
];

/// Bracketed words that describe the rip, not the release.
const FORMAT_WORDS: &[&str] = &[
    "flac", "mp3", "aac", "alac", "ogg", "opus", "wav", "aiff", "320", "256", "192", "v0", "v2",
    "cbr", "vbr", "web", "cd", "cdr", "vinyl", "lossless", "hires", "hi", "res", "bit", "khz",
    "16bit", "24bit", "16", "24", "44", "44.1", "48", "88", "88.2", "96", "176", "192khz", "kbps",
    "scene", "retail", "promo", "rip", "webflac", "lp",
];

fn is_generic(dir: &str) -> bool {
    dir.starts_with("@@") || GENERIC_DIRS.contains(&dir.trim().to_lowercase().as_str())
}

/// "CD2", "Disc 1", "disk 03" → the disc number.
fn disc_folder(dir: &str) -> Option<u32> {
    let lower = dir.trim().to_lowercase();
    let rest = ["cd", "disc", "disk"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))?;
    rest.trim_start_matches([' ', '-', '_', '.'])
        .parse()
        .ok()
        .filter(|n| (1..=99).contains(n))
}

fn year_of(text: &str) -> Option<u16> {
    let year: u16 = text.trim().parse().ok()?;
    (1900..=2100).contains(&year).then_some(year)
}

/// Remove rip descriptions such as "[FLAC]" or "(24-96)" from a folder
/// name, and pick up a year in brackets. Other brackets ("(Deluxe
/// Edition)") are part of the album name and stay.
fn clean_release_name(raw: &str) -> (String, Option<u16>) {
    let mut out = String::new();
    let mut year = None;
    let mut rest = raw;
    while let Some(start) = rest.find(['(', '[', '{']) {
        let close = match &rest[start..start + 1] {
            "(" => ')',
            "[" => ']',
            _ => '}',
        };
        let Some(len) = rest[start + 1..].find(close) else {
            break;
        };
        let inner = &rest[start + 1..start + 1 + len];
        let end = start + len + 2;
        let words: Vec<String> = inner
            .split(|c: char| !c.is_alphanumeric() && c != '.')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        if let Some(y) = year_of(inner) {
            year = year.or(Some(y));
            out.push_str(&rest[..start]);
        } else if !words.is_empty()
            && words
                .iter()
                .all(|w| FORMAT_WORDS.contains(&w.as_str()) || year_of(w).is_some())
        {
            if let Some(y) = words.iter().find_map(|w| year_of(w)) {
                year = year.or(Some(y));
            }
            out.push_str(&rest[..start]);
        } else {
            out.push_str(&rest[..end]);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    let mut name = clean_text(&out).unwrap_or_default();
    // "1999 - Album" or "Album - 1999"
    if let Some((left, right)) = name.split_once(" - ") {
        if let Some(y) = year_of(left) {
            year = year.or(Some(y));
            name = right.trim().to_owned();
        } else if let Some(y) = year_of(right) {
            year = year.or(Some(y));
            name = left.trim().to_owned();
        }
    }
    (name, year)
}

/// "01 - Title", "01. Title", "01 Title", "1-03 Title", "A1. Title" →
/// (disc, track, title). Vinyl positions ("A1") give no track number.
fn split_track_number(stem: &str) -> (Option<u32>, Option<u32>, &str) {
    let stem = stem.trim();
    let digits: usize = stem.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return (None, None, stem);
    }
    let (number, rest) = stem.split_at(digits);
    let mut disc = None;
    let mut track: u32 = match number.parse() {
        Ok(n) => n,
        Err(_) => return (None, None, stem),
    };
    let mut rest = rest;
    // "1-03" disc-track
    if let Some(after) = rest.strip_prefix('-') {
        let more: usize = after.chars().take_while(char::is_ascii_digit).count();
        if (1..=3).contains(&more) {
            disc = Some(track);
            track = after[..more].parse().unwrap_or(0);
            rest = &after[more..];
        }
    }
    let title = rest.trim_start_matches([' ', '.', '-', '_', ')']).trim();
    // Without a separator the number is part of the title ("2 Unlimited");
    // a lone digit before a plain space is taken as part of it too, while
    // "01 Title" (zero-padded) is a track number.
    let separated = rest.starts_with(['.', '-', '_', ')'])
        || (rest.starts_with(' ')
            && (digits >= 2 || disc.is_some() || rest.trim_start().starts_with(['-', '.'])));
    if title.is_empty() || !separated || track == 0 {
        return (None, None, stem);
    }
    (disc, Some(track), title)
}

pub(super) fn guess_from_path(path: &str) -> PathGuess {
    let parts: Vec<&str> = path.split('\\').filter(|p| !p.trim().is_empty()).collect();
    let Some((file, dirs)) = parts.split_last() else {
        return PathGuess::default();
    };
    let (_, name) = split_path(file);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let mut guess = PathGuess::default();

    let (disc, track, rest) = split_track_number(stem);
    guess.disc_number = disc;
    guess.track_number = track;
    match split_artist_title(rest) {
        Some((artist, title)) => {
            guess.artist = Some(artist);
            guess.title = Some(title);
        }
        None => guess.title = clean_text(rest),
    }

    let mut dirs: Vec<&str> = dirs.to_vec();
    if let Some(last) = dirs.last()
        && let Some(n) = disc_folder(last)
    {
        guess.disc_number = guess.disc_number.or(Some(n));
        dirs.pop();
    }
    let meaningful: Vec<&str> = dirs.into_iter().filter(|d| !is_generic(d)).collect();
    if let Some((album_dir, above)) = meaningful.split_last() {
        let (album_name, year) = clean_release_name(album_dir);
        guess.year = year;
        match album_name.split_once(" - ") {
            Some((artist, album)) if !artist.trim().is_empty() && !album.trim().is_empty() => {
                guess.album_artist = clean_text(artist);
                guess.album = clean_text(album);
            }
            _ => {
                guess.album = Some(album_name).filter(|a| !a.is_empty());
                guess.album_artist = above.last().and_then(|a| clean_text(a));
            }
        }
    }
    if guess.artist.is_none() {
        guess.artist = guess.album_artist.clone();
    }
    guess
}

/// Combine the file's own tags with what its path suggests. Tags win;
/// path guesses only fill gaps and are recorded as inferred.
fn build_metadata(
    key: &TrackKey,
    source_url: String,
    tags: &ExistingTags,
    guess: &PathGuess,
) -> TrackMetadata {
    let mut inferred = Vec::new();
    let title = match &tags.title {
        Some(title) => title.clone(),
        None => {
            inferred.push(MetadataField::Title);
            guess.title.clone().unwrap_or_else(|| "Untitled".into())
        }
    };
    let artists = if tags.artists.is_empty() {
        let guessed = clean_artists(guess.artist.iter());
        if !guessed.is_empty() {
            inferred.push(MetadataField::Artists);
        }
        guessed
    } else {
        tags.artists.clone()
    };
    let album = tags.album.clone().or_else(|| {
        guess
            .album
            .clone()
            .inspect(|_| inferred.push(MetadataField::Album))
    });
    let from_tags_album = tags.album.is_some();
    let track_number = tags.track_number.or_else(|| {
        guess
            .track_number
            .inspect(|_| inferred.push(MetadataField::TrackNumber))
    });
    let release_date = tags.release_date.or_else(|| {
        guess
            .year
            .and_then(|y| ReleaseDate::from_year(i64::from(y)))
            .inspect(|_| inferred.push(MetadataField::ReleaseDate))
    });
    TrackMetadata {
        provider: key.provider,
        provider_track_id: key.id.clone(),
        source_url,
        title,
        artists,
        album_artist: tags.album_artist.clone().or_else(|| {
            // A guessed album artist only makes sense with the guessed album.
            (!from_tags_album)
                .then(|| guess.album_artist.clone())
                .flatten()
        }),
        album,
        track_number,
        disc_number: tags.disc_number.or(guess.disc_number),
        release_date,
        genres: tags.genres.clone(),
        description: None,
        copyright: tags.copyright.clone(),
        label: tags.label.clone(),
        isrc: tags.isrc.clone(),
        artwork_url: None,
        duration_secs: None,
        inferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::ProviderKind;

    #[test]
    fn guesses_album_layouts() {
        assert_eq!(
            guess_from_path("@@abcde\\Music\\Bicep\\Bicep (2017) [FLAC]\\03 - Glue.flac"),
            PathGuess {
                artist: Some("Bicep".into()),
                album_artist: Some("Bicep".into()),
                album: Some("Bicep".into()),
                title: Some("Glue".into()),
                track_number: Some(3),
                disc_number: None,
                year: Some(2017),
            }
        );
        assert_eq!(
            guess_from_path("@@x\\Downloads\\Bicep - Isles (2021) [24-96]\\CD2\\01. Atlas.flac"),
            PathGuess {
                artist: Some("Bicep".into()),
                album_artist: Some("Bicep".into()),
                album: Some("Isles".into()),
                title: Some("Atlas".into()),
                track_number: Some(1),
                disc_number: Some(2),
                year: Some(2021),
            }
        );
    }

    #[test]
    fn guesses_singles_and_keeps_meaningful_brackets() {
        let g = guess_from_path("music\\Singles\\Floating Points - Silhouettes (I, II & III).mp3");
        assert_eq!(g.artist.as_deref(), Some("Floating Points"));
        assert_eq!(g.title.as_deref(), Some("Silhouettes (I, II & III)"));
        assert_eq!(g.album, None);
        assert_eq!(g.track_number, None);

        let g = guess_from_path("a\\Artist\\Album (Deluxe Edition) [WEB FLAC]\\1-07 Song.flac");
        assert_eq!(g.album.as_deref(), Some("Album (Deluxe Edition)"));
        assert_eq!((g.disc_number, g.track_number), (Some(1), Some(7)));

        // A number that is part of the title is not a track number.
        let g = guess_from_path("x\\Hits\\2 Unlimited.mp3");
        assert_eq!(g.title.as_deref(), Some("2 Unlimited"));
        assert_eq!(g.track_number, None);
    }

    #[test]
    fn hostile_paths_stay_harmless() {
        let g = guess_from_path("..\\..\\..\\etc\\passwd");
        assert_eq!(g.title.as_deref(), Some("passwd"));
        let g = guess_from_path("");
        assert_eq!(g, PathGuess::default());
    }

    #[test]
    fn tags_win_and_guesses_are_flagged() {
        let key = TrackKey {
            provider: ProviderKind::Soulseek,
            id: "k".into(),
        };
        let guess = guess_from_path("m\\Bicep\\Isles (2021)\\03 - Apricots.flac");
        let tags = ExistingTags {
            title: Some("Apricots".into()),
            artists: vec!["Bicep".into()],
            ..ExistingTags::default()
        };
        let meta = build_metadata(&key, "slsk://u/p".into(), &tags, &guess);
        assert_eq!(meta.title, "Apricots");
        assert_eq!(meta.album.as_deref(), Some("Isles"));
        assert_eq!(meta.track_number, Some(3));
        assert_eq!(
            meta.inferred,
            vec![
                MetadataField::Album,
                MetadataField::TrackNumber,
                MetadataField::ReleaseDate
            ]
        );

        let untagged = build_metadata(&key, "slsk://u/p".into(), &ExistingTags::default(), &guess);
        assert!(untagged.inferred.contains(&MetadataField::Title));
        assert!(untagged.inferred.contains(&MetadataField::Artists));
        assert_eq!(untagged.artists, ["Bicep"]);
    }
}
