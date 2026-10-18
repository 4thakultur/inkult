//! Downloads from Soulseek users.
//!
//! ```text
//! Queued ─► Requesting ─► Remote (in the peer's upload queue, with place)
//!                              │ peer: TransferRequest (our turn)
//!                              ▼
//!                           Offered ─► Transferring ─► Importing ─► Completed
//! (already in the library) ─► Skipped
//! any unfinished state ─► Failed (retry possible) | Cancelled
//! ```
//!
//! The peer decides when an upload starts; we ask to be queued
//! (`QueueUpload`) and wait, possibly for hours. While waiting we
//! periodically ask for our place in its queue and re-send the request in
//! case the peer restarted and lost its queue. A peer that cannot be reached
//! is retried on a growing schedule before the download is marked failed.
//!
//! Data is written to a partial file in the staging directory, named after
//! the file's identity, so an interrupted transfer resumes where it stopped
//! (the protocol's `FileOffset`), also after a restart: unfinished downloads
//! are saved in the Soulseek settings. Nothing reaches the library until the
//! file is complete, verified and tagged (see `import`).
//!
//! Folder downloads also fetch the folder's cover image (if any) and embed
//! it into tracks that carry no artwork of their own. The cover is asked
//! for first, so peers usually send it before the tracks.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::import::{self, ImportRequest, UpgradeImport};
use super::messages::{PeerMessage, SharedDir};
use super::session::Session;
use super::store::SavedDownload;
use super::upgrade::UpgradeTarget;
use super::wire::md5_hex;
use super::{SoulseekError, check_username, cover_rank, is_audio, now_ms, split_path};
use crate::domain::provider::{ProviderKind, TrackKey};
use crate::domain::quality::OutputFormat;
use crate::jobs::lock;

/// Files larger than this are refused (no audio file is anywhere near).
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_PATH_CHARS: usize = 1024;
/// Unfinished downloads per session.
const MAX_QUEUED: usize = 5000;
/// Finished downloads kept for display.
const MAX_FINISHED: usize = 1000;
const MAX_FILES_PER_REQUEST: usize = 2000;
/// Peers contacted at the same time when queueing.
const MAX_REQUESTING: usize = 8;
/// Waits between attempts to reach a peer; after the last, the download fails.
const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
];
/// Ask for our place in a peer's queue this often.
const PLACE_EVERY_MS: u64 = 2 * 60 * 1000;
/// Re-send the queue request this often, in case the peer forgot it.
const REQUEUE_EVERY_MS: u64 = 20 * 60 * 1000;
/// The peer opens the file connection soon after offering the upload.
const OFFER_TIMEOUT_MS: u64 = 2 * 60 * 1000;
/// A transfer with no data for this long is considered broken.
const STALL_TIMEOUT: Duration = Duration::from_secs(90);
/// The import waits this long for a folder cover that is being transferred.
const COVER_WAIT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    Requesting,
    Remote,
    Offered,
    Transferring,
    Importing,
    Completed,
    Skipped,
    Failed,
    Cancelled,
}

impl DownloadState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DownloadState::Completed
                | DownloadState::Skipped
                | DownloadState::Failed
                | DownloadState::Cancelled
        )
    }
}

/// A file to download, as the UI sends it.
#[derive(Debug, Clone, Deserialize)]
pub struct FileRequest {
    pub path: String,
    pub size: u64,
}

/// What the UI shows about a download.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadView {
    pub id: u64,
    pub username: String,
    pub path: String,
    pub size: u64,
    pub cover: bool,
    pub state: DownloadState,
    /// Place in the peer's upload queue, when it told us.
    pub place: Option<u32>,
    pub bytes: u64,
    pub speed_bytes_per_sec: Option<f64>,
    pub note: Option<String>,
    pub error: Option<String>,
    pub retryable: bool,
    pub attempts: u32,
    /// Library identity once imported (for playing and saving).
    pub track_key: Option<TrackKey>,
    /// Library-relative path of the imported file.
    pub destination: Option<String>,
    pub quality: Option<crate::domain::quality::QualityReport>,
    pub artwork: Option<String>,
    pub inferred: Vec<String>,
    /// Title of the library track this file is a quality upgrade for.
    pub upgrade_for: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

struct Download {
    view: DownloadView,
    key: TrackKey,
    /// Set when the file is a quality upgrade: it is filed under that
    /// library track instead of becoming a Soulseek track of its own.
    upgrade: Option<UpgradeTarget>,
    token: Option<u32>,
    next_attempt_ms: u64,
    last_queue_ms: u64,
    last_place_ms: u64,
    offered_at_ms: u64,
    cancel: CancellationToken,
}

impl Download {
    fn touch(&mut self) {
        self.view.updated_at_ms = now_ms();
    }

