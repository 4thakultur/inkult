use std::fmt;

use serde::{Deserialize, Serialize};

use super::provider::{ProviderKind, TrackKey};

/// Normalized track metadata, independent of the provider it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackMetadata {
    pub provider: ProviderKind,
    pub provider_track_id: String,
    pub source_url: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub release_date: Option<ReleaseDate>,
    pub genres: Vec<String>,
    pub description: Option<String>,
    pub copyright: Option<String>,
    pub label: Option<String>,
    pub isrc: Option<String>,
    pub artwork_url: Option<String>,
    pub duration_secs: Option<f64>,
    /// Fields that were guessed (e.g. parsed from a video title) rather than
    /// provided as structured data. Surfaced in the UI so users know what to
    /// double-check.
    pub inferred: Vec<MetadataField>,
}

impl TrackMetadata {
    pub fn key(&self) -> TrackKey {
        TrackKey {
            provider: self.provider,
            id: self.provider_track_id.clone(),
        }
    }

    pub fn display_artist(&self) -> String {
        self.artists.join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataField {
    Title,
    Artists,
    Album,
    TrackNumber,
    ReleaseDate,
}

/// A release date with optional month/day precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReleaseDate {
    pub year: u16,
    pub month: Option<u8>,
    pub day: Option<u8>,
}

impl ReleaseDate {
    /// Parse `YYYYMMDD`, `YYYY-MM-DD`, `YYYY-MM` or `YYYY`.
    pub fn parse(raw: &str) -> Option<Self> {
        let digits: String = raw.trim().chars().filter(|c| *c != '-').collect();
        if !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let year: u16 = digits.get(0..4)?.parse().ok()?;
        if !(1000..=9999).contains(&year) {
            return None;
        }
        let month = match digits.get(4..6) {
            Some(m) => Some(m.parse::<u8>().ok().filter(|m| (1..=12).contains(m))?),
            None => None,
        };
        let day = match digits.get(6..8) {
            Some(d) => Some(d.parse::<u8>().ok().filter(|d| (1..=31).contains(d))?),
            None => None,
        };
        if digits.len() > 8 {
            return None;
        }
        Some(Self { year, month, day })
    }

    pub fn from_year(year: i64) -> Option<Self> {
        u16::try_from(year)
            .ok()
            .filter(|y| (1000..=9999).contains(y))
            .map(|year| Self {
                year,
                month: None,
                day: None,
            })
    }

    /// The UTC calendar date of a Unix timestamp (seconds).
    pub fn from_unix_timestamp(secs: i64) -> Option<Self> {
        // Howard Hinnant's days-to-civil algorithm (proleptic Gregorian).
        let days = secs.div_euclid(86_400) + 719_468;
        let era = days.div_euclid(146_097);
        let day_of_era = days.rem_euclid(146_097);
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let mp = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = year_of_era + era * 400 + i64::from(month <= 2);
        let mut date = Self::from_year(year)?;
        date.month = u8::try_from(month).ok();
        date.day = u8::try_from(day).ok();
        Some(date)
    }
}

impl fmt::Display for ReleaseDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}", self.year)?;
        if let Some(month) = self.month {
            write!(f, "-{month:02}")?;
            if let Some(day) = self.day {
                write!(f, "-{day:02}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_dates() {
        let d = ReleaseDate::parse("20190503").unwrap();
        assert_eq!(d.to_string(), "2019-05-03");
        assert_eq!(ReleaseDate::parse("2019-05-03"), Some(d));
        assert_eq!(ReleaseDate::parse("2019").unwrap().to_string(), "2019");
        assert_eq!(
            ReleaseDate::parse("2019-07").unwrap().to_string(),
            "2019-07"
        );
        assert_eq!(ReleaseDate::parse("20191303"), None);
        assert_eq!(ReleaseDate::parse("abc"), None);
        assert_eq!(ReleaseDate::parse("201905031"), None);
    }

    #[test]
    fn converts_unix_timestamps() {
        let date = |secs| ReleaseDate::from_unix_timestamp(secs).unwrap().to_string();
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_501_753_117), "2017-08-03");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_704_067_199), "2023-12-31");
        assert_eq!(ReleaseDate::from_unix_timestamp(-62_135_596_801), None);
    }
}
