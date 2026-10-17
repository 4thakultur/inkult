//! Sharing: other Soulseek users browsing and downloading from us.
//!
//! Off by default. When a user turns it on, the tracks in *their* Inkult
//! library (the files they own, nobody else's) are shared read-only under
//! a virtual `inkult\…` folder; real filesystem paths are never revealed and
//! only files in that index can ever be read. Many Soulseek users refuse to
//! serve people who share nothing, so sharing also helps one's own
//! downloads.
//!
//! Uploads run through a queue with `INKULT_SOULSEEK_UPLOAD_SLOTS` slots.
//! Per protocol, we tell the peer when its turn comes (`TransferRequest`),
//! it accepts, and we open a file connection, send the token, learn the
//! offset it already has, and send the rest.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::messages::{
    ConnKind, FileAttrs, PeerMessage, SearchReply, ServerRequest, SharedDir, SharedFile, UserInfo,
};
use super::peer::PeerConn;
use super::session::{Session, Waiters};
use super::{SoulseekError, now_ms};
use crate::jobs::lock;

/// Root of the virtual folder our shares appear under.
const SHARE_ROOT: &str = "inkult";
/// A share index older than this is rebuilt before it is used.
const INDEX_MAX_AGE_MS: u64 = 5 * 60 * 1000;
const MAX_QUEUED_PER_USER: usize = 100;
const MAX_QUEUED: usize = 1000;
const MAX_FINISHED: usize = 200;
const MAX_SEARCH_RESULTS: usize = 100;
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
const OFFSET_TIMEOUT: Duration = Duration::from_secs(30);
const STALL_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadState {
    Queued,
    Starting,
    Transferring,
    Completed,
    Failed,
    Cancelled,
}

impl UploadState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            UploadState::Completed | UploadState::Failed | UploadState::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadView {
    pub id: u64,
    pub username: String,
    pub path: String,
    pub size: u64,
    pub state: UploadState,
    pub bytes: u64,
    pub speed_bytes_per_sec: Option<f64>,
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

struct Upload {
    view: UploadView,
    file: PathBuf,
    cancel: CancellationToken,
}

#[derive(Default)]
struct ShareIndex {
    built_at_ms: u64,
    dirs: Vec<SharedDir>,
    /// Virtual path → real file and size.
    files: HashMap<String, (PathBuf, u64)>,
}

pub(super) struct Uploads {
    sharing: AtomicBool,
    index: Mutex<Arc<ShareIndex>>,
    items: Mutex<Vec<Upload>>,
    next_id: Mutex<u64>,
    wake: Notify,
    /// Peers' answers to our TransferRequests, by (user, token).
    answers: Waiters<(String, u32), (bool, Option<String>)>,
    completed: Mutex<u32>,
}

impl Uploads {
    pub fn new(sharing: bool) -> Self {
        Self {
            sharing: AtomicBool::new(sharing),
            index: Mutex::new(Arc::new(ShareIndex::default())),
            items: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
            wake: Notify::new(),
            answers: Waiters::new(),
            completed: Mutex::new(0),
        }
    }

    pub fn sharing(&self) -> bool {
        self.sharing.load(Ordering::Relaxed)
    }

    /// (folders, files) currently shared.
    pub fn share_counts(&self) -> (usize, usize) {
        if !self.sharing() {
            return (0, 0);
        }
        let index = lock(&self.index);
        (index.dirs.len(), index.files.len())
    }

    pub fn active_count(&self) -> usize {
        lock(&self.items)
            .iter()
            .filter(|u| !u.view.state.is_terminal())
            .count()
    }

    pub fn answered(&self, username: &str, token: u32, allowed: bool, reason: Option<String>) {
        self.answers
            .resolve(&(username.to_owned(), token), (allowed, reason));
    }

    /// 1-based place of a queued file in our upload queue.
    pub fn place(&self, username: &str, path: &str) -> Option<u32> {
        let items = lock(&self.items);
        let queued: Vec<&Upload> = items
            .iter()
            .filter(|u| u.view.state == UploadState::Queued)
            .collect();
        let index = queued
            .iter()
            .position(|u| u.view.username == username && u.view.path == path)?;
        Some(index as u32 + 1)
    }

    fn with<T>(&self, id: u64, f: impl FnOnce(&mut Upload) -> T) -> Option<T> {
        lock(&self.items)
            .iter_mut()
            .find(|u| u.view.id == id)
            .map(f)
    }