    fn set(&mut self, state: DownloadState) {
        self.view.state = state;
        if state != DownloadState::Transferring {
            self.view.speed_bytes_per_sec = None;
        }
        self.touch();
    }

    fn fail(&mut self, error: String, retryable: bool) {
        self.view.error = Some(error);
        self.view.retryable = retryable;
        self.view.note = None;
        self.view.place = None;
        self.set(DownloadState::Failed);
    }

    /// Whether we still want the file if the peer offers it now.
    fn wanted(&self) -> bool {
        match self.view.state {
            DownloadState::Queued
            | DownloadState::Requesting
            | DownloadState::Remote
            | DownloadState::Offered => true,
            DownloadState::Failed => self.view.retryable,
            _ => false,
        }
    }
}

/// Stable identity of a peer's file: the same file from the same user is
/// the same track; anything else is not assumed to be.
pub(super) fn file_key(peer: &str, path: &str) -> TrackKey {
    TrackKey {
        provider: ProviderKind::Soulseek,
        id: md5_hex(&format!("{peer}\n{path}")),
    }
}

pub(super) struct Downloads {
    items: Mutex<Vec<Download>>,
    next_id: Mutex<u64>,
    wake: Notify,
    requesting: Arc<Semaphore>,
    /// The saved list is out of date.
    dirty: Mutex<bool>,
}

fn new_download(
    id: u64,
    peer: String,
    path: String,
    size: u64,
    cover: bool,
    upgrade: Option<UpgradeTarget>,
) -> Download {
    let now = now_ms();
    Download {
        key: file_key(&peer, &path),
        view: DownloadView {
            id,
            username: peer,
            path,
            size,
            cover,
            state: DownloadState::Queued,
            place: None,
            bytes: 0,
            speed_bytes_per_sec: None,
            note: None,
            error: None,
            retryable: false,
            attempts: 0,
            track_key: None,
            destination: None,
            quality: None,
            artwork: None,
            inferred: Vec::new(),
            upgrade_for: upgrade.as_ref().map(|u| u.title.clone()),
            created_at_ms: now,
            updated_at_ms: now,
        },
        upgrade,
        token: None,
        next_attempt_ms: 0,
        last_queue_ms: 0,
        last_place_ms: 0,
        offered_at_ms: 0,
        cancel: CancellationToken::new(),
    }
}

impl Downloads {
    pub fn new(saved: &[SavedDownload]) -> Self {
        let items: Vec<Download> = saved
            .iter()
            .enumerate()
            .map(|(i, s)| {
                new_download(
                    i as u64 + 1,
                    s.peer.clone(),
                    s.path.clone(),
                    s.size,
                    s.cover,
                    s.upgrade.clone(),
                )
            })
            .collect();
        Self {
            next_id: Mutex::new(items.len() as u64 + 1),
            items: Mutex::new(items),
            wake: Notify::new(),
            requesting: Arc::new(Semaphore::new(MAX_REQUESTING)),
            dirty: Mutex::new(false),
        }
    }

    fn mark_dirty(&self) {
        *lock(&self.dirty) = true;
    }

    fn with<T>(&self, id: u64, f: impl FnOnce(&mut Download) -> T) -> Option<T> {
        lock(&self.items)
            .iter_mut()
            .find(|d| d.view.id == id)
            .map(f)
    }

    fn find_wanted(&self, peer: &str, path: &str, f: impl FnOnce(&mut Download)) -> bool {
        match lock(&self.items)
            .iter_mut()
            .find(|d| d.view.username == peer && d.view.path == path && d.wanted())
        {
            Some(d) => {
                f(d);
                true
            }
            None => false,
        }
    }

    pub fn active_count(&self) -> usize {
        lock(&self.items)
            .iter()
            .filter(|d| !d.view.state.is_terminal())
            .count()
    }

    fn saved(&self) -> Vec<SavedDownload> {
        lock(&self.items)
            .iter()
            .filter(|d| !d.view.state.is_terminal())
            .map(|d| SavedDownload {
                peer: d.view.username.clone(),
                path: d.view.path.clone(),
                size: d.view.size,
                cover: d.view.cover,
                upgrade: d.upgrade.clone(),
            })
            .collect()
    }

    /// The peer is ready to upload `filename` to us. Answer for the
    /// TransferRequest.
    pub fn offered(
        &self,
        peer: &str,
        token: u32,
        filename: &str,
        size: Option<u64>,
    ) -> PeerMessage {
        let size = size.filter(|s| *s <= MAX_FILE_BYTES);
        let accepted = size.is_some()
            && self.find_wanted(peer, filename, |d| {
                if let Some(size) = size {
                    d.view.size = size;
                }
                d.token = Some(token);
                d.offered_at_ms = now_ms();
                d.view.place = None;
                d.view.error = None;
                d.view.note = Some("Upload starting".into());
                d.set(DownloadState::Offered);
            });
        if accepted {
            tracing::debug!(%peer, token, "accepting Soulseek upload offer");
        }
        PeerMessage::TransferResponse {
            token,
            allowed: accepted,
            reason: (!accepted).then(|| "Cancelled".to_owned()),
        }
    }

