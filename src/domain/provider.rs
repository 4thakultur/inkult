use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The music sources Inkult knows how to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    YouTube,
    SoundCloud,
    /// Files from other users on the Soulseek network. Not a URL-based
    /// [`crate::providers::MusicProvider`]: see `crate::soulseek`.
    Soulseek,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 3] = [
        ProviderKind::YouTube,
        ProviderKind::SoundCloud,
        ProviderKind::Soulseek,
    ];

    /// Stable, lowercase identifier used in APIs, index files and staging paths.
    pub fn slug(self) -> &'static str {
        match self {
            ProviderKind::YouTube => "youtube",
            ProviderKind::SoundCloud => "soundcloud",
            ProviderKind::Soulseek => "soulseek",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ProviderKind::YouTube => "YouTube",
            ProviderKind::SoundCloud => "SoundCloud",
            ProviderKind::Soulseek => "Soulseek",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown provider '{0}'")]
pub struct UnknownProvider(String);

impl FromStr for ProviderKind {
    type Err = UnknownProvider;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.slug().eq_ignore_ascii_case(s))
            .ok_or_else(|| UnknownProvider(s.to_owned()))
    }
}

/// Stable identity of a track at a provider, used for duplicate detection.
///
/// Titles and artists are deliberately not part of the identity: different
/// recordings (remixes, live versions, re-uploads) frequently share them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackKey {
    pub provider: ProviderKind,
    pub id: String,
}

impl fmt::Display for TrackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider.slug(), self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_slugs_case_insensitively() {
        assert_eq!(
            "YouTube".parse::<ProviderKind>().unwrap(),
            ProviderKind::YouTube
        );
        assert_eq!(
            "soundcloud".parse::<ProviderKind>().unwrap(),
            ProviderKind::SoundCloud
        );
        assert!("spotify".parse::<ProviderKind>().is_err());
    }
}
