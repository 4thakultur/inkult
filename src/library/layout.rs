//! Where a track lives inside the library.
//!
//! ```text
//! <Album Artist>/<Album>/<NN> - <Title>.<ext>              album tracks
//! <Album Artist>/<Album>/<D>-<NN> - <Title>.<ext>          disc 2+
//! Various Artists/<Album>/<NN> - <Artist> - <Title>.<ext>  compilations
//! <Artist>/Singles/<Title>.<ext>                           no album
//! ```
//!
//! A release whose album name equals the track title and has no track
//! number is a single (YouTube Music reports singles that way) and goes to
//! `Singles/` rather than a one-track album folder.

use std::path::PathBuf;

use super::sanitize::sanitize_component;
use crate::domain::metadata::TrackMetadata;

pub const UNKNOWN_ARTIST: &str = "Unknown Artist";
pub const VARIOUS_ARTISTS: &str = "Various Artists";
pub const SINGLES_DIR: &str = "Singles";

fn is_compilation(album_artist: &str) -> bool {
    ["various artists", "various", "va"].contains(&album_artist.to_lowercase().as_str())
}

/// Library-relative path for a track. `extension` must be a bare,
/// alphanumeric extension such as `opus`.
pub fn relative_path(meta: &TrackMetadata, extension: &str) -> PathBuf {
    let primary_artist = meta
        .artists
        .first()
        .map(String::as_str)
        .unwrap_or(UNKNOWN_ARTIST);
    let album_artist = meta.album_artist.as_deref().unwrap_or(primary_artist);
    let compilation = is_compilation(album_artist);
    let title = sanitize_component(&meta.title, "Untitled");

    let album = meta.album.as_deref().filter(|album| {
        !(meta.track_number.is_none() && album.trim().eq_ignore_ascii_case(meta.title.trim()))
    });

    let (artist_dir, album_dir, stem) = match album {
        Some(album) => {
            let number = match (meta.disc_number, meta.track_number) {
                (Some(disc), Some(track)) if disc > 1 => format!("{disc}-{track:02} - "),
                (_, Some(track)) => format!("{track:02} - "),
                _ => String::new(),
            };
            let stem = if compilation {
                let artist = sanitize_component(&meta.display_artist(), UNKNOWN_ARTIST);
                format!("{number}{artist} - {title}")
            } else {
                format!("{number}{title}")
            };
            let artist_dir = if compilation {
                VARIOUS_ARTISTS
            } else {
                album_artist
            };
            (artist_dir, sanitize_component(album, "Unknown Album"), stem)
        }
        None => (primary_artist, SINGLES_DIR.to_owned(), title),
    };

    let extension: String = extension
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let file_name = sanitize_component(&format!("{stem}.{extension}"), "track");

    [
        sanitize_component(artist_dir, UNKNOWN_ARTIST),
        album_dir,
        file_name,
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::ProviderKind;

    fn meta(title: &str, artists: &[&str]) -> TrackMetadata {
        TrackMetadata {
            provider: ProviderKind::YouTube,
            provider_track_id: "id".into(),
            source_url: "https://example".into(),
            title: title.into(),
            artists: artists.iter().map(|s| s.to_string()).collect(),
            album: None,
            album_artist: None,
            track_number: None,
            disc_number: None,
            release_date: None,
            genres: vec![],
            description: None,
            copyright: None,
            label: None,
            isrc: None,
            artwork_url: None,
            duration_secs: None,
            inferred: vec![],
        }
    }

    #[test]
    fn album_track() {
        let mut m = meta("Glue", &["Bicep"]);
        m.album = Some("Bicep".into());
        m.track_number = Some(3);
        assert_eq!(
            relative_path(&m, "opus"),
            PathBuf::from("Bicep/Bicep/03 - Glue.opus")
        );
        m.disc_number = Some(2);
        assert_eq!(
            relative_path(&m, "opus"),
            PathBuf::from("Bicep/Bicep/2-03 - Glue.opus")
        );
    }

    #[test]
    fn single_without_album() {
        let m = meta("Glue", &["Bicep", "Someone"]);
        assert_eq!(
            relative_path(&m, "m4a"),
            PathBuf::from("Bicep/Singles/Glue.m4a")
        );
    }

    #[test]
    fn album_named_after_title_is_a_single() {
        let mut m = meta("Glue", &["Bicep"]);
        m.album = Some("Glue".into());
        assert_eq!(
            relative_path(&m, "opus"),
            PathBuf::from("Bicep/Singles/Glue.opus")
        );
    }

    #[test]
    fn compilation() {
        let mut m = meta("Track", &["Artist A"]);
        m.album = Some("Club Hits".into());
        m.album_artist = Some("Various Artists".into());
        m.track_number = Some(7);
        assert_eq!(
            relative_path(&m, "mp3"),
            PathBuf::from("Various Artists/Club Hits/07 - Artist A - Track.mp3")
        );
    }

    #[test]
    fn unknown_artist_and_hostile_metadata() {
        let mut m = meta("../../etc/passwd", &[]);
        m.album = Some("..".into());
        m.track_number = Some(1);
        let path = relative_path(&m, "../x");
        assert_eq!(
            path,
            PathBuf::from("Unknown Artist/Unknown Album/01 - -..-etc-passwd.x")
        );
        assert!(
            path.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        );
    }
}