    pub fn place_reported(&self, peer: &str, filename: &str, place: u32) {
        self.find_wanted(peer, filename, |d| {
            if d.view.state == DownloadState::Remote {
                d.view.place = Some(place);
                d.touch();
            }
        });
    }

    pub fn upload_failed(&self, peer: &str, filename: &str) {
        self.find_wanted(peer, filename, |d| {
            if d.view.state == DownloadState::Offered {
                d.view.note = Some("The peer's upload failed; waiting for it to try again".into());
                d.set(DownloadState::Remote);
            }
        });
    }

    pub fn denied(&self, peer: &str, filename: &str, reason: &str) {
        let reason: String = reason
            .chars()
            .filter(|c| !c.is_control())
            .take(100)
            .collect();
        // Queue limits clear up as the peer's queue drains; anything else
        // (not shared, banned, cancelled) will not change by retrying.
        let retryable = matches!(
            reason.as_str(),
            "Too many files" | "Too many megabytes" | "Pending shutdown." | "Queued"
        );
        let found = self.find_wanted(peer, filename, |d| {
            d.fail(format!("{peer} refused: {reason}"), retryable);
        });
        if found {
            self.mark_dirty();
        }
    }
}

/// Pick what to fetch from a folder listing: every audio file, and the
/// best-named cover image of each directory.
pub(super) fn folder_selection(dirs: &[SharedDir]) -> Vec<(String, u64, bool)> {
    let mut out = Vec::new();
    for dir in dirs {
        let join = |name: &str| {
            if dir.path.is_empty() {
                name.to_owned()
            } else {
                format!("{}\\{}", dir.path, name)
            }
        };
        let has_audio = dir.files.iter().any(|f| is_audio(&f.name));
        if has_audio
            && let Some(cover) = dir
                .files
                .iter()
                .filter(|f| f.size > 0 && f.size <= 16 * 1024 * 1024)
                .filter_map(|f| cover_rank(&f.name).map(|rank| (rank, f)))
                .min_by_key(|(rank, f)| (*rank, std::cmp::Reverse(f.size)))
                .map(|(_, f)| f)
        {
            out.push((join(&cover.name), cover.size, true));
        }
        for file in dir.files.iter().filter(|f| is_audio(&f.name)) {
            out.push((join(&file.name), file.size, false));
        }
    }
    out
}

fn check_path(path: &str) -> Result<(), SoulseekError> {
    if path.is_empty()
        || path.chars().count() > MAX_PATH_CHARS
        || path.chars().any(char::is_control)
    {
        return Err(SoulseekError::InvalidInput("invalid file path".into()));
    }
    Ok(())
}

impl Session {
    fn staging(&self) -> PathBuf {
        self.ctx
            .config
            .staging_dir
            .join("soulseek")
            .join(self.owner.to_string())
    }

    fn partial_path(&self, key: &TrackKey) -> PathBuf {
        self.staging().join(format!("{}.part", key.id))
    }

    /// Where a folder's cover is kept until its tracks are imported.
    fn cover_path(&self, peer: &str, path: &str) -> PathBuf {
        let (dir, _) = split_path(path);
        let ext = super::extension(path);
        self.staging()
            .join("covers")
            .join(format!("{}.{ext}", md5_hex(&format!("{peer}\n{dir}"))))
    }

    /// Queue files from `peer`. Audio files go to the library when done;
    /// cover images are used as artwork for the audio of their folder.
    /// Returns how many were queued (already queued files are skipped).
    pub async fn download_files(
        self: &Arc<Self>,
        peer: &str,
        files: Vec<FileRequest>,
    ) -> Result<usize, SoulseekError> {
        let peer = check_username(peer)?;
        if files.is_empty() || files.len() > MAX_FILES_PER_REQUEST {
            return Err(SoulseekError::InvalidInput(format!(
                "choose 1–{MAX_FILES_PER_REQUEST} files"
            )));
        }
        let mut wanted = Vec::new();
        for file in files {
            check_path(&file.path)?;
            if file.size > MAX_FILE_BYTES {
                return Err(SoulseekError::InvalidInput(format!(
                    "{} is too large",
                    split_path(&file.path).1
                )));
            }
            let cover = !is_audio(&file.path) && cover_rank(&file.path).is_some();
            if !cover && !is_audio(&file.path) {
                continue; // playlists, logs, nfo files: not music
            }
            wanted.push((file.path, file.size, cover));
        }
        if wanted.is_empty() {
            return Err(SoulseekError::InvalidInput(
                "none of these files is audio Inkult can add to the library".into(),
            ));
        }
        // Covers first, so peers send them before the tracks.
        wanted.sort_by_key(|(_, _, cover)| !cover);
        self.queue(peer, wanted).await
    }

