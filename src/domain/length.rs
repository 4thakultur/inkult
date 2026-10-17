//! The longest track Inkult accepts.
//!
//! Inkult is a library of tracks: uploads longer than the limit are almost
//! always DJ mixes, live sets or full albums in one file, so they are kept
//! out of search results, artist pages and collections, and refused at
//! download time. The limit is checked against the provider's listing first
//! (cheap, but sometimes missing) and again against the track's own
//! metadata or the received file before anything reaches the library.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::Serialize;

/// Upper bound on a track's length; tracks exactly at the limit pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxTrackLength(Option<u32>);

impl MaxTrackLength {
    /// 10:00.
    pub const DEFAULT: Self = Self(Some(600));
    pub const UNLIMITED: Self = Self(None);

    /// `0` means no limit.
    pub fn from_secs(secs: u32) -> Self {
        if secs == 0 {
            Self::UNLIMITED
        } else {
            Self(Some(secs))
        }
    }

    pub fn secs(self) -> Option<u32> {
        self.0
    }

    /// Whether a track of this length is too long. An unknown length is
    /// not: it is judged again once the track's own metadata is known.
    pub fn exceeds(self, duration_secs: Option<f64>) -> bool {
        match (self.0, duration_secs) {
            (Some(max), Some(d)) => d.is_finite() && d > f64::from(max),
            _ => false,
        }
    }
}

impl MaxTrackLength {
    /// Remove the items known to be too long; says how many went, if any.
    pub fn drop_too_long<T>(
        self,
        items: &mut Vec<T>,
        duration_secs: impl Fn(&T) -> Option<f64>,
    ) -> Option<HiddenTooLong> {
        let max_secs = self.0?;
        let before = items.len();
        items.retain(|item| !self.exceeds(duration_secs(item)));
        let count = before - items.len();
        (count > 0).then_some(HiddenTooLong { count, max_secs })
    }
}

/// The limit as the running server applies it. Captains change it from
/// the admin panel; every clone sees the change, so jobs and Soulseek
/// sessions started before it follow it too.
#[derive(Debug, Clone)]
pub struct TrackLengthLimit(Arc<AtomicU32>);

impl TrackLengthLimit {
    pub fn new(limit: MaxTrackLength) -> Self {
        Self(Arc::new(AtomicU32::new(limit.secs().unwrap_or(0))))
    }

    pub fn get(&self) -> MaxTrackLength {
        MaxTrackLength::from_secs(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, limit: MaxTrackLength) {
        self.0.store(limit.secs().unwrap_or(0), Ordering::Relaxed);
    }
}

/// Tracks left out of a listing for their length, so the UI can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HiddenTooLong {
    pub count: usize,
    pub max_secs: u32,
}

impl Default for MaxTrackLength {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `m:ss`, or `h:mm:ss` from an hour on.
pub struct Clock(pub f64);

impl fmt::Display for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.0.max(0.0).round() as u64;
        let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
        if h > 0 {
            write!(f, "{h}:{m:02}:{s:02}")
        } else {
            write!(f, "{m}:{s:02}")
        }
    }
}

/// A track refused for its length.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "this track is {} long, over the {} limit (likely a mix or a set)",
    Clock(*.duration_secs),
    Clock(f64::from(*.max_secs))
)]
pub struct TooLong {
    pub duration_secs: f64,
    pub max_secs: u32,
}

impl MaxTrackLength {
    pub fn check(self, duration_secs: Option<f64>) -> Result<(), TooLong> {
        match (self.0, duration_secs) {
            (Some(max_secs), Some(duration_secs)) if self.exceeds(Some(duration_secs)) => {
                Err(TooLong {
                    duration_secs,
                    max_secs,
                })
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_minutes_passes_one_second_more_does_not() {
        let max = MaxTrackLength::DEFAULT;
        assert!(!max.exceeds(Some(600.0)));
        assert!(max.exceeds(Some(600.5)));
        assert!(max.exceeds(Some(3600.0)));
        assert!(!max.exceeds(Some(245.0)));
    }

    #[test]
    fn unknown_or_garbage_length_is_not_judged() {
        let max = MaxTrackLength::DEFAULT;
        assert!(!max.exceeds(None));
        assert!(!max.exceeds(Some(f64::NAN)));
        assert!(max.check(None).is_ok());
    }

    #[test]
    fn zero_turns_the_limit_off() {
        assert_eq!(MaxTrackLength::from_secs(0), MaxTrackLength::UNLIMITED);
        assert!(!MaxTrackLength::UNLIMITED.exceeds(Some(86_400.0)));
    }

    #[test]
    fn drops_only_known_long_tracks_and_counts_them() {
        let mut lengths = vec![Some(200.0), Some(3600.0), None, Some(601.0)];
        let hidden = MaxTrackLength::DEFAULT.drop_too_long(&mut lengths, |d| *d);
        assert_eq!(lengths, vec![Some(200.0), None]);
        assert_eq!(
            hidden,
            Some(HiddenTooLong {
                count: 2,
                max_secs: 600
            })
        );
        assert_eq!(
            MaxTrackLength::DEFAULT.drop_too_long(&mut lengths, |d| *d),
            None
        );
    }

    #[test]
    fn refusal_names_both_lengths() {
        let err = MaxTrackLength::DEFAULT.check(Some(3725.0)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "this track is 1:02:05 long, over the 10:00 limit (likely a mix or a set)"
        );
    }
}
