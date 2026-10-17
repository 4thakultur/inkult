//! Reading audio properties and writing normalized tags + artwork.
//!
//! These functions do blocking file I/O; call them from `spawn_blocking`.

use std::borrow::Cow;
use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::FileType;
use lofty::id3::v2::Id3v2Tag;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
use lofty::ogg::tag::VorbisComments;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::items::Timestamp;
use lofty::tag::{ItemValue, Tag, TagItem, TagType};

use crate::domain::metadata::{ReleaseDate, TrackMetadata};
use crate::domain::quality::{Codec, QualityReport};
use crate::metadata::{clean_artists, clean_text};

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("file is not readable audio: {0}")]
    InvalidMedia(#[source] lofty::error::FileParseError),
    #[error("could not write tags: {0}")]
    Write(#[source] lofty::error::FileEncodingError),
}

/// Measure the quality of a downloaded file from the file itself.
///
/// `expected` disambiguates containers that do not reveal their codec through
/// the generic properties (MP4 may hold AAC or ALAC).
pub fn probe_quality(path: &Path, expected: Codec) -> Result<QualityReport, TagError> {
    let file = lofty::read_from_path(path).map_err(TagError::InvalidMedia)?;
    let props = file.properties();
    let (codec, container) = match file.file_type() {
        FileType::Opus => (Codec::Opus, "opus"),
        FileType::Vorbis => (Codec::Vorbis, "ogg"),
        FileType::Flac => (Codec::Flac, "flac"),
        FileType::Mpeg => (Codec::Mp3, "mp3"),
        FileType::Wav => (Codec::Pcm, "wav"),
        FileType::Aiff => (Codec::Pcm, "aiff"),
        FileType::Aac => (Codec::Aac, "aac"),
        FileType::Mp4 => (
            if matches!(expected, Codec::Alac | Codec::Flac | Codec::Mp3) {
                expected
            } else {
                Codec::Aac
            },
            "m4a",
        ),
        _ => (expected, "unknown"),
    };
    Ok(QualityReport::new(
        codec,
        container.to_owned(),
        props.audio_bitrate().filter(|b| *b > 0),
        props.sample_rate(),
        props.bit_depth(),
        props.channels(),
    ))
}

/// The codec of an audio file, read from the file. MP4 containers are
/// opened once more to tell AAC from ALAC (both are common as `.m4a`).
pub fn detect_codec(path: &Path) -> Result<Codec, TagError> {
    let file = lofty::read_from_path(path).map_err(TagError::InvalidMedia)?;
    Ok(match file.file_type() {
        FileType::Opus => Codec::Opus,
        FileType::Vorbis => Codec::Vorbis,
        FileType::Flac => Codec::Flac,
        FileType::Mpeg => Codec::Mp3,
        FileType::Wav | FileType::Aiff => Codec::Pcm,
        FileType::Aac => Codec::Aac,
        FileType::Mp4 => mp4_codec(path).unwrap_or(Codec::Aac),
        _ => Codec::Other,
    })
}

fn mp4_codec(path: &Path) -> Option<Codec> {
    use lofty::config::ParseOptions;
    use lofty::mp4::{Mp4Codec, Mp4File};
    let mut reader = std::fs::File::open(path).ok()?;
    let file = Mp4File::read_from(&mut reader, ParseOptions::new()).ok()?;
    Some(match file.properties().codec()? {
        Mp4Codec::ALAC => Codec::Alac,
        Mp4Codec::FLAC => Codec::Flac,
        Mp4Codec::MP3 => Codec::Mp3,
        _ => Codec::Aac,
    })
}

/// Tags a file already carries, e.g. one shared by another user, normalized
/// the same way as provider metadata.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExistingTags {
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub release_date: Option<ReleaseDate>,
    pub genres: Vec<String>,
    pub label: Option<String>,
    pub copyright: Option<String>,
    pub isrc: Option<String>,
    pub has_front_cover: bool,
}