    /// Queue every audio file (and the cover) of one of `peer`'s folders.
    pub async fn download_folder(
        self: &Arc<Self>,
        peer: &str,
        folder: &str,
    ) -> Result<usize, SoulseekError> {
        let peer = check_username(peer)?;
        check_path(folder)?;
        let dirs = self.folder_contents(&peer, folder).await?;
        let selection = folder_selection(&dirs);
        if selection.iter().all(|(_, _, cover)| *cover) {
            return Err(SoulseekError::InvalidInput(
                "this folder has no audio files Inkult can add to the library".into(),
            ));
        }
        self.queue(peer, selection).await
    }

    async fn queue(
        self: &Arc<Self>,
        peer: String,
        files: Vec<(String, u64, bool)>,
    ) -> Result<usize, SoulseekError> {
        let mut in_library = Vec::new();
        for (path, _, cover) in &files {
            if !cover {
                let key = file_key(&peer, path);
                if self
                    .ctx
                    .library
                    .find(&key, OutputFormat::Original)
                    .await
                    .is_some()
                {
                    in_library.push(key);
                }
            }
        }
        let owners = BTreeSet::from([self.owner]);
        let mut queued = 0;
        {
            let mut items = lock(&self.downloads.items);
            let unfinished = items.iter().filter(|d| !d.view.state.is_terminal()).count();
            if unfinished + files.len() > MAX_QUEUED {
                return Err(SoulseekError::Limit(format!(
                    "the download queue holds at most {MAX_QUEUED} unfinished files"
                )));
            }
            let pending: HashSet<(String, String)> = items
                .iter()
                .filter(|d| !d.view.state.is_terminal())
                .map(|d| (d.view.username.clone(), d.view.path.clone()))
                .collect();
            let mut next_id = lock(&self.downloads.next_id);
            for (path, size, cover) in files {
                if pending.contains(&(peer.clone(), path.clone())) {
                    continue;
                }
                let id = *next_id;
                *next_id += 1;
                let mut download = new_download(id, peer.clone(), path, size, cover, None);
                if in_library.contains(&download.key) {
                    download.view.note =
                        Some("Already in the library; added to yours without downloading".into());
                    download.view.track_key = Some(download.key.clone());
                    download.set(DownloadState::Skipped);
                }
                items.push(download);
                queued += 1;
            }
            prune_finished(&mut items);
        }
        for key in &in_library {
            if let Some(entry) = self
                .ctx
                .library
                .add_owners(key, OutputFormat::Original, &owners)
                .await
            {
                self.downloads
                    .find_skipped(key, &entry.path.display().to_string());
            }
        }
        self.downloads.mark_dirty();
        self.downloads.wake.notify_one();
        tracing::info!(owner = %self.owner, %peer, queued, "Soulseek downloads queued");
        Ok(queued)
    }

    /// Queue `peer`'s file as a quality upgrade of `target`. Returns the
    /// download's id, or `None` if the file is already being downloaded.
    pub(super) async fn queue_upgrade(
        &self,
        peer: &str,
        path: &str,
        size: u64,
        target: UpgradeTarget,
    ) -> Result<Option<u64>, SoulseekError> {
        let peer = check_username(peer)?;
        check_path(path)?;
        if size > MAX_FILE_BYTES || !is_audio(path) {
            return Err(SoulseekError::InvalidInput(
                "not an audio file Inkult can add to the library".into(),
            ));
        }
        let id = {
            let mut items = lock(&self.downloads.items);
            let unfinished = items.iter().filter(|d| !d.view.state.is_terminal()).count();
            if unfinished >= MAX_QUEUED {
                return Err(SoulseekError::Limit(format!(
                    "the download queue holds at most {MAX_QUEUED} unfinished files"
                )));
            }
            if items.iter().any(|d| {
                !d.view.state.is_terminal() && d.view.username == peer && d.view.path == path
            }) {
                return Ok(None);
            }
            let mut next_id = lock(&self.downloads.next_id);
            let id = *next_id;
            *next_id += 1;
            items.push(new_download(
                id,
                peer.clone(),
                path.to_owned(),
                size,
                false,
                Some(target),
            ));
            prune_finished(&mut items);
            id
        };
        self.downloads.mark_dirty();
        self.downloads.wake.notify_one();
        tracing::info!(owner = %self.owner, %peer, "Soulseek quality upgrade queued");
        Ok(Some(id))
    }

    pub(super) fn download_view(&self, id: u64) -> Option<DownloadView> {
        self.downloads.with(id, |d| d.view.clone())
    }

    pub fn downloads(&self) -> Vec<DownloadView> {
        lock(&self.downloads.items)
            .iter()
            .rev()
            .map(|d| d.view.clone())
            .collect()
    }

