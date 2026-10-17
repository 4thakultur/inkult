//! Core domain types shared by providers, the download pipeline, and the library.

pub mod length;
pub mod metadata;
pub mod provider;
pub mod quality;

use serde::Serialize;

/// Byte-level progress of a running download.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct DownloadProgress {
    pub downloaded_bytes: Option<u64>,
    /// Exact size if known, otherwise the provider's estimate.
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: Option<f64>,
}

impl DownloadProgress {
    /// Completion ratio in `0.0..=1.0`, when the total size is known.
    pub fn fraction(&self) -> Option<f64> {
        match (self.downloaded_bytes, self.total_bytes) {
            (Some(done), Some(total)) if total > 0 => {
                Some((done as f64 / total as f64).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }
}
