//! Search filters applied to provider results.
//!
//! Providers' search endpoints only take free text, so filters run on the
//! results Inkult gets back. Every filter is strict: a result passes only if
//! its data is known to match. Results the filter cannot judge (no year, no
//! label information) are counted separately so the user can choose to see
//! them, rather than being silently dropped or silently kept.
//!
//! Word matching works on whole words, case-insensitively: excluding `live`
//! drops "Glue (Live)" but not "Oliver". A quoted or multi-word entry such as
//! `"sped up"` matches those words next to each other.

use std::fmt;

use serde::Serialize;

use crate::providers::TrackSummary;

const MAX_LABEL_CHARS: usize = 100;
const MAX_EXCLUDE_CHARS: usize = 300;
const MAX_EXCLUDE_TERMS: usize = 20;
const MIN_YEAR: u16 = 1000;
const MAX_YEAR: u16 = 9999;
const MAX_DURATION_SECS: u32 = 24 * 60 * 60;

/// Filters as received from the user, before validation.
#[derive(Debug, Default, Clone)]
pub struct RawFilters {
    pub year_from: Option<String>,
    pub year_to: Option<String>,
    pub label: Option<String>,
    pub exclude: Option<String>,
    pub min_duration_secs: Option<String>,
    pub max_duration_secs: Option<String>,
    /// Keep results the year/label filters cannot judge.
    pub include_unknown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    #[error("{field} must be a year between {MIN_YEAR} and {MAX_YEAR}")]
    InvalidYear { field: &'static str },
    #[error("the 'from' year is after the 'to' year")]
    YearRangeReversed,
    #[error("{field} must be a number of seconds up to {MAX_DURATION_SECS}")]
    InvalidDuration { field: &'static str },
    #[error("the minimum length is longer than the maximum length")]
    DurationRangeReversed,
    #[error(
        "label filter must be at most {MAX_LABEL_CHARS} characters and contain a letter or digit"
    )]
    InvalidLabel,
    #[error(
        "exclude filter must be at most {MAX_EXCLUDE_CHARS} characters and {MAX_EXCLUDE_TERMS} terms"
    )]
    TooManyExcludes,
}

/// A sequence of lowercase words that must appear next to each other.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Phrase(Vec<String>);

impl Phrase {
    fn new(text: &str) -> Option<Self> {
        let words = words(text);
        (!words.is_empty()).then_some(Self(words))
    }

    fn found_in(&self, text: &str) -> bool {
        let haystack = words(text);
        haystack
            .windows(self.0.len())
            .any(|window| window == self.0)
    }
}

impl fmt::Display for Phrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(" "))
    }
}

/// Lowercase words, splitting on anything that is not a letter or digit.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Split an exclude list into phrases: whitespace and commas separate
/// entries, double quotes group words into one phrase.
fn parse_excludes(raw: &str) -> Vec<Phrase> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in raw.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                entries.push(std::mem::take(&mut current));
            }
            ',' => entries.push(std::mem::take(&mut current)),
            c if c.is_whitespace() && !quoted => entries.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    entries.push(current);
    let mut phrases: Vec<Phrase> = Vec::new();
    for phrase in entries.iter().filter_map(|e| Phrase::new(e)) {
        if !phrases.contains(&phrase) {
            phrases.push(phrase);
        }
    }
    phrases
}

fn non_empty(raw: &Option<String>) -> Option<&str> {
    raw.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn parse_year(raw: &Option<String>, field: &'static str) -> Result<Option<u16>, FilterError> {
    non_empty(raw)
        .map(|s| {
            s.parse::<u16>()
                .ok()
                .filter(|y| (MIN_YEAR..=MAX_YEAR).contains(y))
                .ok_or(FilterError::InvalidYear { field })
        })
        .transpose()
}

fn parse_duration(raw: &Option<String>, field: &'static str) -> Result<Option<u32>, FilterError> {
    non_empty(raw)
        .map(|s| {
            s.parse::<u32>()
                .ok()
                .filter(|d| *d <= MAX_DURATION_SECS)
                .ok_or(FilterError::InvalidDuration { field })
        })
        .transpose()
}

/// Validated search filters. The default filters nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    year_from: Option<u16>,
    year_to: Option<u16>,
    label: Option<Phrase>,
    exclude: Vec<Phrase>,
    min_duration_secs: Option<u32>,
    max_duration_secs: Option<u32>,
    include_unknown: bool,
}

