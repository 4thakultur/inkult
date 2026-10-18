//! The on-disk music library and its index.
//!
//! The index (`library.json` in the data directory) maps stable provider
//! identities to library files so the same track is not downloaded twice.
//! An entry whose file has since been removed by the user no longer counts
//! as a duplicate. A corrupt index stops startup instead of being replaced,
//! so a bug or crash can never silently discard it.
//!
//! All users share one library on disk, so a track is stored once however
//! many people download it. Each entry records which users *own* it (have
//! downloaded it); pirates only see entries they own. Entries from before
//! accounts existed have no owners and are visible to captains only until
//! someone downloads them.

pub mod layout;
pub mod place;
pub mod sanitize;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::auth::UserId;
use crate::domain::metadata::TrackMetadata;
use crate::domain::provider::TrackKey;
use crate::domain::quality::{OutputFormat, QualityReport};
use crate::storage::{Visibility, write_atomic};

const INDEX_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error(
        "library index {path} is unreadable ({reason}); fix or move it away — it is never overwritten automatically"
    )]
    CorruptIndex { path: PathBuf, reason: String },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> LibraryError {
    let context = context.into();
    move |source| LibraryError::Io { context, source }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub key: TrackKey,
    /// Path relative to the library root.
    pub path: PathBuf,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub source_url: String,
    pub artwork_url: Option<String>,
    pub quality: QualityReport,
    /// Requested output format; entries from before formats existed are
    /// originals.
    #[serde(default)]
    pub output: OutputFormat,
    /// When the file was placed in the library. Unix timestamp (seconds).
    pub added_at: u64,
    /// Users who downloaded this file and therefore see it in their library.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub owners: BTreeSet<UserId>,
    /// When each owner got the file, which is later than `added_at` for
    /// someone who joined a download or claimed a shared track. Owners from
    /// before this was recorded have no entry (see [`LibraryEntry::added_for`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub owned_since: BTreeMap<UserId, u64>,
    /// Where the file itself came from, when that is not the track's own
    /// source: a quality upgrade found on Soulseek is filed under the
    /// original track (`key`, `source_url`) but is another user's file,
    /// e.g. `slsk://user/Music/Artist/01 Track.flac`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obtained_from: Option<String>,
}

impl LibraryEntry {
    /// When this file entered `user`'s library.
    pub fn added_for(&self, user: UserId) -> u64 {
        self.owned_since
            .get(&user)
            .copied()
            .unwrap_or(self.added_at)
    }

    fn add_owner(&mut self, user: UserId, now: u64) {
        if self.owners.insert(user) {
            self.owned_since.insert(user, now);
        }
    }