pub fn read_existing_tags(path: &Path) -> Result<ExistingTags, TagError> {
    let file = lofty::read_from_path(path).map_err(TagError::InvalidMedia)?;
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else {
        return Ok(ExistingTags::default());
    };
    let text = |key: ItemKey| tag.get_string(key).and_then(clean_text);
    let mut artists = clean_artists(tag.get_strings(ItemKey::TrackArtists));
    if artists.is_empty()
        && let Some(artist) = tag.artist()
    {
        // Multi-artist tags are often joined with "; " or NUL; "&" and ","
        // are not split, since they appear inside single artist names.
        artists = clean_artists(artist.split(['\0', ';']));
    }
    let release_date = tag.date().and_then(|ts| {
        ReleaseDate::from_year(i64::from(ts.year)).map(|mut date| {
            date.month = ts.month.filter(|m| (1..=12).contains(m));
            date.day = date.month.and(ts.day.filter(|d| (1..=31).contains(d)));
            date
        })
    });
    Ok(ExistingTags {
        title: tag.title().as_deref().and_then(clean_text),
        artists,
        album: tag.album().as_deref().and_then(clean_text),
        album_artist: text(ItemKey::AlbumArtist),
        track_number: tag.track().filter(|n| *n > 0),
        disc_number: tag.disk().filter(|n| *n > 0),
        release_date,
        genres: clean_artists(tag.get_strings(ItemKey::Genre)),
        label: text(ItemKey::Label).or_else(|| text(ItemKey::Publisher)),
        copyright: text(ItemKey::CopyrightMessage),
        isrc: text(ItemKey::Isrc),
        has_front_cover: tag
            .pictures()
            .iter()
            .any(|p| matches!(p.pic_type(), PictureType::CoverFront | PictureType::Other)),
    })
}

/// Playing time of an audio file, if its headers state it.
pub fn probe_duration(path: &Path) -> Option<std::time::Duration> {
    let file = lofty::read_from_path(path).ok()?;
    Some(file.properties().duration()).filter(|d| !d.is_zero())
}

/// Detect the image type of artwork bytes; returns `None` for formats we do
/// not embed (only JPEG and PNG are universally supported by players).
pub fn artwork_mime(data: &[u8]) -> Option<MimeType> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(MimeType::Jpeg)
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(MimeType::Png)
    } else {
        None
    }
}

/// Custom field holding the source URL (same name yt-dlp uses).
const SOURCE_URL_FIELD: &str = "PURL";
/// Custom field holding Inkult's stable identity, e.g. `youtube:lXc8EHmFXOM`,
/// so the library index can be rebuilt from the files themselves.
const TRACK_KEY_FIELD: &str = "INKULT_TRACK_KEY";
const MP4_FREEFORM_MEAN: &str = "com.apple.iTunes";

/// Write normalized metadata (and optionally front-cover artwork) into `path`.
///
/// The file's existing primary tag is updated; fields we have no value for
/// are left untouched rather than cleared.
pub fn write_tags(
    path: &Path,
    meta: &TrackMetadata,
    artwork: Option<(Vec<u8>, MimeType)>,
) -> Result<(), TagError> {
    let file = lofty::read_from_path(path).map_err(TagError::InvalidMedia)?;
    let tag_type = file.primary_tag_type();
    let mut tag = file
        .primary_tag()
        .cloned()
        .unwrap_or_else(|| Tag::new(tag_type));
    apply_metadata(&mut tag, meta, artwork);
    save_with_provenance(
        tag,
        tag_type,
        path,
        &meta.key().to_string(),
        &meta.source_url,
    )
}

/// Copy all tags and artwork from `source` into the freshly converted
/// `target`, translated to the target's tag format, then re-add Inkult's
/// provenance fields. Carrying the source's tags rather than regenerating
/// them keeps anything the user edited in their DJ software (comments,
/// BPM, key, corrected titles). Fields the target format cannot represent
/// are dropped by the translation. Returns whether artwork was carried.
pub fn copy_tags(
    source: &Path,
    target: &Path,
    track_key: &str,
    source_url: &str,
) -> Result<bool, TagError> {
    let source_file = lofty::read_from_path(source).map_err(TagError::InvalidMedia)?;
    let target_file = lofty::read_from_path(target).map_err(TagError::InvalidMedia)?;
    let tag_type = target_file.primary_tag_type();
    let mut tag = source_file
        .primary_tag()
        .or_else(|| source_file.first_tag())
        .cloned()
        .unwrap_or_else(|| Tag::new(tag_type));
    tag.re_map(tag_type);
    let has_artwork = !tag.pictures().is_empty();
    save_with_provenance(tag, tag_type, target, track_key, source_url)?;
    Ok(has_artwork)
}