/// Why a result was or was not shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// Known not to match.
    Hidden,
    /// Missing the data a filter needs.
    Unknown,
}

/// How many results the filters looked at and left out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FilterSummary {
    pub scanned: usize,
    pub hidden: usize,
    /// Left out because their year or label is not known. Zero when the
    /// user asked to include such results.
    pub unknown: usize,
}

impl SearchFilters {
    pub fn parse(raw: &RawFilters) -> Result<Self, FilterError> {
        let year_from = parse_year(&raw.year_from, "year from")?;
        let year_to = parse_year(&raw.year_to, "year to")?;
        if let (Some(from), Some(to)) = (year_from, year_to)
            && from > to
        {
            return Err(FilterError::YearRangeReversed);
        }

        let min_duration_secs = parse_duration(&raw.min_duration_secs, "minimum length")?;
        let max_duration_secs = parse_duration(&raw.max_duration_secs, "maximum length")?;
        if let (Some(min), Some(max)) = (min_duration_secs, max_duration_secs)
            && min > max
        {
            return Err(FilterError::DurationRangeReversed);
        }

        let label = match non_empty(&raw.label) {
            Some(label) if label.chars().count() <= MAX_LABEL_CHARS => {
                Some(Phrase::new(label).ok_or(FilterError::InvalidLabel)?)
            }
            Some(_) => return Err(FilterError::InvalidLabel),
            None => None,
        };

        let exclude = match non_empty(&raw.exclude) {
            Some(text) if text.chars().count() > MAX_EXCLUDE_CHARS => {
                return Err(FilterError::TooManyExcludes);
            }
            Some(text) => parse_excludes(text),
            None => Vec::new(),
        };
        if exclude.len() > MAX_EXCLUDE_TERMS {
            return Err(FilterError::TooManyExcludes);
        }

        Ok(Self {
            year_from,
            year_to,
            label,
            exclude,
            min_duration_secs,
            max_duration_secs,
            include_unknown: raw.include_unknown,
        })
    }

    /// Whether any filter is set (`include_unknown` alone filters nothing).
    pub fn is_active(&self) -> bool {
        self.needs_release_info()
            || !self.exclude.is_empty()
            || self.min_duration_secs.is_some()
            || self.max_duration_secs.is_some()
    }

    /// Whether results need release year and label, which some providers
    /// only find out with extra requests.
    pub fn needs_release_info(&self) -> bool {
        self.year_from.is_some() || self.year_to.is_some() || self.label.is_some()
    }

    /// The label as free text, for searching when no query was typed.
    pub fn label_text(&self) -> Option<String> {
        self.label.as_ref().map(Phrase::to_string)
    }

    pub fn judge(&self, track: &TrackSummary) -> Verdict {
        let mut unknown = false;

        let excluded = self.exclude.iter().any(|phrase| {
            phrase.found_in(&track.title)
                || track.artists.iter().any(|a| phrase.found_in(a))
                || track.album.as_deref().is_some_and(|a| phrase.found_in(a))
        });
        if excluded {
            return Verdict::Hidden;
        }

        if self.year_from.is_some() || self.year_to.is_some() {
            match track.year {
                Some(year) => {
                    let after_start = self.year_from.is_none_or(|from| year.year >= from);
                    let before_end = self.year_to.is_none_or(|to| year.year <= to);
                    if !(after_start && before_end) {
                        return Verdict::Hidden;
                    }
                }
                None => unknown = true,
            }
        }

        if let Some(label) = &self.label {
            let mut texts = track.label.iter().chain(&track.label_hints).peekable();
            if texts.peek().is_none() {
                unknown = true;
            } else if !texts.any(|text| label.found_in(text)) {
                return Verdict::Hidden;
            }
        }

        if self.min_duration_secs.is_some() || self.max_duration_secs.is_some() {
            match track.duration_secs {
                Some(secs) => {
                    let long_enough = self
                        .min_duration_secs
                        .is_none_or(|min| secs >= f64::from(min));
                    let short_enough = self
                        .max_duration_secs
                        .is_none_or(|max| secs <= f64::from(max));
                    if !(long_enough && short_enough) {
                        return Verdict::Hidden;
                    }
                }
                None => unknown = true,
            }
        }

        if unknown && !self.include_unknown {
            Verdict::Unknown
        } else {
            Verdict::Keep
        }
    }