    fn drop_owner(&mut self, user: UserId) -> bool {
        self.owned_since.remove(&user);
        self.owners.remove(&user)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: Vec<LibraryEntry>,
}

pub struct Library {
    root: PathBuf,
    index_path: PathBuf,
    index: Mutex<IndexFile>,
}

impl Library {
    pub async fn open(root: PathBuf, index_path: PathBuf) -> Result<Self, LibraryError> {
        let index = match tokio::fs::read(&index_path).await {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| LibraryError::CorruptIndex {
                    path: index_path.clone(),
                    reason: e.to_string(),
                })?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => IndexFile {
                version: INDEX_VERSION,
                entries: Vec::new(),
            },
            Err(e) => return Err(io_err(format!("reading {}", index_path.display()))(e)),
        };
        Ok(Self {
            root,
            index_path,
            index: Mutex::new(index),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The existing library entry for this identity in this output format,
    /// if its file still exists. The same track in a different format (e.g.
    /// an MP3 copy for CDJs next to the original) is not a duplicate.
    pub async fn find(&self, key: &TrackKey, output: OutputFormat) -> Option<LibraryEntry> {
        let entry = {
            let index = self.index.lock().await;
            index
                .entries
                .iter()
                .rev()
                .find(|e| &e.key == key && e.output == output)
                .cloned()
        }?;
        let exists = tokio::fs::try_exists(self.root.join(&entry.path))
            .await
            .unwrap_or(false);
        exists.then_some(entry)
    }

    /// Absolute path of an entry's file, if it is still a regular file inside
    /// the library. Paths come from Inkult's own index, never from requests.
    pub async fn file_path(&self, entry: &LibraryEntry) -> Option<PathBuf> {
        if !entry
            .path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        let path = self.root.join(&entry.path);
        let meta = tokio::fs::metadata(&path).await.ok()?;
        meta.is_file().then_some(path)
    }

    /// Add `owners` to the current entry for this identity and format (the
    /// one [`Library::find`] returns). Returns the updated entry, or `None`
    /// if the track is not in the library.
    pub async fn add_owners(
        &self,
        key: &TrackKey,
        output: OutputFormat,
        owners: &BTreeSet<UserId>,
    ) -> Option<LibraryEntry> {
        let mut index = self.index.lock().await;
        let entry = index
            .entries
            .iter_mut()
            .rev()
            .find(|e| &e.key == key && e.output == output)?;
        if owners.is_subset(&entry.owners) {
            return Some(entry.clone());
        }
        let now = unix_now();
        for &owner in owners {
            entry.add_owner(owner, now);
        }
        let updated = entry.clone();
        persist(&self.index_path, &index).await;
        Some(updated)
    }

    /// Forget a deleted user's ownership. Files stay in the library: they
    /// are shared, and deleting music is never a side effect of account
    /// management.
    pub async fn remove_owner(&self, user: UserId) {
        let mut index = self.index.lock().await;
        let mut changed = false;
        for entry in &mut index.entries {
            changed |= entry.drop_owner(user);
        }
        if changed {
            persist(&self.index_path, &index).await;
        }
    }

    /// Remove a track in one format: for one user (`Only`) or for everyone
    /// (`Everyone`). The file is deleted from disk only once no owner is
    /// left, since the library is shared and other users may still have it.
    /// Targets the same entry [`Library::find`] would; `None` if there is
    /// no such entry or `Only` names a user who does not own it.
    pub async fn remove(
        &self,
        key: &TrackKey,
        output: OutputFormat,
        from: RemoveFrom,
    ) -> Result<Option<Removal>, LibraryError> {
        let mut index = self.index.lock().await;
        let Some(pos) = index
            .entries
            .iter()
            .rposition(|e| &e.key == key && e.output == output)
        else {
            return Ok(None);
        };
        if let RemoveFrom::Only(user) = from {
            let entry = &mut index.entries[pos];
            if !entry.drop_owner(user) {
                return Ok(None);
            }
            if !entry.owners.is_empty() {
                persist(&self.index_path, &index).await;
                return Ok(Some(Removal::Unlisted));
            }
        }

        let path = index.entries[pos].path.clone();
        // A stale entry whose file was removed by hand can share its path
        // with a later download; that file is still in use, so keep it.
        let shared = index
            .entries
            .iter()
            .enumerate()
            .any(|(i, e)| i != pos && e.path == path);
        if !shared {
            let root = self.root.clone();
            let relative = path.clone();
            tokio::task::spawn_blocking(move || delete_file(&root, &relative))
                .await
                .map_err(|e| io_err("deleting file")(io::Error::other(e)))?
                .map_err(io_err(format!("deleting {}", path.display())))?;
        }
        index.entries.remove(pos);
        persist(&self.index_path, &index).await;
        Ok(Some(Removal::Deleted))
    }

    /// Newest entries first.
    pub async fn entries(&self) -> Vec<LibraryEntry> {
        let index = self.index.lock().await;
        index.entries.iter().rev().cloned().collect()
    }

    /// Move a fully processed file into the library and record it.
    pub async fn import(
        &self,
        staged_file: PathBuf,
        meta: &TrackMetadata,
        quality: QualityReport,
        output: OutputFormat,
        extension: &str,
        owners: BTreeSet<UserId>,
    ) -> Result<LibraryEntry, LibraryError> {
        let entry = LibraryEntry {
            key: meta.key(),
            path: layout::relative_path(meta, extension),
            title: meta.title.clone(),
            artists: meta.artists.clone(),
            album: meta.album.clone(),
            source_url: meta.source_url.clone(),
            artwork_url: meta.artwork_url.clone(),
            quality,
            output,
            added_at: 0,
            owners,
            owned_since: BTreeMap::new(),
            obtained_from: None,
        };
        self.place_and_record(staged_file, entry).await
    }

    /// The best copy of a track on disk to convert from (see
    /// [`QualityReport::source_rank`]), with its absolute path.
    pub async fn best_source(&self, key: &TrackKey) -> Option<(LibraryEntry, PathBuf)> {
        let mut candidates: Vec<LibraryEntry> = {
            let index = self.index.lock().await;
            index
                .entries
                .iter()
                .filter(|e| &e.key == key)
                .cloned()
                .collect()
        };
        candidates.sort_by_key(|e| std::cmp::Reverse(e.quality.source_rank()));
        for entry in candidates {
            if let Some(path) = self.file_path(&entry).await {
                return Some((entry, path));
            }
        }
        None
    }

    /// Add a file converted from `source` to the library, next to the
    /// source (same name, new extension) so the formats of a track sit
    /// together. The source file is not touched.
    pub async fn import_converted(
        &self,
        staged_file: PathBuf,
        source: &LibraryEntry,
        quality: QualityReport,
        output: OutputFormat,
        extension: &str,
        owners: BTreeSet<UserId>,
    ) -> Result<LibraryEntry, LibraryError> {
        let entry = LibraryEntry {
            path: source.path.with_extension(extension),
            quality,
            output,
            owners,
            owned_since: BTreeMap::new(),
            ..source.clone()
        };
        self.place_and_record(staged_file, entry).await
    }

    /// Add a lossless copy of a track that came from somewhere else (a
    /// quality upgrade) as the track's [`OutputFormat::Lossless`] format,
    /// with `obtained_from` recording where the file really came from.
    /// Next to the library's copy when there is one (same identity and
    /// listing; that copy is not touched), else filed like a download of
    /// the track.
    pub async fn import_upgrade(
        &self,
        staged_file: PathBuf,
        of: UpgradeOf<'_>,
        quality: QualityReport,
        extension: &str,
        obtained_from: String,
        owners: BTreeSet<UserId>,
    ) -> Result<LibraryEntry, LibraryError> {
        let entry = match of {
            UpgradeOf::Track(track) => LibraryEntry {
                path: track.path.with_extension(extension),
                quality,
                output: OutputFormat::Lossless,
                owners,
                owned_since: BTreeMap::new(),
                obtained_from: Some(obtained_from),
                ..track.clone()
            },
            UpgradeOf::New(meta) => LibraryEntry {
                key: meta.key(),
                path: layout::relative_path(meta, extension),
                title: meta.title.clone(),
                artists: meta.artists.clone(),
                album: meta.album.clone(),
                source_url: meta.source_url.clone(),
                artwork_url: meta.artwork_url.clone(),
                quality,
                output: OutputFormat::Lossless,
                added_at: 0,
                owners,
                owned_since: BTreeMap::new(),
                obtained_from: Some(obtained_from),
            },
        };
        self.place_and_record(staged_file, entry).await
    }

    /// Move a staged file to `entry.path` (or a free variant of it, never
    /// overwriting) and add the entry, with the path actually used.
    async fn place_and_record(
        &self,
        staged_file: PathBuf,
        mut entry: LibraryEntry,
    ) -> Result<LibraryEntry, LibraryError> {
        let planned = entry.path.clone();
        // Hold the index lock across placement so concurrent imports of
        // similarly named tracks are serialized together with their entries.
        let mut index = self.index.lock().await;

        let root = self.root.clone();
        let planned_for_task = planned.clone();
        entry.path = tokio::task::spawn_blocking(move || {
            place::place_file(&staged_file, &root, &planned_for_task)
        })
        .await
        .map_err(|e| io_err("placing file")(io::Error::other(e)))?
        .map_err(io_err(format!("placing {}", planned.display())))?;
        entry.added_at = unix_now();
        entry.owned_since = entry.owners.iter().map(|&o| (o, entry.added_at)).collect();

        index.entries.push(entry.clone());
        index.version = INDEX_VERSION;
        persist(&self.index_path, &index).await;
        Ok(entry)
    }
}

/// What a quality upgrade is a copy of.
#[derive(Debug, Clone, Copy)]
pub enum UpgradeOf<'a> {
    /// A track in the library.
    Track(&'a LibraryEntry),
    /// A track known only from its provider (upgraded from a search result).
    New(&'a TrackMetadata),
}

/// Whose copy of a track [`Library::remove`] removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveFrom {
    /// This user's library; the file stays while anyone else owns it.
    Only(UserId),
    /// Everyone's: the file is deleted.
    Everyone,
}

/// What [`Library::remove`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Removal {
    /// Gone from the user's library; the file stays for its other owners.
    Unlisted,
    /// The entry and its file are gone.
    Deleted,
}

/// Delete a library file (already gone is fine), then the directories it
/// leaves empty, up to but not including the library root. Paths come from
/// Inkult's own index; anything that is not a plain relative path is refused
/// so a damaged index cannot delete outside the library.
fn delete_file(root: &Path, relative: &Path) -> io::Result<()> {
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a path inside the library",
        ));
    }
    let path = root.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => std::fs::remove_file(&path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // `remove_dir` only removes empty directories, so other music and
    // anything the user put there is left alone.
    let mut dir = relative.parent();
    while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        if std::fs::remove_dir(root.join(d)).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Save the index, logging failure. The files are safely in the library;
/// only duplicate detection and ownership for them are degraded until the
/// next successful save, so the in-memory state is kept rather than lost.
async fn persist(path: &Path, index: &IndexFile) {
    if let Err(e) = save_index(path, index).await {
        tracing::error!(error = %e, "failed to persist library index");
    }
}

/// Write the index atomically (temp file + fsync + rename).
async fn save_index(path: &Path, index: &IndexFile) -> Result<(), LibraryError> {
    let json = serde_json::to_vec_pretty(index)
        .map_err(|e| io_err("serializing library index")(io::Error::other(e)))?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || write_atomic(&path, &json, Visibility::Normal))
        .await
        .map_err(|e| io_err("writing library index")(io::Error::other(e)))?
        .map_err(io_err("writing library index"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::ProviderKind;
    use crate::domain::quality::Codec;

    fn meta(id: &str) -> TrackMetadata {
        TrackMetadata {
            provider: ProviderKind::SoundCloud,
            provider_track_id: id.into(),
            source_url: "https://soundcloud.com/a/b".into(),
            title: "Same Title".into(),
            artists: vec!["Artist".into()],
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

    fn quality() -> QualityReport {
        QualityReport::new(
            Codec::Mp3,
            "mp3".into(),
            Some(128),
            Some(44_100),
            None,
            Some(2),
        )
    }

    #[tokio::test]
    async fn imports_detects_duplicates_and_persists() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let index_path = data.path().join("library.json");
        let library = Library::open(root.path().into(), index_path.clone())
            .await
            .unwrap();

        let staged = data.path().join("media.mp3");
        std::fs::write(&staged, b"a").unwrap();
        let entry = library
            .import(
                staged,
                &meta("1"),
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::new(),
            )
            .await
            .unwrap();
        assert_eq!(entry.path, PathBuf::from("Artist/Singles/Same Title.mp3"));
        assert!(
            library
                .find(&meta("1").key(), OutputFormat::Original)
                .await
                .is_some()
        );

        // Different identity, same title: kept side by side, not deduplicated.
        assert!(
            library
                .find(&meta("2").key(), OutputFormat::Original)
                .await
                .is_none()
        );
        let staged = data.path().join("media2.mp3");
        std::fs::write(&staged, b"b").unwrap();
        let second = library
            .import(
                staged,
                &meta("2"),
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            second.path,
            PathBuf::from("Artist/Singles/Same Title (2).mp3")
        );

        // Another output format of the same track is not a duplicate.
        assert!(
            library
                .find(&meta("1").key(), OutputFormat::Mp3_320)
                .await
                .is_none()
        );

        // Index survives a reopen.
        let reopened = Library::open(root.path().into(), index_path).await.unwrap();
        assert_eq!(reopened.entries().await.len(), 2);

        // Deleting the file means it is no longer considered present.
        std::fs::remove_file(root.path().join(&entry.path)).unwrap();
        assert!(
            reopened
                .find(&meta("1").key(), OutputFormat::Original)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn ownership_is_granted_persisted_and_revoked() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let index_path = data.path().join("library.json");
        let library = Library::open(root.path().into(), index_path.clone())
            .await
            .unwrap();
        let (anne, jack) = (UserId(1), UserId(2));

        let staged = data.path().join("media.mp3");
        std::fs::write(&staged, b"a").unwrap();
        let entry = library
            .import(
                staged,
                &meta("1"),
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::from([anne]),
            )
            .await
            .unwrap();
        assert_eq!(entry.owners, BTreeSet::from([anne]));

        // A second user gets the same file, not a new download.
        let key = meta("1").key();
        let updated = library
            .add_owners(&key, OutputFormat::Original, &BTreeSet::from([jack]))
            .await
            .unwrap();
        assert_eq!(updated.owners, BTreeSet::from([anne, jack]));
        assert_eq!(updated.path, entry.path);
        assert!(
            library
                .add_owners(&key, OutputFormat::Mp3_320, &BTreeSet::from([jack]))
                .await
                .is_none()
        );

        library.remove_owner(anne).await;
        let reopened = Library::open(root.path().into(), index_path).await.unwrap();
        assert_eq!(reopened.entries().await[0].owners, BTreeSet::from([jack]));
        assert!(root.path().join(&entry.path).exists());
    }

    #[tokio::test]
    async fn each_owner_keeps_the_time_they_got_the_file() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let index_path = data.path().join("library.json");
        let library = Library::open(root.path().into(), index_path.clone())
            .await
            .unwrap();
        let (anne, jack) = (UserId(1), UserId(2));

        let staged = data.path().join("media.mp3");
        std::fs::write(&staged, b"a").unwrap();
        let entry = library
            .import(
                staged,
                &meta("1"),
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::from([anne]),
            )
            .await
            .unwrap();
        assert!(entry.added_at > 0);
        assert_eq!(entry.added_for(anne), entry.added_at);

        // Joining later is recorded; joining again keeps the first time.
        let key = meta("1").key();
        let mut later = entry.clone();
        later.owned_since.clear();
        let updated = library
            .add_owners(&key, OutputFormat::Original, &BTreeSet::from([jack]))
            .await
            .unwrap();
        assert!(updated.owned_since.contains_key(&jack));
        assert!(updated.added_for(jack) >= entry.added_at);

        // Owners recorded before per-owner times existed fall back to the
        // file's time.
        later.owners.insert(jack);
        assert_eq!(later.added_for(jack), later.added_at);

        library.remove_owner(anne).await;
        let reopened = Library::open(root.path().into(), index_path).await.unwrap();
        let stored = &reopened.entries().await[0];
        assert_eq!(
            stored.owned_since.keys().copied().collect::<Vec<_>>(),
            vec![jack]
        );
        assert_eq!(stored.added_for(jack), updated.added_for(jack));
    }

    #[tokio::test]
    async fn converted_copies_sit_next_to_the_best_source() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let library = Library::open(root.path().into(), data.path().join("library.json"))
            .await
            .unwrap();
        let staged = data.path().join("media.mp3");
        std::fs::write(&staged, b"original").unwrap();
        let original = library
            .import(
                staged,
                &meta("1"),
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::from([UserId(1)]),
            )
            .await
            .unwrap();

        let staged = data.path().join("converted.aiff");
        std::fs::write(&staged, b"pcm").unwrap();
        let converted_quality =
            QualityReport::new(Codec::Pcm, "aiff".into(), None, None, Some(16), Some(2))
                .converted_from_file(&original.quality);
        let aiff = library
            .import_converted(
                staged,
                &original,
                converted_quality,
                OutputFormat::Aiff,
                "aiff",
                BTreeSet::from([UserId(2)]),
            )
            .await
            .unwrap();
        assert_eq!(aiff.path, PathBuf::from("Artist/Singles/Same Title.aiff"));
        assert_eq!(aiff.owners, BTreeSet::from([UserId(2)]));
        assert_eq!(aiff.title, original.title);
        assert_eq!(
            std::fs::read(root.path().join(&original.path)).unwrap(),
            b"original"
        );

        // The untouched original is preferred as a source over the AIFF.
        let (best, path) = library.best_source(&original.key).await.unwrap();
        assert_eq!(best.output, OutputFormat::Original);
        assert!(path.ends_with("Same Title.mp3"));

        // Once the original is gone, the AIFF is the best that is left.
        std::fs::remove_file(root.path().join(&original.path)).unwrap();
        let (best, _) = library.best_source(&original.key).await.unwrap();
        assert_eq!(best.output, OutputFormat::Aiff);
    }

    #[tokio::test]
    async fn upgrades_join_the_track_without_touching_it() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let library = Library::open(root.path().into(), data.path().join("library.json"))
            .await
            .unwrap();
        let staged = data.path().join("media.mp3");
        std::fs::write(&staged, b"original").unwrap();
        let original = library
            .import(
                staged,
                &meta("1"),
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::from([UserId(1)]),
            )
            .await
            .unwrap();

        let staged = data.path().join("received.flac");
        std::fs::write(&staged, b"flac").unwrap();
        let flac = QualityReport::new(
            Codec::Flac,
            "flac".into(),
            Some(900),
            Some(44_100),
            Some(16),
            Some(2),
        );
        let upgrade = library
            .import_upgrade(
                staged,
                UpgradeOf::Track(&original),
                flac,
                "flac",
                "slsk://peer/Music/Same Title.flac".into(),
                BTreeSet::from([UserId(1)]),
            )
            .await
            .unwrap();
        assert_eq!(upgrade.key, original.key);
        assert_eq!(upgrade.output, OutputFormat::Lossless);
        assert_eq!(upgrade.source_url, original.source_url);
        assert_eq!(
            upgrade.obtained_from.as_deref(),
            Some("slsk://peer/Music/Same Title.flac")
        );
        assert_eq!(
            upgrade.path,
            PathBuf::from("Artist/Singles/Same Title.flac")
        );
        assert_eq!(
            std::fs::read(root.path().join(&original.path)).unwrap(),
            b"original"
        );
        // The lossless copy is now what conversions start from.
        let (best, _) = library.best_source(&original.key).await.unwrap();
        assert_eq!(best.output, OutputFormat::Lossless);
        assert!(
            library
                .find(&original.key, OutputFormat::Lossless)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn removal_deletes_the_file_only_when_nobody_owns_it() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let index_path = data.path().join("library.json");
        let library = Library::open(root.path().into(), index_path.clone())
            .await
            .unwrap();
        let (anne, jack) = (UserId(1), UserId(2));
        let key = meta("1").key();

        let staged = data.path().join("media.mp3");
        std::fs::write(&staged, b"a").unwrap();
        let entry = library
            .import(
                staged,
                &meta("1"),
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::from([anne, jack]),
            )
            .await
            .unwrap();
        let file = root.path().join(&entry.path);
        // Something else the user keeps in the same folder.
        std::fs::write(file.with_file_name("notes.txt"), b"keep").unwrap();

        // Not an owner, or not in the library in that format: nothing happens.
        let stranger = RemoveFrom::Only(UserId(3));
        assert_eq!(
            library
                .remove(&key, OutputFormat::Original, stranger)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            library
                .remove(&key, OutputFormat::Aiff, RemoveFrom::Only(anne))
                .await
                .unwrap(),
            None
        );

        // Jack still owns it, so the file stays.
        assert_eq!(
            library
                .remove(&key, OutputFormat::Original, RemoveFrom::Only(anne))
                .await
                .unwrap(),
            Some(Removal::Unlisted)
        );
        assert!(file.exists());

        // The last owner removes it: entry and file go, the folder with
        // the user's other file stays.
        assert_eq!(
            library
                .remove(&key, OutputFormat::Original, RemoveFrom::Only(jack))
                .await
                .unwrap(),
            Some(Removal::Deleted)
        );
        assert!(!file.exists());
        assert!(file.with_file_name("notes.txt").exists());
        let reopened = Library::open(root.path().into(), index_path).await.unwrap();
        assert!(reopened.entries().await.is_empty());

        // Removing for everyone deletes regardless of owners, and prunes
        // the folders it leaves empty.
        let staged = data.path().join("media2.mp3");
        std::fs::write(&staged, b"b").unwrap();
        let mut album = meta("2");
        album.album = Some("Album".into());
        let entry = library
            .import(
                staged,
                &album,
                quality(),
                OutputFormat::Original,
                "mp3",
                BTreeSet::from([anne]),
            )
            .await
            .unwrap();
        assert_eq!(
            library
                .remove(&album.key(), OutputFormat::Original, RemoveFrom::Everyone)
                .await
                .unwrap(),
            Some(Removal::Deleted)
        );
        assert!(!root.path().join(&entry.path).exists());
        assert!(!root.path().join("Artist/Album").exists());
        assert!(root.path().join("Artist/Singles/notes.txt").exists());
    }

    #[test]
    fn delete_file_refuses_paths_outside_the_library() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside.mp3");
        std::fs::write(&outside, b"x").unwrap();
        let inner = root.path().join("lib");
        std::fs::create_dir(&inner).unwrap();
        assert!(delete_file(&inner, Path::new("../outside.mp3")).is_err());
        assert!(delete_file(&inner, &outside).is_err());
        assert!(delete_file(&inner, Path::new("")).is_err());
        assert!(outside.exists());
    }

    #[tokio::test]
    async fn corrupt_index_is_an_error_not_a_reset() {
        let root = tempfile::tempdir().unwrap();
        let index_path = root.path().join("library.json");
        std::fs::write(&index_path, b"{not json").unwrap();
        let err = Library::open(root.path().into(), index_path.clone())
            .await
            .err()
            .unwrap();
        assert!(matches!(err, LibraryError::CorruptIndex { .. }));
        assert_eq!(std::fs::read(&index_path).unwrap(), b"{not json");
    }
}
