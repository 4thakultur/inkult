//! Browsing a user's shares, user info, folder listings and private chat.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use super::messages::{PeerMessage, ServerRequest, SharedDir, SharedFile, UserStatus, peer_code};
use super::session::Session;
use super::{MAX_MESSAGE_CHARS, SoulseekError, check_username, now_ms};
use crate::jobs::lock;
use crate::metadata::clean_multiline;

/// Share lists can be large; only the most recent few are kept.
const MAX_CACHED_BROWSES: usize = 3;
const BROWSE_TIMEOUT: Duration = Duration::from_secs(180);
/// A cached share list younger than this is shown instead of asking again.
const BROWSE_FRESH_MS: u64 = 10 * 60 * 1000;
const FOLDER_TIMEOUT: Duration = Duration::from_secs(60);
const INFO_TIMEOUT: Duration = Duration::from_secs(20);
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PICTURE_BYTES: usize = 1024 * 1024;
const MAX_CONVERSATIONS: usize = 100;
const MAX_MESSAGES: usize = 300;

// ---------- browse ----------

enum BrowseState {
    Loading {
        since_ms: u64,
    },
    Ready {
        dirs: Arc<Vec<SharedDir>>,
        at_ms: u64,
    },
    Failed {
        error: String,
        at_ms: u64,
    },
}