    pub fn cancel_download(&self, id: u64) -> bool {
        let cancelled = self.downloads.with(id, |d| {
            // An import takes moments and either fully happens or not at
            // all; it is not interrupted halfway.
            if d.view.state.is_terminal() || d.view.state == DownloadState::Importing {
                return false;
            }
            d.cancel.cancel();
            d.view.note = None;
            d.view.place = None;
            d.set(DownloadState::Cancelled);
            true
        });
        if cancelled == Some(true) {
            self.downloads.mark_dirty();
            let path = self.downloads.with(id, |d| self.partial_path(&d.key));
            if let Some(path) = path {
                tokio::spawn(async move {
                    // Give an active transfer a moment to notice and close.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = tokio::fs::remove_file(path).await;
                });
            }
            return true;
        }
        false
    }

    pub fn retry_download(&self, id: u64) -> bool {
        let retried = self.downloads.with(id, |d| {
            if !matches!(
                d.view.state,
                DownloadState::Failed | DownloadState::Cancelled
            ) {
                return false;
            }
            d.cancel = CancellationToken::new();
            d.view.error = None;
            d.view.retryable = false;
            d.view.attempts = 0;
            d.view.note = None;
            d.next_attempt_ms = 0;
            d.set(DownloadState::Queued);
            true
        });
        if retried == Some(true) {
            self.downloads.mark_dirty();
            self.downloads.wake.notify_one();
            return true;
        }
        false
    }

    /// Remove finished downloads from the list (files stay in the library).
    pub fn clear_finished_downloads(&self) -> usize {
        let mut items = lock(&self.downloads.items);
        let before = items.len();
        items.retain(|d| !d.view.state.is_terminal());
        before - items.len()
    }

    pub(super) async fn save_downloads(&self) {
        let saved = self.downloads.saved();
        *lock(&self.downloads.dirty) = false;
        self.ctx
            .store
            .update(self.owner, |a| a.downloads = saved)
            .await;
    }

    /// Background work: contact peers, keep queue places fresh, notice
    /// stuck offers, save the queue.
    pub(super) async fn download_driver(self: Arc<Self>) {
        let _ = tokio::fs::create_dir_all(self.staging().join("covers")).await;
        loop {
            tokio::select! {
                _ = self.downloads.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = self.shutdown.cancelled() => return,
            }
            if *lock(&self.downloads.dirty) {
                self.save_downloads().await;
            }
            if !self.is_online() {
                continue;
            }
            let now = now_ms();
            let mut to_request = Vec::new();
            let mut to_poll = Vec::new();
            {
                let mut items = lock(&self.downloads.items);
                for d in items.iter_mut() {
                    match d.view.state {
                        DownloadState::Queued if d.next_attempt_ms <= now => {
                            if let Ok(permit) =
                                Arc::clone(&self.downloads.requesting).try_acquire_owned()
                            {
                                d.set(DownloadState::Requesting);
                                d.view.note = Some("Contacting the user".into());
                                to_request.push((d.view.id, permit));
                            }
                        }
                        DownloadState::Remote => {
                            if now.saturating_sub(d.last_queue_ms) > REQUEUE_EVERY_MS {
                                d.last_queue_ms = now;
                                d.last_place_ms = now;
                                to_poll.push((d.view.username.clone(), d.view.path.clone(), true));
                            } else if now.saturating_sub(d.last_place_ms) > PLACE_EVERY_MS {
                                d.last_place_ms = now;
                                to_poll.push((d.view.username.clone(), d.view.path.clone(), false));
                            }
                        }
                        DownloadState::Offered
                            if now.saturating_sub(d.offered_at_ms) > OFFER_TIMEOUT_MS =>
                        {
                            d.token = None;
                            d.view.note = Some(
                                "The upload did not start; waiting for the user to try again"
                                    .into(),
                            );
                            d.last_place_ms = now;
                            d.set(DownloadState::Remote);
                        }
                        _ => {}
                    }
                }
            }
            for (id, permit) in to_request {
                tokio::spawn(Arc::clone(&self).request_download(id, permit));
            }
            for (peer, path, requeue) in to_poll {
                let session = Arc::clone(&self);
                tokio::spawn(async move {
                    if requeue {
                        let _ = session
                            .send_to(
                                &peer,
                                PeerMessage::QueueUpload {
                                    filename: path.clone(),
                                },
                            )
                            .await;
                    }
                    let _ = session
                        .send_to(&peer, PeerMessage::PlaceInQueueRequest { filename: path })
                        .await;
                });
            }
        }
    }