    fn set_state(&self, id: u64, state: UploadState, error: Option<String>) {
        self.with(id, |u| {
            u.view.state = state;
            if error.is_some() {
                u.view.error = error;
            }
            if state != UploadState::Transferring {
                u.view.speed_bytes_per_sec = None;
            }
            u.view.updated_at_ms = now_ms();
        });
    }
}

/// Library-relative path → the virtual path peers see.
fn virtual_path(relative: &std::path::Path) -> String {
    let mut out = String::from(SHARE_ROOT);
    for component in relative.components() {
        out.push('\\');
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
}

/// Whether every query word occurs in `path` and no excluded (`-word`) does.
fn matches_query(path: &str, query: &str) -> bool {
    let path = path.to_lowercase();
    let mut any = false;
    for word in query.to_lowercase().split_whitespace() {
        match word.strip_prefix('-') {
            Some(excluded) if !excluded.is_empty() => {
                if path.contains(excluded) {
                    return false;
                }
            }
            _ => {
                if !path.contains(word.trim_matches('*')) {
                    return false;
                }
                any = true;
            }
        }
    }
    any
}

impl Session {
    /// Turn sharing of the user's library on or off.
    pub async fn set_sharing(self: &Arc<Self>, share: bool) {
        self.uploads.sharing.store(share, Ordering::Relaxed);
        self.ctx
            .store
            .update(self.owner, |a| a.share_library = share)
            .await;
        if !share {
            let denied: Vec<(String, String)> = {
                let mut items = lock(&self.uploads.items);
                items
                    .iter_mut()
                    .filter(|u| !u.view.state.is_terminal())
                    .map(|u| {
                        u.cancel.cancel();
                        u.view.state = UploadState::Cancelled;
                        u.view.updated_at_ms = now_ms();
                        (u.view.username.clone(), u.view.path.clone())
                    })
                    .collect()
            };
            for (username, filename) in denied {
                let session = Arc::clone(self);
                tokio::spawn(async move {
                    let _ = session
                        .send_to(
                            &username,
                            PeerMessage::UploadDenied {
                                filename,
                                reason: "Cancelled".into(),
                            },
                        )
                        .await;
                });
            }
        }
        self.refresh_shares().await;
    }

    /// Rebuild the share index from the user's library and tell the server
    /// the new counts.
    pub(super) async fn refresh_shares(&self) {
        let index = if self.uploads.sharing() {
            self.build_index().await
        } else {
            ShareIndex {
                built_at_ms: now_ms(),
                ..ShareIndex::default()
            }
        };
        *lock(&self.uploads.index) = Arc::new(index);
        self.announce_shares();
    }

    async fn build_index(&self) -> ShareIndex {
        let library = &self.ctx.library;
        let mut dirs: HashMap<String, Vec<SharedFile>> = HashMap::new();
        let mut files = HashMap::new();
        for entry in library.entries().await {
            if !entry.owners.contains(&self.owner) {
                continue;
            }
            let Some(real) = library.file_path(&entry).await else {
                continue;
            };
            let Ok(meta) = tokio::fs::metadata(&real).await else {
                continue;
            };
            let virtual_file = virtual_path(&entry.path);
            let (dir, name) = super::split_path(&virtual_file);
            let quality = &entry.quality;
            let shared = SharedFile {
                name: name.to_owned(),
                size: meta.len(),
                attrs: FileAttrs {
                    bitrate_kbps: quality.bitrate_kbps.filter(|_| !quality.lossless),
                    duration_secs: None,
                    vbr: None,
                    sample_rate_hz: quality.sample_rate_hz,
                    bit_depth: quality
                        .bit_depth
                        .filter(|_| quality.lossless)
                        .map(u32::from),
                },
            };
            dirs.entry(dir.to_owned()).or_default().push(shared);
            files.insert(virtual_file, (real, meta.len()));
        }
        let mut dirs: Vec<SharedDir> = dirs
            .into_iter()
            .map(|(path, mut files)| {
                files.sort_by(|a, b| a.name.cmp(&b.name));
                SharedDir { path, files }
            })
            .collect();
        dirs.sort_by(|a, b| a.path.cmp(&b.path));
        ShareIndex {
            built_at_ms: now_ms(),
            dirs,
            files,
        }
    }

    async fn share_index(&self) -> Arc<ShareIndex> {
        let current = Arc::clone(&lock(&self.uploads.index));
        if self.uploads.sharing() && now_ms().saturating_sub(current.built_at_ms) > INDEX_MAX_AGE_MS
        {
            self.refresh_shares().await;
            return Arc::clone(&lock(&self.uploads.index));
        }
        current
    }

    pub(super) async fn send_share_list(self: Arc<Self>, conn: PeerConn, username: String) {
        let index = self.share_index().await;
        tracing::debug!(peer = %username, dirs = index.dirs.len(), "sending our share list");
        let dirs = index.dirs.clone();
        // Compressing a large list is CPU work.
        let frame =
            tokio::task::spawn_blocking(move || PeerMessage::SharedFileList(dirs).encode()).await;
        if let Ok(frame) = frame {
            let _ = conn.send_frame(frame).await;
        }
    }

    pub(super) async fn send_folder_contents(
        self: Arc<Self>,
        conn: PeerConn,
        token: u32,
        folder: String,
    ) {
        let index = self.share_index().await;
        let prefix = format!("{folder}\\");
        let dirs: Vec<SharedDir> = index
            .dirs
            .iter()
            .filter(|d| d.path == folder || d.path.starts_with(&prefix))
            .cloned()
            .collect();
        let _ = conn
            .send_wait(PeerMessage::FolderContents {
                token,
                folder,
                dirs,
            })
            .await;
    }

    pub(super) fn send_user_info(&self, conn: &PeerConn) {
        let (queued, active) = {
            let items = lock(&self.uploads.items);
            (
                items
                    .iter()
                    .filter(|u| u.view.state == UploadState::Queued)
                    .count(),
                items
                    .iter()
                    .filter(|u| {
                        matches!(
                            u.view.state,
                            UploadState::Starting | UploadState::Transferring
                        )
                    })
                    .count(),
            )
        };
        conn.send(PeerMessage::UserInfo(UserInfo {
            description: "Digging with Inkult, a self-hosted music library.".into(),
            picture: None,
            total_uploads: *lock(&self.uploads.completed),
            queue_size: queued as u32,
            slots_free: self.uploads.sharing() && active < self.ctx.config.upload_slots,
        }));
    }

    /// Someone searched our shares (a user search). Answer if we share.
    pub(super) async fn answer_user_search(
        self: Arc<Self>,
        username: String,
        token: u32,
        query: String,
    ) {
        if !self.uploads.sharing() {
            return;
        }
        let index = self.share_index().await;
        let mut files: Vec<SharedFile> = index
            .dirs
            .iter()
            .flat_map(|d| {
                d.files.iter().map(move |f| SharedFile {
                    name: format!("{}\\{}", d.path, f.name),
                    size: f.size,
                    attrs: f.attrs.clone(),
                })
            })
            .filter(|f| matches_query(&f.name, &query))
            .take(MAX_SEARCH_RESULTS)
            .collect();
        if files.is_empty() {
            return;
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        let queued = lock(&self.uploads.items)
            .iter()
            .filter(|u| u.view.state == UploadState::Queued)
            .count();
        let reply = SearchReply {
            username: self.username().to_owned(),
            token,
            files,
            slot_free: queued == 0,
            avg_speed: 0,
            queue_length: queued as u32,
        };
        let _ = self
            .send_to(&username, PeerMessage::SearchReply(reply))
            .await;
    }

    /// A peer asks to download `path` from us. `Err` carries the refusal
    /// reason to send back.
    pub(super) fn queue_upload_request(&self, username: &str, path: &str) -> Result<(), String> {
        const NOT_SHARED: &str = "File not shared.";
        if !self.uploads.sharing() {
            return Err(NOT_SHARED.into());
        }
        let Some((file, size)) = lock(&self.uploads.index).files.get(path).cloned() else {
            return Err(NOT_SHARED.into());
        };
        let mut items = lock(&self.uploads.items);
        let pending: Vec<&Upload> = items
            .iter()
            .filter(|u| !u.view.state.is_terminal())
            .collect();
        if pending
            .iter()
            .any(|u| u.view.username == username && u.view.path == path)
        {
            return Ok(());
        }
        if pending
            .iter()
            .filter(|u| u.view.username == username)
            .count()
            >= MAX_QUEUED_PER_USER
        {
            return Err("Too many files".into());
        }
        if pending.len() >= MAX_QUEUED {
            return Err("Too many files".into());
        }
        let mut next_id = lock(&self.uploads.next_id);
        let now = now_ms();
        items.push(Upload {
            view: UploadView {
                id: *next_id,
                username: username.to_owned(),
                path: path.to_owned(),
                size,
                state: UploadState::Queued,
                bytes: 0,
                speed_bytes_per_sec: None,
                error: None,
                created_at_ms: now,
                updated_at_ms: now,
            },
            file,
            cancel: CancellationToken::new(),
        });
        *next_id += 1;
        let finished = items.iter().filter(|u| u.view.state.is_terminal()).count();
        let mut excess = finished.saturating_sub(MAX_FINISHED);
        items.retain(|u| {
            if excess > 0 && u.view.state.is_terminal() {
                excess -= 1;
                false
            } else {
                true
            }
        });
        drop(items);
        self.uploads.wake.notify_one();
        tracing::info!(peer = %username, "upload queued");
        Ok(())
    }

    pub fn uploads(&self) -> Vec<UploadView> {
        lock(&self.uploads.items)
            .iter()
            .rev()
            .map(|u| u.view.clone())
            .collect()
    }

    pub fn cancel_upload(self: &Arc<Self>, id: u64) -> bool {
        let cancelled = self.uploads.with(id, |u| {
            if u.view.state.is_terminal() {
                return None;
            }
            u.cancel.cancel();
            u.view.state = UploadState::Cancelled;
            u.view.updated_at_ms = now_ms();
            Some((u.view.username.clone(), u.view.path.clone()))
        });
        let Some(Some((username, filename))) = cancelled else {
            return false;
        };
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let _ = session
                .send_to(
                    &username,
                    PeerMessage::UploadDenied {
                        filename,
                        reason: "Cancelled".into(),
                    },
                )
                .await;
        });
        true
    }

    pub fn clear_finished_uploads(&self) -> usize {
        let mut items = lock(&self.uploads.items);
        let before = items.len();
        items.retain(|u| !u.view.state.is_terminal());
        before - items.len()
    }

    pub(super) async fn upload_driver(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.uploads.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = self.shutdown.cancelled() => return,
            }
            if !self.is_online() || !self.uploads.sharing() {
                continue;
            }
            let next = {
                let mut items = lock(&self.uploads.items);
                let active_users: HashSet<String> = items
                    .iter()
                    .filter(|u| {
                        matches!(
                            u.view.state,
                            UploadState::Starting | UploadState::Transferring
                        )
                    })
                    .map(|u| u.view.username.clone())
                    .collect();
                if active_users.len() >= self.ctx.config.upload_slots {
                    None
                } else {
                    // One slot per user at a time, oldest request first.
                    items
                        .iter_mut()
                        .find(|u| {
                            u.view.state == UploadState::Queued
                                && !active_users.contains(&u.view.username)
                        })
                        .map(|u| {
                            u.view.state = UploadState::Starting;
                            u.view.updated_at_ms = now_ms();
                            u.view.id
                        })
                }
            };
            if let Some(id) = next {
                tokio::spawn(Arc::clone(&self).run_upload(id));
                self.uploads.wake.notify_one();
            }
        }
    }

    async fn run_upload(self: Arc<Self>, id: u64) {
        let Some((username, path, file, size, cancel)) = self.uploads.with(id, |u| {
            (
                u.view.username.clone(),
                u.view.path.clone(),
                u.file.clone(),
                u.view.size,
                u.cancel.clone(),
            )
        }) else {
            return;
        };
        let result = tokio::select! {
            result = self.send_file(id, &username, &path, &file, size) => result,
            _ = cancel.cancelled() => return,
            _ = self.shutdown.cancelled() => return,
        };
        match result {
            Ok(speed) => {
                tracing::info!(peer = %username, "upload complete");
                *lock(&self.uploads.completed) += 1;
                self.set_completed(id);
                let _ = self.send_server(ServerRequest::SendUploadSpeed(speed));
            }
            Err(e) => {
                tracing::info!(peer = %username, error = %e, "upload failed");
                self.uploads
                    .set_state(id, UploadState::Failed, Some(e.to_string()));
                // Lets the peer re-queue the file on its side.
                let _ = self
                    .send_to(&username, PeerMessage::UploadFailed { filename: path })
                    .await;
            }
        }
        self.uploads.wake.notify_one();
    }

    fn set_completed(&self, id: u64) {
        self.uploads.set_state(id, UploadState::Completed, None);
    }

    /// Returns the average speed in bytes per second.
    async fn send_file(
        self: &Arc<Self>,
        id: u64,
        username: &str,
        path: &str,
        file: &std::path::Path,
        size: u64,
    ) -> Result<u32, SoulseekError> {
        let token = self.next_token();
        let answer = self.uploads.answers.wait((username.to_owned(), token));
        self.send_to(
            username,
            PeerMessage::TransferRequest {
                direction: 1,
                token,
                filename: path.to_owned(),
                size: Some(size),
            },
        )
        .await?;
        let (allowed, reason) = tokio::time::timeout(ANSWER_TIMEOUT, answer)
            .await
            .map_err(|_| SoulseekError::PeerTimeout {
                username: username.to_owned(),
            })?
            .map_err(|_| SoulseekError::Offline)?;
        if !allowed {
            return Err(SoulseekError::Peer(format!(
                "{username} declined: {}",
                reason.unwrap_or_else(|| "no reason given".into())
            )));
        }
        let mut stream = self.open_connection(username, ConnKind::File).await?;
        let io = |e: std::io::Error| SoulseekError::Peer(format!("connection lost: {e}"));
        stream.write_all(&token.to_le_bytes()).await.map_err(io)?;
        let offset = tokio::time::timeout(OFFSET_TIMEOUT, stream.read_u64_le())
            .await
            .map_err(|_| SoulseekError::PeerTimeout {
                username: username.to_owned(),
            })?
            .map_err(io)?;
        if offset > size {
            return Err(SoulseekError::Peer(format!(
                "{username} asked for an offset beyond the end of the file"
            )));
        }
        let mut source = tokio::fs::File::open(file)
            .await
            .map_err(|e| SoulseekError::Peer(format!("cannot read the shared file: {e}")))?;
        source
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(io)?;
        self.uploads.with(id, |u| {
            u.view.state = UploadState::Transferring;
            u.view.bytes = offset;
            u.view.updated_at_ms = now_ms();
        });

        let started = tokio::time::Instant::now();
        let mut sent = offset;
        let mut buf = vec![0u8; 64 * 1024];
        let mut last_update = tokio::time::Instant::now();
        while sent < size {
            let want = buf.len().min((size - sent) as usize);
            let n = source.read(&mut buf[..want]).await.map_err(io)?;
            if n == 0 {
                return Err(SoulseekError::Peer("the shared file got shorter".into()));
            }
            tokio::time::timeout(STALL_TIMEOUT, stream.write_all(&buf[..n]))
                .await
                .map_err(|_| SoulseekError::PeerTimeout {
                    username: username.to_owned(),
                })?
                .map_err(io)?;
            sent += n as u64;
            if last_update.elapsed() >= Duration::from_millis(500) {
                let elapsed = started.elapsed().as_secs_f64().max(0.001);
                self.uploads.with(id, |u| {
                    u.view.bytes = sent;
                    u.view.speed_bytes_per_sec = Some((sent - offset) as f64 / elapsed);
                    u.view.updated_at_ms = now_ms();
                });
                last_update = tokio::time::Instant::now();
            }
        }
        self.uploads.with(id, |u| u.view.bytes = sent);
        // The downloader closes the connection once it has everything.
        let mut rest = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(60), stream.read(&mut rest)).await;
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        Ok(((sent - offset) as f64 / elapsed).min(f64::from(u32::MAX)) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_paths_hide_the_real_location() {
        assert_eq!(
            virtual_path(std::path::Path::new("Bicep/Isles/03 - Apricots.flac")),
            "inkult\\Bicep\\Isles\\03 - Apricots.flac"
        );
    }

    #[test]
    fn query_matching() {
        let path = "inkult\\Bicep\\Isles\\03 - Apricots.flac";
        assert!(matches_query(path, "bicep apricots"));
        assert!(matches_query(path, "BICEP"));
        assert!(!matches_query(path, "bicep glue"));
        assert!(!matches_query(path, "bicep -isles"));
        assert!(!matches_query(path, ""));
        assert!(!matches_query(path, "-glue"));
    }
}