#[derive(Default)]
pub(super) struct Browses {
    map: Mutex<HashMap<String, BrowseState>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirSummary {
    pub path: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowseStatus {
    Loading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowseView {
    pub username: String,
    pub status: BrowseStatus,
    pub error: Option<String>,
    pub received_at_ms: Option<u64>,
    pub total_files: usize,
    pub dirs: Vec<DirSummary>,
}

impl Browses {
    /// A peer sent its share list. Only accepted if we asked for it.
    pub fn received(&self, username: &str, mut dirs: Vec<SharedDir>) {
        let mut map = lock(&self.map);
        if !matches!(map.get(username), Some(BrowseState::Loading { .. })) {
            return;
        }
        dirs.retain(|d| !d.path.is_empty() || !d.files.is_empty());
        dirs.sort_by_key(|d| d.path.to_lowercase());
        map.insert(
            username.to_owned(),
            BrowseState::Ready {
                dirs: Arc::new(dirs),
                at_ms: now_ms(),
            },
        );
        // Keep memory bounded: forget the oldest finished lists.
        let mut ready: Vec<(String, u64)> = map
            .iter()
            .filter_map(|(user, state)| match state {
                BrowseState::Ready { at_ms, .. } => Some((user.clone(), *at_ms)),
                _ => None,
            })
            .collect();
        if ready.len() > MAX_CACHED_BROWSES {
            ready.sort_by_key(|(_, at)| *at);
            for (user, _) in ready.iter().take(ready.len() - MAX_CACHED_BROWSES) {
                map.remove(user);
            }
        }
    }

    fn fail(&self, username: &str, error: String) {
        let mut map = lock(&self.map);
        if matches!(map.get(username), Some(BrowseState::Loading { .. })) {
            map.insert(
                username.to_owned(),
                BrowseState::Failed {
                    error,
                    at_ms: now_ms(),
                },
            );
        }
    }

    fn view(&self, username: &str) -> Option<BrowseView> {
        let map = lock(&self.map);
        let state = map.get(username)?;
        let mut view = BrowseView {
            username: username.to_owned(),
            status: BrowseStatus::Loading,
            error: None,
            received_at_ms: None,
            total_files: 0,
            dirs: Vec::new(),
        };
        match state {
            BrowseState::Loading { .. } => {}
            BrowseState::Failed { error, at_ms } => {
                view.status = BrowseStatus::Failed;
                view.error = Some(error.clone());
                view.received_at_ms = Some(*at_ms);
            }
            BrowseState::Ready { dirs, at_ms } => {
                view.status = BrowseStatus::Ready;
                view.received_at_ms = Some(*at_ms);
                view.total_files = dirs.iter().map(|d| d.files.len()).sum();
                view.dirs = dirs
                    .iter()
                    .map(|d| DirSummary {
                        path: d.path.clone(),
                        files: d.files.len(),
                        bytes: d.files.iter().map(|f| f.size).sum(),
                    })
                    .collect();
            }
        }
        Some(view)
    }

    fn files(&self, username: &str, dir: &str) -> Option<Vec<SharedFile>> {
        match lock(&self.map).get(username)? {
            BrowseState::Ready { dirs, .. } => {
                dirs.iter().find(|d| d.path == dir).map(|d| d.files.clone())
            }
            _ => None,
        }
    }
}

// ---------- chat ----------

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub from_me: bool,
    pub text: String,
    pub at_ms: u64,
}

struct Conversation {
    username: String,
    messages: VecDeque<ChatMessage>,
    unread: usize,
}

#[derive(Default)]
pub(super) struct Chats {
    /// Most recently active first.
    conversations: VecDeque<Conversation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationSummary {
    pub username: String,
    pub unread: usize,
    pub last: Option<ChatMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatView {
    pub username: String,
    pub messages: Vec<ChatMessage>,
}

impl Chats {
    fn conversation(&mut self, username: &str) -> &mut Conversation {
        let existing = self
            .conversations
            .iter()
            .position(|c| c.username == username);
        let conversation = match existing {
            Some(index) => self
                .conversations
                .remove(index)
                .unwrap_or_else(|| Conversation {
                    username: username.to_owned(),
                    messages: VecDeque::new(),
                    unread: 0,
                }),
            None => Conversation {
                username: username.to_owned(),
                messages: VecDeque::new(),
                unread: 0,
            },
        };
        self.conversations.push_front(conversation);
        self.conversations.truncate(MAX_CONVERSATIONS);
        &mut self.conversations[0]
    }

    fn push(&mut self, username: &str, message: ChatMessage) {
        let from_me = message.from_me;
        let conversation = self.conversation(username);
        conversation.messages.push_back(message);
        while conversation.messages.len() > MAX_MESSAGES {
            conversation.messages.pop_front();
        }
        if !from_me {
            conversation.unread += 1;
        }
    }

    /// A private message from `username`; `timestamp` is the server's
    /// (Unix seconds) and may be old for messages sent while we were away.
    pub fn received(&mut self, username: &str, text: &str, timestamp: u32) {
        let Some(text) = clean_multiline(text) else {
            return;
        };
        let text: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
        let at_ms = match u64::from(timestamp) * 1000 {
            0 => now_ms(),
            ms => ms.min(now_ms()),
        };
        self.push(
            username,
            ChatMessage {
                from_me: false,
                text,
                at_ms,
            },
        );
    }

    pub fn unread(&self) -> usize {
        self.conversations.iter().map(|c| c.unread).sum()
    }

    fn summaries(&self) -> Vec<ConversationSummary> {
        self.conversations
            .iter()
            .map(|c| ConversationSummary {
                username: c.username.clone(),
                unread: c.unread,
                last: c.messages.back().cloned(),
            })
            .collect()
    }

    fn view(&mut self, username: &str, mark_read: bool) -> ChatView {
        let messages = match self
            .conversations
            .iter_mut()
            .find(|c| c.username == username)
        {
            Some(conversation) => {
                if mark_read {
                    conversation.unread = 0;
                }
                conversation.messages.iter().cloned().collect()
            }
            None => Vec::new(),
        };
        ChatView {
            username: username.to_owned(),
            messages,
        }
    }

    fn close(&mut self, username: &str) {
        self.conversations.retain(|c| c.username != username);
    }
}

// ---------- user info ----------

#[derive(Debug, Clone, Serialize)]
pub struct UserDetails {
    pub username: String,
    pub status: Option<UserStatus>,
    /// Bytes per second.
    pub avg_speed: Option<u32>,
    pub uploads: Option<u32>,
    pub shared_files: Option<u32>,
    pub shared_folders: Option<u32>,
    pub description: Option<String>,
    /// The user's picture as a `data:` URL (JPEG, PNG or GIF only).
    pub picture: Option<String>,
    pub slots_free: Option<bool>,
    pub queue_size: Option<u32>,
    /// Why the peer's own info could not be fetched.
    pub info_error: Option<String>,
}

/// Only common raster formats are passed to the browser.
fn picture_data_url(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_PICTURE_BYTES {
        return None;
    }
    let mime = if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else {
        return None;
    };
    Some(format!("data:{mime};base64,{}", base64(bytes)))
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Session {
    /// Ask `username` for their share list (or reuse a recent one).
    pub fn browse(
        self: &Arc<Self>,
        username: &str,
        refresh: bool,
    ) -> Result<BrowseView, SoulseekError> {
        let username = check_username(username)?;
        {
            let mut map = lock(&self.browses.map);
            let now = now_ms();
            let reuse = match map.get(&username) {
                Some(BrowseState::Loading { since_ms }) => {
                    now.saturating_sub(*since_ms) < BROWSE_TIMEOUT.as_millis() as u64
                }
                Some(BrowseState::Ready { at_ms, .. }) => {
                    !refresh && now.saturating_sub(*at_ms) < BROWSE_FRESH_MS
                }
                _ => false,
            };
            if !reuse {
                if !self.is_online() {
                    return Err(SoulseekError::Offline);
                }
                map.insert(username.clone(), BrowseState::Loading { since_ms: now });
                tokio::spawn(Arc::clone(self).request_shares(username.clone()));
            }
        }
        self.browse_view(&username).ok_or(SoulseekError::NotFound)
    }

    async fn request_shares(self: Arc<Self>, username: String) {
        if let Err(e) = self.send_to(&username, PeerMessage::GetShareFileList).await {
            self.browses.fail(&username, e.to_string());
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(BROWSE_TIMEOUT) => {}
            _ = self.shutdown.cancelled() => return,
        }
        self.browses.fail(
            &username,
            format!("{username} did not send their share list in time"),
        );
    }

    pub fn browse_view(&self, username: &str) -> Option<BrowseView> {
        self.browses.view(username)
    }

    pub fn browse_files(&self, username: &str, dir: &str) -> Option<Vec<SharedFile>> {
        self.browses.files(username, dir)
    }

    /// A message from `username` could not be decoded; fail what waited on it.
    pub(super) fn peer_message_failed(&self, username: &str, code: u32) {
        if code == peer_code::SHARED_FILE_LIST {
            self.browses
                .fail(username, "their share list could not be read".into());
        }
    }

    /// The full contents of one of `username`'s folders (with subfolders).
    pub async fn folder_contents(
        self: &Arc<Self>,
        username: &str,
        folder: &str,
    ) -> Result<Arc<Vec<SharedDir>>, SoulseekError> {
        let token = self.next_token();
        let answer = self.peers.folders.wait(token);
        self.send_to(
            username,
            PeerMessage::FolderContentsRequest {
                token,
                folder: folder.to_owned(),
            },
        )
        .await?;
        tokio::time::timeout(FOLDER_TIMEOUT, answer)
            .await
            .map_err(|_| SoulseekError::PeerTimeout {
                username: username.to_owned(),
            })?
            .map_err(|_| SoulseekError::Offline)
    }

    pub async fn user_details(
        self: &Arc<Self>,
        username: &str,
    ) -> Result<UserDetails, SoulseekError> {
        let username = check_username(username)?;
        if !self.is_online() {
            return Err(SoulseekError::Offline);
        }
        let status_answer = self.statuses.wait(username.clone());
        let stats_answer = self.stats.wait(username.clone());
        self.send_server(ServerRequest::GetUserStatus(username.clone()))?;
        self.send_server(ServerRequest::GetUserStats(username.clone()))?;
        let status = tokio::time::timeout(STATUS_TIMEOUT, status_answer)
            .await
            .ok()
            .and_then(Result::ok);
        let stats = tokio::time::timeout(STATUS_TIMEOUT, stats_answer)
            .await
            .ok()
            .and_then(Result::ok);

        let mut details = UserDetails {
            username: username.clone(),
            status,
            avg_speed: stats.as_ref().map(|s| s.avg_speed),
            uploads: stats.as_ref().map(|s| s.uploads),
            shared_files: stats.as_ref().map(|s| s.files),
            shared_folders: stats.as_ref().map(|s| s.dirs),
            description: None,
            picture: None,
            slots_free: None,
            queue_size: None,
            info_error: None,
        };
        if status == Some(UserStatus::Offline) {
            details.info_error = Some(format!("{username} is offline"));
            return Ok(details);
        }
        let info_answer = self.peers.infos.wait(username.clone());
        let info = match self.send_to(&username, PeerMessage::UserInfoRequest).await {
            Ok(()) => tokio::time::timeout(INFO_TIMEOUT, info_answer)
                .await
                .map_err(|_| format!("{username} did not answer in time"))
                .and_then(|r| r.map_err(|_| "connection closed".to_owned())),
            Err(e) => Err(e.to_string()),
        };
        match info {
            Ok(info) => {
                details.description = clean_multiline(&info.description)
                    .map(|d| d.chars().take(MAX_MESSAGE_CHARS * 2).collect());
                details.picture = info.picture.as_deref().and_then(picture_data_url);
                details.slots_free = Some(info.slots_free);
                details.queue_size = Some(info.queue_size);
                if details.uploads.is_none() {
                    details.uploads = Some(info.total_uploads);
                }
            }
            Err(e) => details.info_error = Some(e),
        }
        Ok(details)
    }

    pub fn send_message(&self, username: &str, text: &str) -> Result<ChatView, SoulseekError> {
        let username = check_username(username)?;
        let text = clean_multiline(text)
            .filter(|t| t.chars().count() <= MAX_MESSAGE_CHARS)
            .ok_or_else(|| {
                SoulseekError::InvalidInput(format!(
                    "messages are 1–{MAX_MESSAGE_CHARS} characters"
                ))
            })?;
        self.send_server(ServerRequest::MessageUser {
            username: username.clone(),
            message: text.clone(),
        })?;
        let mut chats = lock(&self.chats);
        chats.push(
            &username,
            ChatMessage {
                from_me: true,
                text,
                at_ms: now_ms(),
            },
        );
        Ok(chats.view(&username, true))
    }

    pub fn conversations(&self) -> Vec<ConversationSummary> {
        lock(&self.chats).summaries()
    }

    /// A conversation; reading it marks it read.
    pub fn conversation(&self, username: &str) -> ChatView {
        lock(&self.chats).view(username, true)
    }

    pub fn close_conversation(&self, username: &str) {
        lock(&self.chats).close(username);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn only_raster_pictures_are_passed_on() {
        assert!(
            picture_data_url(&[0xFF, 0xD8, 0xFF, 0xE0])
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
        assert!(picture_data_url(b"<svg onload=alert(1)>").is_none());
        assert!(picture_data_url(&vec![0xFF; MAX_PICTURE_BYTES + 1]).is_none());
    }

    #[test]
    fn chats_track_unread_and_bounds() {
        let mut chats = Chats::default();
        chats.received("alice", "hey, got that dub?\u{7}", 0);
        chats.received("bob", "  ", 0);
        assert_eq!(chats.unread(), 1);
        assert_eq!(chats.summaries().len(), 1);
        let view = chats.view("alice", true);
        assert_eq!(view.messages[0].text, "hey, got that dub?");
        assert_eq!(chats.unread(), 0);
        for i in 0..(MAX_MESSAGES + 10) {
            chats.received("alice", &format!("m{i}"), 0);
        }
        assert_eq!(chats.view("alice", false).messages.len(), MAX_MESSAGES);
    }

    #[test]
    fn browse_lists_are_only_accepted_when_requested() {
        let browses = Browses::default();
        let dirs = vec![SharedDir {
            path: "music\\a".into(),
            files: vec![],
        }];
        browses.received("stranger", dirs.clone());
        assert!(browses.view("stranger").is_none());
        lock(&browses.map).insert("friend".into(), BrowseState::Loading { since_ms: 0 });
        browses.received("friend", dirs);
        assert!(matches!(
            browses.view("friend").unwrap().status,
            BrowseStatus::Ready
        ));
    }
}
