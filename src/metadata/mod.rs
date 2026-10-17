//! Metadata normalization helpers shared across providers, and tag writing.
//!
//! Provider data is untrusted: every text field goes through [`clean_text`]
//! (bounded length, no control characters) before it enters the domain.

pub mod tagging;

/// Maximum characters kept for short fields (title, artist, album, ...).
pub const MAX_FIELD_CHARS: usize = 400;
/// Maximum characters kept for long free-form fields (description).
pub const MAX_DESCRIPTION_CHARS: usize = 5000;
/// Maximum number of artists kept for a single track.
pub const MAX_ARTISTS: usize = 16;

/// Trim, collapse whitespace, drop control characters and bound the length.
/// Returns `None` for values that end up empty.
pub fn clean_text(raw: &str) -> Option<String> {
    let collapsed = raw
        .split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let bounded: String = collapsed.chars().take(MAX_FIELD_CHARS).collect();
    (!bounded.is_empty()).then_some(bounded)
}

/// Like [`clean_text`] but keeps line breaks, for descriptions.
pub fn clean_multiline(raw: &str) -> Option<String> {
    let text: String = raw
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .chars()
        .take(MAX_DESCRIPTION_CHARS)
        .collect();
    (!text.is_empty()).then_some(text)
}

/// Clean and de-duplicate a list of artist names, preserving order.
pub fn clean_artists<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if let Some(name) = clean_text(name.as_ref())
            && !out
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&name))
        {
            out.push(name);
        }
        if out.len() >= MAX_ARTISTS {
            break;
        }
    }
    out
}

/// Bracketed title suffixes that describe the *video*, not the recording.
/// Mix/version descriptors such as "(Extended Mix)" are intentionally absent:
/// for DJs they identify a distinct recording and must be kept.
const TITLE_NOISE: &[&str] = &[
    "official video",
    "official music video",
    "official audio",
    "official lyric video",
    "official lyrics video",
    "official visualizer",
    "official visualiser",
    "lyric video",
    "lyrics video",
    "lyrics",
    "lyric",
    "audio",
    "visualizer",
    "visualiser",
    "music video",
    "video",
    "hd",
    "hq",
    "4k",
    "video oficial",
    "clip officiel",
];

/// Remove video-related bracketed noise like "(Official Video)" from a title.
pub fn strip_title_noise(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut rest = title;
    while let Some(start) = rest.find(['(', '[']) {
        let close = if rest[start..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(len) = rest[start + 1..].find(close) else {
            break;
        };
        let inner = rest[start + 1..start + 1 + len].trim().to_lowercase();
        let end = start + 1 + len + 1;
        if TITLE_NOISE.contains(&inner.as_str()) {
            out.push_str(&rest[..start]);
        } else {
            out.push_str(&rest[..end]);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    clean_text(&out).unwrap_or_else(|| title.trim().to_owned())
}

/// Words that, when they make up the whole right-hand side of "X - Y", mean
/// the dash separates a title from its version rather than artist from title.
const VERSION_WORDS: &[&str] = &[
    "mix",
    "remix",
    "edit",
    "version",
    "dub",
    "bootleg",
    "vip",
    "remaster",
    "remastered",
    "rework",
    "instrumental",
    "acapella",
    "live",
];

/// Split a video-style title such as "Artist - Title (Official Video)".
/// Returns `(artist, title)` or `None` when the title has no usable separator.
pub fn split_artist_title(raw: &str) -> Option<(String, String)> {
    let (artist, title) = [" - ", " – ", " — ", " -- "]
        .iter()
        .filter_map(|sep| raw.split_once(sep))
        .min_by_key(|(artist, _)| artist.len())?;
    let artist = clean_text(artist)?;
    let title = strip_title_noise(title);
    if title.is_empty() {
        return None;
    }

    let lower = title.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let is_version_suffix = words.len() <= 3
        && words
            .last()
            .is_some_and(|last| VERSION_WORDS.contains(last));
    if is_version_suffix {
        return None;
    }
    Some((artist, title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_strips_controls_and_whitespace() {
        assert_eq!(
            clean_text("  Hello\u{0007}\n  World\t ").as_deref(),
            Some("Hello World")
        );
        assert_eq!(clean_text(" \n\t "), None);
        assert_eq!(
            clean_text(&"a".repeat(10_000)).unwrap().len(),
            MAX_FIELD_CHARS
        );
    }

    #[test]
    fn clean_artists_dedups_case_insensitively() {
        assert_eq!(
            clean_artists(["Daft Punk", " daft punk ", "", "Pharrell Williams"]),
            vec!["Daft Punk", "Pharrell Williams"]
        );
    }

    #[test]
    fn strips_video_noise_but_keeps_mix_names() {
        assert_eq!(strip_title_noise("Strobe (Official Video) [HD]"), "Strobe");
        assert_eq!(
            strip_title_noise("Strobe (Club Edit) [Official Audio]"),
            "Strobe (Club Edit)"
        );
        assert_eq!(strip_title_noise("Unclosed (paren"), "Unclosed (paren");
    }

    #[test]
    fn splits_artist_and_title() {
        assert_eq!(
            split_artist_title("deadmau5 - Strobe (Official Audio)"),
            Some(("deadmau5".into(), "Strobe".into()))
        );
        assert_eq!(
            split_artist_title("Bicep – Glue (Extended Mix)"),
            Some(("Bicep".into(), "Glue (Extended Mix)".into()))
        );
        assert_eq!(split_artist_title("No separator here"), None);
    }

    #[test]
    fn does_not_mistake_version_suffix_for_title() {
        assert_eq!(split_artist_title("Glue - Extended Mix"), None);
        assert_eq!(split_artist_title("Song Name - Radio Edit"), None);
    }
}