    /// Ask the peer to queue the file for us.
    async fn request_download(self: Arc<Self>, id: u64, _permit: OwnedSemaphorePermit) {
        let Some((peer, path, key, size)) = self.downloads.with(id, |d| {
            (
                d.view.username.clone(),
                d.view.path.clone(),
                d.key.clone(),
                d.view.size,
            )
        }) else {
            return;
        };
        // A complete partial file (e.g. the import was interrupted by a
        // restart) needs no transfer.
        let partial = self.partial_path(&key);
        if size > 0
            && tokio::fs::metadata(&partial)
                .await
                .is_ok_and(|m| m.len() == size)
        {
            self.downloads.with(id, |d| d.set(DownloadState::Importing));
            tokio::spawn(Arc::clone(&self).finish_download(id));
            return;
        }
        let result = async {
            let conn = self.peer(&peer).await?;
            conn.send_wait(PeerMessage::QueueUpload {
                filename: path.clone(),
            })
            .await?;
            conn.send_wait(PeerMessage::PlaceInQueueRequest { filename: path })
                .await
        }
        .await;
        let now = now_ms();
        self.downloads.with(id, |d| {
            if d.view.state != DownloadState::Requesting {
                return; // cancelled, or the peer already offered it
            }
            match &result {
                Ok(()) => {
                    d.last_queue_ms = now;
                    d.last_place_ms = now;
                    d.view.error = None;
                    d.view.note = Some("Waiting in the user's upload queue".into());
                    d.set(DownloadState::Remote);
                }
                Err(e) => {
                    let attempt = d.view.attempts as usize;
                    d.view.attempts += 1;
                    match RETRY_DELAYS.get(attempt) {
                        Some(delay) => {
                            d.next_attempt_ms = now + delay.as_millis() as u64;
                            d.view.note =
                                Some(format!("{e}; trying again in {}", human_delay(*delay)));
                            d.set(DownloadState::Queued);
                        }
                        None => d.fail(e.to_string(), true),
                    }
                }
            }
        });
        if result.is_err() {
            self.downloads.mark_dirty();
        }
    }

    /// The peer opened a file connection for transfer `token`.
    pub(super) async fn receive_file(
        self: Arc<Self>,
        peer: String,
        token: u32,
        mut stream: TcpStream,
        _permit: OwnedSemaphorePermit,
    ) {
        let found = lock(&self.downloads.items)
            .iter_mut()
            .find(|d| {
                d.view.username == peer
                    && d.token == Some(token)
                    && d.view.state == DownloadState::Offered
            })
            .map(|d| {
                d.view.note = None;
                d.set(DownloadState::Transferring);
                (d.view.id, d.key.clone(), d.view.size, d.cancel.clone())
            });
        let Some((id, key, size, cancel)) = found else {
            tracing::debug!(%peer, token, "file connection for an unknown transfer");
            return;
        };
        let partial = self.partial_path(&key);
        let result = tokio::select! {
            result = self.transfer(id, &partial, size, &mut stream) => result,
            _ = cancel.cancelled() => return,
            _ = self.shutdown.cancelled() => return,
        };
        // Closing the connection is the downloader's job and tells the
        // peer the transfer is over.
        let _ = stream.shutdown().await;
        match result {
            Ok(()) => {
                tracing::info!(%peer, id, "Soulseek transfer complete");
                self.downloads.with(id, |d| d.set(DownloadState::Importing));
                self.finish_download(id).await;
            }
            Err(e) => {
                tracing::info!(%peer, id, error = %e, "Soulseek transfer interrupted");
                self.downloads.with(id, |d| {
                    let attempt = d.view.attempts as usize;
                    d.view.attempts += 1;
                    d.token = None;
                    match RETRY_DELAYS.get(attempt) {
                        Some(delay) => {
                            d.next_attempt_ms = now_ms() + delay.as_millis() as u64;
                            d.view.note = Some(format!(
                                "Transfer interrupted ({e}); resuming in {}",
                                human_delay(*delay)
                            ));
                            d.set(DownloadState::Queued);
                        }
                        None => d.fail(format!("transfer kept failing: {e}"), true),
                    }
                });
                self.downloads.mark_dirty();
            }
        }
    }

    /// Receive the file data into the partial file, resuming at its length.
    async fn transfer(
        &self,
        id: u64,
        partial: &std::path::Path,
        size: u64,
        stream: &mut TcpStream,
    ) -> Result<(), String> {
        if let Some(dir) = partial.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| format!("cannot create staging folder: {e}"))?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(partial)
            .await
            .map_err(|e| format!("cannot write the download: {e}"))?;
        let mut offset = file
            .metadata()
            .await
            .map(|m| m.len())
            .map_err(|e| e.to_string())?;
        if offset > size {
            // Not a prefix of this file (it changed at the peer): start over.
            file.set_len(0).await.map_err(|e| e.to_string())?;
            offset = 0;
        }
        stream
            .write_all(&offset.to_le_bytes())
            .await
            .map_err(|e| format!("connection lost: {e}"))?;