fn apply_metadata(tag: &mut Tag, meta: &TrackMetadata, artwork: Option<(Vec<u8>, MimeType)>) {
    tag.set_title(meta.title.clone());
    if !meta.artists.is_empty() {
        tag.set_artist(meta.display_artist());
        tag.remove_key(ItemKey::TrackArtists);
        for artist in &meta.artists {
            tag.push(TagItem::new(
                ItemKey::TrackArtists,
                ItemValue::Text(artist.clone()),
            ));
        }
    }
    if let Some(album) = &meta.album {
        tag.set_album(album.clone());
    }
    set_text(tag, ItemKey::AlbumArtist, meta.album_artist.as_ref());
    if let Some(track) = meta.track_number {
        tag.set_track(track);
    }
    if let Some(disc) = meta.disc_number {
        tag.set_disk(disc);
    }
    if let Some(date) = meta.release_date {
        tag.set_date(Timestamp {
            year: date.year,
            month: date.month,
            day: date.day,
            hour: None,
            minute: None,
            second: None,
        });
    }
    if let Some((first, rest)) = meta.genres.split_first() {
        tag.set_genre(first.clone());
        for genre in rest {
            tag.push(TagItem::new(ItemKey::Genre, ItemValue::Text(genre.clone())));
        }
    }
    // The description goes into a dedicated field, never the comment: DJ
    // software uses the comment for cue notes and key annotations.
    set_text(tag, ItemKey::Description, meta.description.as_ref());
    set_text(tag, ItemKey::CopyrightMessage, meta.copyright.as_ref());
    set_text(tag, ItemKey::Label, meta.label.as_ref());
    set_text(tag, ItemKey::Publisher, meta.label.as_ref());
    set_text(tag, ItemKey::Isrc, meta.isrc.as_ref());

    if let Some((data, mime)) = artwork {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(
            Picture::unchecked(data)
                .pic_type(PictureType::CoverFront)
                .mime_type(mime)
                .build(),
        );
    }
}

/// Save the tag, adding source URL and track identity in each format's
/// native custom-field form. The generic `Tag` cannot carry custom fields,
/// so this goes through the concrete tag types.
fn save_with_provenance(
    tag: Tag,
    tag_type: TagType,
    path: &Path,
    track_key: &str,
    source_url: &str,
) -> Result<(), TagError> {
    let options = WriteOptions::default();
    let key = track_key.to_owned();
    let url = source_url.to_owned();
    let result = match tag_type {
        TagType::VorbisComments => {
            let mut comments = VorbisComments::from(tag);
            comments.insert(SOURCE_URL_FIELD.to_owned(), url);
            comments.insert(TRACK_KEY_FIELD.to_owned(), key);
            comments.save_to_path(path, options)
        }
        TagType::Mp4Ilst => {
            let mut ilst = Ilst::from(tag);
            for (name, value) in [(SOURCE_URL_FIELD, url), (TRACK_KEY_FIELD, key)] {
                ilst.insert(Atom::new(
                    AtomIdent::Freeform {
                        mean: Cow::Borrowed(MP4_FREEFORM_MEAN),
                        name: Cow::Borrowed(name),
                    },
                    AtomData::UTF8(value),
                ));
            }
            ilst.save_to_path(path, options)
        }
        TagType::Id3v2 => {
            let mut id3 = Id3v2Tag::from(tag);
            id3.insert_user_text(SOURCE_URL_FIELD.to_owned(), url);
            id3.insert_user_text(TRACK_KEY_FIELD.to_owned(), key);
            id3.save_to_path(path, options)
        }
        _ => {
            let mut tag = tag;
            tag.insert_text(ItemKey::AudioSourceUrl, url);
            tag.save_to_path(path, options)
        }
    };
    result.map_err(TagError::Write)
}