    /// Keep the first `limit` results that pass, counting the ones left out
    /// on the way.
    pub fn apply(
        &self,
        results: Vec<TrackSummary>,
        limit: usize,
        summary: &mut FilterSummary,
    ) -> Vec<TrackSummary> {
        let mut kept = Vec::new();
        for track in results {
            if kept.len() == limit {
                break;
            }
            summary.scanned += 1;
            match self.judge(&track) {
                Verdict::Keep => kept.push(track),
                Verdict::Hidden => summary.hidden += 1,
                Verdict::Unknown => summary.unknown += 1,
            }
        }
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::ProviderKind;
    use crate::providers::{SummaryYear, YearBasis};

    fn track(title: &str) -> TrackSummary {
        TrackSummary {
            provider: ProviderKind::YouTube,
            url: "https://www.youtube.com/watch?v=lXc8EHmFXOM".into(),
            title: title.into(),
            artists: vec!["Bicep".into()],
            album: Some("Isles".into()),
            duration_secs: Some(270.0),
            thumbnail_url: None,
            track_number: None,
            year: Some(SummaryYear {
                year: 2017,
                basis: YearBasis::Release,
            }),
            label: Some("Ninja Tune".into()),
            label_hints: vec!["Ninja Tune".into(), "℗ 2017 Ninja Tune".into()],
            artist_links: Vec::new(),
            reposted: false,
        }
    }

    fn filters(f: impl FnOnce(&mut RawFilters)) -> SearchFilters {
        let mut raw = RawFilters::default();
        f(&mut raw);
        SearchFilters::parse(&raw).unwrap()
    }

    #[test]
    fn excludes_whole_words_and_phrases() {
        let f = filters(|r| r.exclude = Some(r#"live, "sped up" karaoke"#.into()));
        assert_eq!(
            f.judge(&track("Glue (Live at Printworks)")),
            Verdict::Hidden
        );
        assert_eq!(f.judge(&track("Glue - Sped Up")), Verdict::Hidden);
        assert_eq!(f.judge(&track("Oliver's Glue")), Verdict::Keep);
        assert_eq!(f.judge(&track("Speed it up")), Verdict::Keep);
        let mut by_artist = track("Glue");
        by_artist.artists.push("Karaoke Kings".into());
        assert_eq!(f.judge(&by_artist), Verdict::Hidden);
    }

    #[test]
    fn parses_exclude_lists() {
        let phrases = parse_excludes(r#"remix,  live "sped  up",edit remix"#);
        let shown: Vec<String> = phrases.iter().map(Phrase::to_string).collect();
        assert_eq!(shown, ["remix", "live", "sped up", "edit"]);
        assert!(parse_excludes(" , \"\" ").is_empty());
    }

    #[test]
    fn filters_by_year_range() {
        let f = filters(|r| {
            r.year_from = Some("2015".into());
            r.year_to = Some("2017".into());
        });
        assert_eq!(f.judge(&track("Glue")), Verdict::Keep);
        let mut newer = track("Apricots");
        newer.year = Some(SummaryYear {
            year: 2020,
            basis: YearBasis::Release,
        });
        assert_eq!(f.judge(&newer), Verdict::Hidden);
        let mut undated = track("Glue");
        undated.year = None;
        assert_eq!(f.judge(&undated), Verdict::Unknown);
    }

    #[test]
    fn unknown_results_can_be_included() {
        let f = filters(|r| {
            r.year_from = Some("2015".into());
            r.include_unknown = true;
        });
        let mut undated = track("Glue");
        undated.year = None;
        assert_eq!(f.judge(&undated), Verdict::Keep);
    }

    #[test]
    fn matches_label_in_any_hint() {
        let f = filters(|r| r.label = Some("ninja tune".into()));
        assert_eq!(f.judge(&track("Glue")), Verdict::Keep);

        let mut distributed = track("Glue");
        distributed.label = Some("The Orchard Enterprises".into());
        distributed.label_hints =
            vec!["The Orchard Enterprises".into(), "℗ 2017 Ninja Tune".into()];
        assert_eq!(f.judge(&distributed), Verdict::Keep);

        let mut other = track("Glue");
        other.label = None;
        other.label_hints = vec!["Warp Records".into()];
        assert_eq!(f.judge(&other), Verdict::Hidden);

        other.label_hints.clear();
        assert_eq!(f.judge(&other), Verdict::Unknown);

        // A part of a word is not a match.
        let f = filters(|r| r.label = Some("tune".into()));
        other.label_hints = vec!["Tuneful".into()];
        assert_eq!(f.judge(&other), Verdict::Hidden);
    }

    #[test]
    fn hidden_wins_over_unknown() {
        let f = filters(|r| {
            r.year_from = Some("2015".into());
            r.exclude = Some("live".into());
        });
        let mut t = track("Glue (Live)");
        t.year = None;
        assert_eq!(f.judge(&t), Verdict::Hidden);
    }

    #[test]
    fn filters_by_duration() {
        let f = filters(|r| {
            r.min_duration_secs = Some("120".into());
            r.max_duration_secs = Some("600".into());
        });
        assert_eq!(f.judge(&track("Glue")), Verdict::Keep);
        let mut clip = track("Glue");
        clip.duration_secs = Some(30.0);
        assert_eq!(f.judge(&clip), Verdict::Hidden);
        clip.duration_secs = None;
        assert_eq!(f.judge(&clip), Verdict::Unknown);
    }

    #[test]
    fn rejects_invalid_filters() {
        let parse = |f: fn(&mut RawFilters)| {
            let mut raw = RawFilters::default();
            f(&mut raw);
            SearchFilters::parse(&raw)
        };
        assert!(matches!(
            parse(|r| r.year_from = Some("17".into())),
            Err(FilterError::InvalidYear { .. })
        ));
        assert_eq!(
            parse(|r| {
                r.year_from = Some("2020".into());
                r.year_to = Some("2010".into());
            }),
            Err(FilterError::YearRangeReversed)
        );
        assert_eq!(
            parse(|r| {
                r.min_duration_secs = Some("600".into());
                r.max_duration_secs = Some("60".into());
            }),
            Err(FilterError::DurationRangeReversed)
        );
        assert!(matches!(
            parse(|r| r.max_duration_secs = Some("-5".into())),
            Err(FilterError::InvalidDuration { .. })
        ));
        assert_eq!(
            parse(|r| r.label = Some("℗ —".into())),
            Err(FilterError::InvalidLabel)
        );
        assert_eq!(
            parse(|r| r.exclude = Some((0..25).map(|i| format!("w{i} ")).collect())),
            Err(FilterError::TooManyExcludes)
        );
        assert!(matches!(
            parse(|r| r.label = Some("x".repeat(101))),
            Err(FilterError::InvalidLabel)
        ));
    }

    #[test]
    fn empty_filters_are_inactive() {
        let f = filters(|r| {
            r.year_from = Some("  ".into());
            r.exclude = Some(",".into());
            r.include_unknown = true;
        });
        assert!(!f.is_active());
        assert!(filters(|r| r.exclude = Some("live".into())).is_active());
        assert!(!filters(|r| r.exclude = Some("live".into())).needs_release_info());
        assert!(filters(|r| r.label = Some("Warp".into())).needs_release_info());
    }

    #[test]
    fn apply_counts_and_limits() {
        let f = filters(|r| r.exclude = Some("live".into()));
        let results = vec![
            track("Glue"),
            track("Glue (Live)"),
            track("Atlas"),
            track("Opal"),
        ];
        let mut summary = FilterSummary::default();
        let kept = f.apply(results, 2, &mut summary);
        assert_eq!(kept.len(), 2);
        assert_eq!(
            summary,
            FilterSummary {
                scanned: 3,
                hidden: 1,
                unknown: 0
            }
        );
    }
}