        let mut received = offset;
        let mut buf = vec![0u8; 64 * 1024];
        let mut window_start = tokio::time::Instant::now();
        let mut window_bytes = 0u64;
        self.downloads.with(id, |d| d.view.bytes = received);
        while received < size {
            let want = buf.len().min((size - received) as usize);
            let n = tokio::time::timeout(STALL_TIMEOUT, stream.read(&mut buf[..want]))
                .await
                .map_err(|_| "no data for 90 s".to_owned())?
                .map_err(|e| format!("connection lost: {e}"))?;
            if n == 0 {
                return Err(format!(
                    "the user closed the connection at {}%",
                    received * 100 / size.max(1)
                ));
            }
            file.write_all(&buf[..n])
                .await
                .map_err(|e| format!("cannot write the download: {e}"))?;
            received += n as u64;
            window_bytes += n as u64;
            let elapsed = window_start.elapsed();
            if elapsed >= Duration::from_millis(500) {
                let speed = window_bytes as f64 / elapsed.as_secs_f64();
                self.downloads.with(id, |d| {
                    d.view.bytes = received;
                    d.view.speed_bytes_per_sec = Some(speed);
                    d.touch();
                });
                window_start = tokio::time::Instant::now();
                window_bytes = 0;
            }
        }
        file.sync_all()
            .await
            .map_err(|e| format!("cannot write the download: {e}"))?;
        self.downloads.with(id, |d| d.view.bytes = received);
        Ok(())
    }

    /// Move a completed file where it belongs: covers aside for their
    /// folder, audio through import into the library.
    async fn finish_download(self: Arc<Self>, id: u64) {
        let Some((peer, path, key, cover, upgrade, cancel)) = self.downloads.with(id, |d| {
            (
                d.view.username.clone(),
                d.view.path.clone(),
                d.key.clone(),
                d.view.cover,
                d.upgrade.clone(),
                d.cancel.clone(),
            )
        }) else {
            return;
        };
        let partial = self.partial_path(&key);
        if cover {
            let target = self.cover_path(&peer, &path);
            let moved = match target.parent() {
                Some(dir) => match tokio::fs::create_dir_all(dir).await {
                    Ok(()) => tokio::fs::rename(&partial, &target).await,
                    Err(e) => Err(e),
                },
                None => Ok(()),
            };
            self.downloads.with(id, |d| match moved {
                Ok(()) => {
                    d.view.note = Some("Folder artwork, embedded into the folder's tracks".into());
                    d.set(DownloadState::Completed);
                }
                Err(e) => d.fail(format!("cannot keep the cover image: {e}"), false),
            });
            self.downloads.mark_dirty();
            return;
        }

        if let Some(target) = upgrade {
            let request = UpgradeImport {
                peer,
                path,
                staged: partial.clone(),
                target,
                max_track_length: self.ctx.config.max_track_length.get(),
                owners: BTreeSet::from([self.owner]),
            };
            let result = import::import_upgrade(&self.ctx.library, &self.ctx.ffmpeg, request).await;
            self.record_import(id, result);
            let _ = tokio::fs::remove_file(&partial).await;
            self.downloads.mark_dirty();
            return;
        }

        self.wait_for_cover(&peer, &path, &cancel).await;
        if cancel.is_cancelled() {
            return;
        }
        let cover_path = self.cover_path(&peer, &path);
        let cover = tokio::fs::try_exists(&cover_path)
            .await
            .unwrap_or(false)
            .then_some(cover_path);
        let request = ImportRequest {
            peer: peer.clone(),
            path: path.clone(),
            key,
            staged: partial.clone(),
            folder_cover: cover,
            max_track_length: self.ctx.config.max_track_length.get(),
            owners: BTreeSet::from([self.owner]),
        };
        let result = import::import(&self.ctx.library, &self.ctx.ffmpeg, request).await;
        self.record_import(id, result);
        // The staged file is gone after a successful import; after a failed
        // one it is unusable, so never resume from it.
        let _ = tokio::fs::remove_file(&partial).await;
        self.downloads.mark_dirty();
    }

    fn record_import(&self, id: u64, result: Result<import::ImportOutcome, import::ImportError>) {
        self.downloads.with(id, |d| match result {
            Ok(outcome) => {
                d.view.track_key = Some(outcome.entry.key.clone());
                d.view.destination = Some(outcome.entry.path.display().to_string());
                d.view.quality = Some(outcome.entry.quality.clone());
                d.view.artwork = Some(outcome.artwork.to_owned());
                d.view.inferred = outcome.inferred;
                d.view.note = outcome.note;
                d.set(if outcome.already_present {
                    DownloadState::Skipped
                } else {
                    DownloadState::Completed
                });
            }
            Err(e) => d.fail(e.to_string(), false),
        });
    }

    /// If the cover of this track's folder is being transferred right now,
    /// give it a little time so the track gets its artwork.
    async fn wait_for_cover(&self, peer: &str, path: &str, cancel: &CancellationToken) {
        let (dir, _) = split_path(path);
        let deadline = tokio::time::Instant::now() + COVER_WAIT;
        loop {
            let pending = lock(&self.downloads.items).iter().any(|d| {
                d.view.cover
                    && d.view.username == peer
                    && split_path(&d.view.path).0 == dir
                    && matches!(
                        d.view.state,
                        DownloadState::Offered | DownloadState::Transferring
                    )
            });
            if !pending || tokio::time::Instant::now() >= deadline || cancel.is_cancelled() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

impl Downloads {
    fn find_skipped(&self, key: &TrackKey, destination: &str) {
        for d in lock(&self.items)
            .iter_mut()
            .filter(|d| &d.key == key && d.view.state == DownloadState::Skipped)
        {
            d.view.destination = Some(destination.to_owned());
        }
    }
}

fn prune_finished(items: &mut Vec<Download>) {
    let finished = items.iter().filter(|d| d.view.state.is_terminal()).count();
    let mut excess = finished.saturating_sub(MAX_FINISHED);
    items.retain(|d| {
        if excess > 0 && d.view.state.is_terminal() {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

fn human_delay(delay: Duration) -> String {
    let secs = delay.as_secs();
    if secs < 120 {
        format!("{secs} s")
    } else {
        format!("{} min", secs / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soulseek::messages::{FileAttrs, SharedFile};

    fn file(name: &str, size: u64) -> SharedFile {
        SharedFile {
            name: name.into(),
            size,
            attrs: FileAttrs::default(),
        }
    }

    #[test]
    fn selects_audio_and_best_cover_per_folder() {
        let dirs = vec![
            SharedDir {
                path: "@@music\\Artist\\Album".into(),
                files: vec![
                    file("01 - One.flac", 30),
                    file("02 - Two.flac", 31),
                    file("scan.jpg", 900),
                    file("Folder.jpg", 100),
                    file("album.nfo", 1),
                    file("album.cue", 1),
                ],
            },
            SharedDir {
                path: "@@music\\Artist\\Album\\Scans".into(),
                files: vec![file("front.jpg", 5000)],
            },
        ];
        let picked = folder_selection(&dirs);
        assert_eq!(
            picked,
            vec![
                ("@@music\\Artist\\Album\\Folder.jpg".to_owned(), 100, true),
                (
                    "@@music\\Artist\\Album\\01 - One.flac".to_owned(),
                    30,
                    false
                ),
                (
                    "@@music\\Artist\\Album\\02 - Two.flac".to_owned(),
                    31,
                    false
                ),
            ]
        );
    }

    #[test]
    fn offers_are_accepted_only_for_wanted_files() {
        let downloads = Downloads::new(&[SavedDownload {
            peer: "alice".into(),
            path: "a\\b.flac".into(),
            size: 10,
            cover: false,
            upgrade: None,
        }]);
        let refuse = downloads.offered("alice", 1, "a\\other.flac", Some(10));
        assert!(matches!(
            refuse,
            PeerMessage::TransferResponse { allowed: false, .. }
        ));
        // Another user offering the same path is not who we asked.
        let refuse = downloads.offered("mallory", 2, "a\\b.flac", Some(10));
        assert!(matches!(
            refuse,
            PeerMessage::TransferResponse { allowed: false, .. }
        ));
        let accept = downloads.offered("alice", 3, "a\\b.flac", Some(12));
        assert!(matches!(
            accept,
            PeerMessage::TransferResponse { allowed: true, .. }
        ));
        let items = lock(&downloads.items);
        assert_eq!(items[0].view.state, DownloadState::Offered);
        assert_eq!(items[0].view.size, 12);
        assert_eq!(items[0].token, Some(3));
    }

    #[test]
    fn denials_distinguish_temporary_from_final() {
        let saved = [
            SavedDownload {
                peer: "p".into(),
                path: "x\\1.mp3".into(),
                size: 1,
                cover: false,
                upgrade: None,
            },
            SavedDownload {
                peer: "p".into(),
                path: "x\\2.mp3".into(),
                size: 1,
                cover: false,
                upgrade: None,
            },
        ];
        let downloads = Downloads::new(&saved);
        downloads.denied("p", "x\\1.mp3", "Too many files");
        downloads.denied("p", "x\\2.mp3", "File not shared.");
        let items = lock(&downloads.items);
        assert!(items[0].view.retryable);
        assert!(!items[1].view.retryable);
        assert_eq!(items[1].view.state, DownloadState::Failed);
    }

    #[test]
    fn keys_identify_user_and_path() {
        assert_eq!(file_key("a", "x\\y.flac"), file_key("a", "x\\y.flac"));
        assert_ne!(file_key("a", "x\\y.flac"), file_key("b", "x\\y.flac"));
        assert_eq!(file_key("a", "x").provider, ProviderKind::Soulseek);
    }
}