fn set_text(tag: &mut Tag, key: ItemKey, value: Option<&String>) {
    if let Some(value) = value {
        // `insert_text` returns false when the tag format has no mapping for
        // this key; that is expected for some formats and not an error.
        tag.insert_text(key, value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_artwork_types() {
        assert_eq!(
            artwork_mime(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(MimeType::Jpeg)
        );
        assert_eq!(artwork_mime(b"\x89PNG\r\n"), Some(MimeType::Png));
        assert_eq!(artwork_mime(b"RIFF....WEBP"), None);
    }

    /// A valid 16-bit stereo 44.1 kHz WAV with 0.1 s of silence.
    fn silent_wav() -> Vec<u8> {
        let data_len: u32 = 4410 * 4;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // channels
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&(44_100u32 * 4).to_le_bytes()); // byte rate
        wav.extend_from_slice(&4u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(wav.len() + data_len as usize, 0);
        wav
    }

    #[test]
    fn writes_and_reads_back_tags() {
        use crate::domain::metadata::ReleaseDate;
        use crate::domain::provider::ProviderKind;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("media.wav");
        std::fs::write(&path, silent_wav()).unwrap();

        let quality = probe_quality(&path, Codec::Other).unwrap();
        assert_eq!(quality.codec, Codec::Pcm);
        assert!(quality.lossless);
        assert_eq!(quality.sample_rate_hz, Some(44_100));
        assert_eq!(quality.bit_depth, Some(16));

        let meta = TrackMetadata {
            provider: ProviderKind::SoundCloud,
            provider_track_id: "42".into(),
            source_url: "https://soundcloud.com/a/b".into(),
            title: "Glue".into(),
            artists: vec!["Bicep".into(), "Guest".into()],
            album: Some("Bicep".into()),
            album_artist: None,
            track_number: Some(3),
            disc_number: None,
            release_date: ReleaseDate::parse("2017-09-01"),
            genres: vec!["Electronic".into()],
            description: None,
            copyright: None,
            label: Some("Ninja Tune".into()),
            isrc: None,
            artwork_url: None,
            duration_secs: None,
            inferred: vec![],
        };
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        write_tags(&path, &meta, Some((jpeg.clone(), MimeType::Jpeg))).unwrap();

        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Glue"));
        assert_eq!(tag.artist().as_deref(), Some("Bicep, Guest"));
        assert_eq!(tag.album().as_deref(), Some("Bicep"));
        assert_eq!(tag.track(), Some(3));
        // ID3 has no label frame; the label lives in Publisher (TPUB).
        assert_eq!(tag.get_string(ItemKey::Publisher), Some("Ninja Tune"));
        assert_eq!(tag.pictures()[0].data(), jpeg.as_slice());

        let id3 = Id3v2Tag::from(tag.clone());
        assert_eq!(
            id3.get_user_text(SOURCE_URL_FIELD),
            Some("https://soundcloud.com/a/b")
        );
        assert_eq!(id3.get_user_text(TRACK_KEY_FIELD), Some("soundcloud:42"));

        // Tagging must not damage the audio.
        assert_eq!(probe_quality(&path, Codec::Other).unwrap(), quality);

        // Tags, a user's own additions and artwork carry over to a copy in
        // another container (AIFF here; same ID3 family, different file).
        let mut edited = lofty::read_from_path(&path)
            .unwrap()
            .primary_tag()
            .unwrap()
            .clone();
        edited.insert_text(ItemKey::IntegerBpm, "124".into());
        edited.insert_text(ItemKey::Comment, "cue at 1:04".into());
        edited.save_to_path(&path, WriteOptions::default()).unwrap();
        let copy = dir.path().join("copy.wav");
        std::fs::write(&copy, silent_wav()).unwrap();
        assert!(copy_tags(&path, &copy, "soundcloud:42", "https://soundcloud.com/a/b").unwrap());
        let file = lofty::read_from_path(&copy).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Glue"));
        assert_eq!(tag.get_string(ItemKey::IntegerBpm), Some("124"));
        assert_eq!(tag.get_string(ItemKey::Comment), Some("cue at 1:04"));
        assert_eq!(tag.pictures()[0].data(), jpeg.as_slice());
        let id3 = Id3v2Tag::from(tag.clone());
        assert_eq!(id3.get_user_text(TRACK_KEY_FIELD), Some("soundcloud:42"));
    }
}
