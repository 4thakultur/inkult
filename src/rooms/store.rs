//! Persistence for chat rooms: one JSON file per room in `rooms/` under the
//! data directory, so a new message rewrites only its own room's (bounded)
//! history rather than every room's.
//!
//! Like the library index, an unreadable room file stops startup instead of
//! being replaced: the crew's history is never thrown away silently.

use std::io;
use std::path::{Path, PathBuf};

use super::{RoomId, RoomRecord};
use crate::storage::{Visibility, write_atomic};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "room file {path} is unreadable ({reason}); fix or move it away — it is never overwritten automatically"
    )]
    Corrupt { path: PathBuf, reason: String },
    #[error("{context} {path}: {source}")]
    Io {
        context: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("internal error: {0}")]
    Internal(String),
}

pub struct RoomStore {
    dir: PathBuf,
}

impl RoomStore {
    /// Open (creating if needed) the rooms directory and load every room.
    pub async fn open(dir: PathBuf) -> Result<(Self, Vec<RoomRecord>), StoreError> {
        let load_dir = dir.clone();
        let rooms = tokio::task::spawn_blocking(move || load_all(&load_dir))
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))??;
        Ok((Self { dir }, rooms))
    }

    fn path(&self, id: RoomId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub async fn save(&self, room: &RoomRecord) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(room).map_err(|e| StoreError::Internal(e.to_string()))?;
        let path = self.path(room.id);
        tokio::task::spawn_blocking(move || {
            // Chat between crew members is private to this server.
            write_atomic(&path, &bytes, Visibility::Private).map_err(|source| StoreError::Io {
                context: "saving room",
                path,
                source,
            })
        })
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?
    }

    pub async fn remove(&self, id: RoomId) -> Result<(), StoreError> {
        let path = self.path(id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                context: "deleting room",
                path,
                source,
            }),
        }
    }
}

fn load_all(dir: &Path) -> Result<Vec<RoomRecord>, StoreError> {
    let io_err = |context, path: &Path| {
        let path = path.to_path_buf();
        move |source| StoreError::Io {
            context,
            path,
            source,
        }
    };
    std::fs::create_dir_all(dir).map_err(io_err("creating rooms directory", dir))?;
    let mut rooms = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io_err("reading rooms directory", dir))? {
        let path = entry
            .map_err(io_err("reading rooms directory", dir))?
            .path();
        // Skips `*.json.tmp` left by a crash mid-save; the previous
        // `*.json` is still intact next to it.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(io_err("reading room", &path))?;
        let corrupt = |reason: String| StoreError::Corrupt {
            path: path.clone(),
            reason,
        };
        let room: RoomRecord =
            serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem != room.id.to_string() {
            return Err(corrupt(format!(
                "file name does not match room id {}",
                room.id
            )));
        }
        rooms.push(room);
    }
    Ok(rooms)
}
