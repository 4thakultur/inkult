//! Chat rooms for the crew, in the spirit of IRC channels and Soulseek
//! rooms: anyone on this Inkult server can create a public room, join it
//! and talk; the room's owner (or a captain, as moderator) can change its
//! topic or delete it.
//!
//! Members can also share a track from their library into a room (see
//! [`SharedTrack`]): the message names one library file, and being a member
//! of a room that holds such a message is what lets others play it or add
//! it to their own library. Access lasts as long as the message is in the
//! room's history.
//!
//! A room can also be private: it is protected by a password chosen when
//! it is created and is invisible to everyone who is not a member,
//! captains included (it does not show in the room list, and its id
//! answers "no such room"). Joining one takes its name and password
//! ([`Rooms::join_private`]); after that, membership is the access grant,
//! so members never type the password again. A private room nobody is in
//! any more is deleted, since no one could see or moderate it.
//!
//! These rooms live on this server only; they are not Soulseek network
//! rooms. History is bounded per room and persisted (see [`store`]). Who is
//! "online" in a room is not persisted: it means the member's browser has
//! looked at the room within the last [`ONLINE_WINDOW`].

mod store;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell, Semaphore};

use self::store::RoomStore;
pub use self::store::StoreError;
use crate::auth::password::{self, MAX_PASSWORD_CHARS, random_id};
use crate::auth::{User, UserId};
use crate::domain::provider::TrackKey;
use crate::domain::quality::{OutputFormat, QualityReport};
use crate::library::LibraryEntry;
use crate::metadata::{clean_multiline, clean_text};

const MAX_ROOMS: usize = 64;
/// Rooms one account may own, so a single user cannot fill the server.
const MAX_OWNED_ROOMS: usize = 8;
/// Messages kept per room; older ones are dropped.
const MAX_HISTORY: usize = 500;
/// Messages sent when a room is opened without `after`.
const RECENT_MESSAGES: usize = 200;
pub const MAX_TEXT_CHARS: usize = 1000;
const MAX_TEXT_LINES: usize = 20;
pub const MAX_TOPIC_CHARS: usize = 200;
const MIN_NAME_CHARS: usize = 2;
pub const MAX_NAME_CHARS: usize = 32;
const ONLINE_WINDOW: Duration = Duration::from_secs(30);
/// Flood control: at most this many messages per user per window.
const FLOOD_MAX: usize = 8;
const FLOOD_WINDOW: Duration = Duration::from_secs(10);
/// Most tracks shared in one go. They count as a single message for flood
/// control, so the cap keeps one send from pushing a room's history out.
pub const MAX_SHARED_AT_ONCE: usize = 50;
const FILE_VERSION: u32 = 1;
/// Wrong private-room passwords one user may try per window. Counted per
/// account (everyone here is signed in), before hashing, so concurrent
/// requests cannot slip past it.
const JOIN_FAILURES_MAX: usize = 10;
const JOIN_FAILURES_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Argon2 runs at once for room passwords, apart from the sign-in ones.
const MAX_CONCURRENT_HASHES: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("a room called #{0} already exists")]
    NameTaken(String),
    #[error("no such room; it may have been deleted")]
    NotFound,
    #[error("join the room first")]
    NotMember,
    #[error("that track is no longer in the room's history")]
    NoSuchTrack,
    #[error("only the room's owner may do that")]
    NotOwner,
    #[error("{0}")]
    Limit(String),
    /// Deliberately the same for an unknown name and a wrong password.
    #[error("no private room matches that name and password")]
    BadCredentials,
    #[error("too many wrong room passwords; try again later")]
    Throttled,
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Random rather than sequential, so a deleted room's id is never reused
/// for a new room that open browsers would then show in its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomId(pub u64);

impl std::fmt::Display for RoomId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Something said.
    Text,
    /// `/me does something`.
    Action,
    Joined,
    Left,
    /// The topic changed; `text` is the new topic (empty when cleared).
    Topic,
    /// A library track shared with the room; see [`SharedTrack`].
    Track,
}

impl MessageKind {
    /// Whether the message counts towards a member's unread count.
    fn is_chat(self) -> bool {
        matches!(self, Self::Text | Self::Action | Self::Track)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredMessage {
    seq: u64,
    kind: MessageKind,
    author: UserId,
    /// The author's handle when they wrote it, so history still reads
    /// right after their account is deleted.
    from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    text: String,
    at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track: Option<SharedTrack>,
}

/// A library file shared into a room, with what the card in the room shows.
/// The display fields are a snapshot taken when it was shared; the file
/// itself is looked up again by `key` + `output` whenever someone uses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedTrack {
    pub key: TrackKey,
    pub output: OutputFormat,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub artwork_url: Option<String>,
    pub source_url: String,
    pub quality: QualityReport,
}

impl From<&LibraryEntry> for SharedTrack {
    fn from(entry: &LibraryEntry) -> Self {
        Self {
            key: entry.key.clone(),
            output: entry.output,
            title: entry.title.clone(),
            artists: entry.artists.clone(),
            album: entry.album.clone(),
            artwork_url: entry.artwork_url.clone(),
            source_url: entry.source_url.clone(),
            quality: entry.quality.clone(),
        }
    }
}

/// Where a shared track stands for the viewer. Rooms do not know the
/// library; the application layer fills this in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackStatus {
    /// Already in the viewer's library.
    InLibrary,
    /// On disk; the viewer can play it or add it to their library.
    Available,
    /// No longer in the library index.
    Gone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Member {
    name: String,
    /// Highest message `seq` this member has seen.
    last_read: u64,
}

/// A room as stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomRecord {
    version: u32,
    id: RoomId,
    name: String,
    #[serde(default)]
    topic: Option<String>,
    owner: UserId,
    owner_name: String,
    created_at_ms: u64,
    /// `seq` of the next message; never reused, even after history trimming.
    next_seq: u64,
    #[serde(default)]
    members: BTreeMap<UserId, Member>,
    #[serde(default)]
    messages: VecDeque<StoredMessage>,
    /// Argon2id PHC string; present exactly when the room is private.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_hash: Option<String>,
}

impl RoomRecord {
    fn push(&mut self, author: &User, kind: MessageKind, text: String) -> StoredMessage {
        self.push_with_track(author, kind, text, None)
    }

    fn push_with_track(
        &mut self,
        author: &User,
        kind: MessageKind,
        text: String,
        track: Option<SharedTrack>,
    ) -> StoredMessage {
        let message = StoredMessage {
            seq: self.next_seq,
            kind,
            author: author.id,
            from: author.username.clone(),
            text,
            at_ms: now_ms(),
            track,
        };
        self.next_seq += 1;
        self.messages.push_back(message.clone());
        while self.messages.len() > MAX_HISTORY {
            self.messages.pop_front();
        }
        message
    }

    fn latest_seq(&self) -> u64 {
        self.messages.back().map_or(0, |m| m.seq)
    }

    fn unread(&self, user: UserId) -> usize {
        let Some(member) = self.members.get(&user) else {
            return 0;
        };
        self.messages
            .iter()
            .rev()
            .take_while(|m| m.seq > member.last_read)
            .filter(|m| m.kind.is_chat() && m.author != user)
            .count()
    }

    fn may_moderate(&self, actor: &User) -> bool {
        self.owner == actor.id || actor.role.is_captain()
    }

    fn is_private(&self) -> bool {
        self.password_hash.is_some()
    }

    /// Whether `user` may know the room exists.
    fn visible_to(&self, user: UserId) -> bool {
        !self.is_private() || self.members.contains_key(&user)
    }
}

/// A room as listed for one user.
#[derive(Debug, Clone, Serialize)]
pub struct RoomSummary {
    pub id: RoomId,
    pub name: String,
    pub topic: Option<String>,
    /// The owner's handle.
    pub owner: String,
    /// Whether the viewer owns the room.
    pub mine: bool,
    /// Whether the viewer may change the topic or delete the room.
    pub can_moderate: bool,
    /// Hidden from non-members and joined with a password.
    pub private: bool,
    pub members: usize,
    pub online: usize,
    pub joined: bool,
    pub unread: usize,
    pub last_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageView {
    pub seq: u64,
    pub kind: MessageKind,
    pub from: String,
    pub mine: bool,
    pub text: String,
    pub at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<SharedTrack>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_status: Option<TrackStatus>,
}

impl MessageView {
    fn new(message: &StoredMessage, viewer: UserId) -> Self {
        Self {
            seq: message.seq,
            kind: message.kind,
            from: message.from.clone(),
            mine: message.author == viewer,
            text: message.text.clone(),
            at_ms: message.at_ms,
            track: message.track.clone(),
            track_status: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RoomUser {
    pub name: String,
    pub owner: bool,
    pub online: bool,
}

/// An open room: its summary, messages and user list.
#[derive(Debug, Clone, Serialize)]
pub struct RoomView {
    pub room: RoomSummary,
    pub messages: Vec<MessageView>,
    pub users: Vec<RoomUser>,
}

#[derive(Default)]
struct Inner {
    rooms: BTreeMap<RoomId, RoomRecord>,
    /// When each member last looked at each room.
    presence: HashMap<RoomId, HashMap<UserId, Instant>>,
    /// Recent message times per user, for flood control.
    sent: HashMap<UserId, VecDeque<Instant>>,
    /// Recent wrong private-room passwords per user.
    join_failures: HashMap<UserId, VecDeque<Instant>>,
}

impl Inner {
    fn room(&self, id: RoomId) -> Result<&RoomRecord, RoomError> {
        self.rooms.get(&id).ok_or(RoomError::NotFound)
    }

    fn room_mut(&mut self, id: RoomId) -> Result<&mut RoomRecord, RoomError> {
        self.rooms.get_mut(&id).ok_or(RoomError::NotFound)
    }

    /// The room, unless it is a private one `user` is not in: that answers
    /// exactly like a room that does not exist.
    fn visible_room(&self, id: RoomId, user: UserId) -> Result<&RoomRecord, RoomError> {
        self.room(id)
            .and_then(|r| r.visible_to(user).then_some(r).ok_or(RoomError::NotFound))
    }

    fn visible_room_mut(&mut self, id: RoomId, user: UserId) -> Result<&mut RoomRecord, RoomError> {
        self.visible_room(id, user)?;
        self.room_mut(id)
    }

    /// Reserve one wrong-password slot for `user`, or refuse when the
    /// window's slots are used up. A right password hands it back.
    fn reserve_join_attempt(&mut self, user: UserId) -> Result<(), RoomError> {
        let failures = self.join_failures.entry(user).or_default();
        while failures
            .front()
            .is_some_and(|t| t.elapsed() >= JOIN_FAILURES_WINDOW)
        {
            failures.pop_front();
        }
        if failures.len() >= JOIN_FAILURES_MAX {
            return Err(RoomError::Throttled);
        }
        failures.push_back(Instant::now());
        Ok(())
    }

    fn release_join_attempt(&mut self, user: UserId) {
        if let Some(failures) = self.join_failures.get_mut(&user) {
            failures.pop_back();
        }
    }

    fn touch(&mut self, room: RoomId, user: UserId) {
        self.presence
            .entry(room)
            .or_default()
            .insert(user, Instant::now());
    }

    fn is_online(&self, room: RoomId, user: UserId) -> bool {
        self.presence
            .get(&room)
            .and_then(|p| p.get(&user))
            .is_some_and(|at| at.elapsed() < ONLINE_WINDOW)
    }

    fn summary(&self, room: &RoomRecord, viewer: &User) -> RoomSummary {
        RoomSummary {
            id: room.id,
            name: room.name.clone(),
            topic: room.topic.clone(),
            owner: room.owner_name.clone(),
            mine: room.owner == viewer.id,
            can_moderate: room.may_moderate(viewer),
            private: room.is_private(),
            members: room.members.len(),
            online: room
                .members
                .keys()
                .filter(|&&id| self.is_online(room.id, id))
                .count(),
            joined: room.members.contains_key(&viewer.id),
            unread: room.unread(viewer.id),
            last_at_ms: room
                .messages
                .iter()
                .rev()
                .find(|m| m.kind.is_chat())
                .map(|m| m.at_ms),
        }
    }

    fn view(&self, room: &RoomRecord, viewer: &User, after: Option<u64>) -> RoomView {
        let messages: Vec<&StoredMessage> = match after {
            Some(after) => room.messages.iter().filter(|m| m.seq > after).collect(),
            None => {
                let skip = room.messages.len().saturating_sub(RECENT_MESSAGES);
                room.messages.iter().skip(skip).collect()
            }
        };
        let mut users: Vec<RoomUser> = room
            .members
            .iter()
            .map(|(&id, member)| RoomUser {
                name: member.name.clone(),
                owner: id == room.owner,
                online: self.is_online(room.id, id),
            })
            .collect();
        users.sort_by(|a, b| (!a.online, &a.name).cmp(&(!b.online, &b.name)));
        RoomView {
            room: self.summary(room, viewer),
            messages: messages
                .into_iter()
                .map(|m| MessageView::new(m, viewer.id))
                .collect(),
            users,
        }
    }

    /// Record a message about to be sent, or refuse it as flooding.
    fn allow_message(&mut self, user: UserId) -> Result<(), RoomError> {
        let sent = self.sent.entry(user).or_default();
        while sent.front().is_some_and(|t| t.elapsed() >= FLOOD_WINDOW) {
            sent.pop_front();
        }
        if sent.len() >= FLOOD_MAX {
            return Err(RoomError::Limit(
                "slow down: too many messages in a short time".into(),
            ));
        }
        sent.push_back(Instant::now());
        Ok(())
    }
}

pub struct Rooms {
    store: RoomStore,
    inner: Mutex<Inner>,
    hashing: Semaphore,
    /// Checked when no private room has the given name, so an unknown
    /// name takes as long as a wrong password.
    dummy_hash: OnceCell<String>,
}

impl Rooms {
    pub async fn open(dir: PathBuf) -> Result<Self, RoomError> {
        let (store, records) = RoomStore::open(dir).await?;
        let rooms = records.into_iter().map(|r| (r.id, r)).collect();
        Ok(Self {
            store,
            inner: Mutex::new(Inner {
                rooms,
                ..Inner::default()
            }),
            hashing: Semaphore::new(MAX_CONCURRENT_HASHES),
            dummy_hash: OnceCell::new(),
        })
    }

    async fn hash(&self, secret: &str) -> Result<String, RoomError> {
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|e| RoomError::Internal(e.to_string()))?;
        let secret = secret.to_owned();
        tokio::task::spawn_blocking(move || password::hash(&secret))
            .await
            .map_err(|e| RoomError::Internal(e.to_string()))?
            .map_err(|e| RoomError::Internal(e.to_string()))
    }

    async fn verify(&self, secret: &str, hash: String) -> Result<bool, RoomError> {
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|e| RoomError::Internal(e.to_string()))?;
        let secret = secret.to_owned();
        Ok(
            tokio::task::spawn_blocking(move || password::verify(&secret, &hash))
                .await
                .unwrap_or(false),
        )
    }

    /// Delete a private room once nobody is in it: no one could see,
    /// rejoin-by-list or moderate it, yet it would count against the
    /// server's room limit forever. Disk first, as in [`Self::delete`].
    async fn drop_if_abandoned(&self, inner: &mut Inner, id: RoomId) {
        let Some(room) = inner.rooms.get(&id) else {
            return;
        };
        if !room.is_private() || !room.members.is_empty() {
            return;
        }
        let name = room.name.clone();
        match self.store.remove(id).await {
            Ok(()) => {
                inner.rooms.remove(&id);
                inner.presence.remove(&id);
                tracing::info!(room = %name, "empty private chat room deleted");
            }
            Err(e) => {
                tracing::error!(room = %name, error = %e, "failed to delete empty private chat room");
            }
        }
    }

    /// Save a room after a change that has already happened in memory.
    /// Memory stays authoritative on failure (the next save catches up), so
    /// a message is never reported as failed after others have seen it.
    async fn persist(&self, room: &RoomRecord) {
        if let Err(e) = self.store.save(room).await {
            tracing::error!(room = %room.name, error = %e, "failed to save chat room");
        }
    }

    /// Every public room, joined or not, and the private rooms the actor
    /// is in, by name.
    pub async fn list(&self, actor: &User) -> Vec<RoomSummary> {
        let inner = self.inner.lock().await;
        let mut rooms: Vec<RoomSummary> = inner
            .rooms
            .values()
            .filter(|r| r.visible_to(actor.id))
            .map(|r| inner.summary(r, actor))
            .collect();
        rooms.sort_by(|a, b| a.name.cmp(&b.name));
        rooms
    }

    /// Unread messages over all rooms the user has joined.
    pub async fn unread_total(&self, actor: &User) -> usize {
        let inner = self.inner.lock().await;
        inner.rooms.values().map(|r| r.unread(actor.id)).sum()
    }

    /// Create a room owned (and joined) by `actor`; with a `password`, a
    /// private one. Names are unique over public and private rooms alike,
    /// so `/join name` can never mean two rooms. The price is that taking
    /// a name tells the creator a private room already has it; the
    /// password is what keeps such a room closed.
    pub async fn create(
        &self,
        actor: &User,
        name: &str,
        topic: Option<&str>,
        password: Option<&str>,
    ) -> Result<RoomView, RoomError> {
        let name = normalize_room_name(name)?;
        let topic = topic.map(clean_topic).transpose()?.flatten();
        if let Some(secret) = password {
            check_room_password(secret)?;
        }
        // Cheap refusals before the slow hash; checked again below.
        self.check_can_create(&*self.inner.lock().await, actor, &name)?;
        let password_hash = match password {
            Some(secret) => Some(self.hash(secret).await?),
            None => None,
        };
        let mut inner = self.inner.lock().await;
        self.check_can_create(&inner, actor, &name)?;
        let id = loop {
            let id = RoomId(random_id());
            if !inner.rooms.contains_key(&id) {
                break id;
            }
        };
        let mut room = RoomRecord {
            version: FILE_VERSION,
            id,
            name,
            topic: None,
            owner: actor.id,
            owner_name: actor.username.clone(),
            created_at_ms: now_ms(),
            next_seq: 1,
            members: BTreeMap::new(),
            messages: VecDeque::new(),
            password_hash,
        };
        room.members.insert(
            actor.id,
            Member {
                name: actor.username.clone(),
                last_read: 0,
            },
        );
        room.push(actor, MessageKind::Joined, String::new());
        if let Some(topic) = topic {
            room.topic = Some(topic.clone());
            room.push(actor, MessageKind::Topic, topic);
        }
        let latest = room.latest_seq();
        if let Some(member) = room.members.get_mut(&actor.id) {
            member.last_read = latest;
        }
        // Unlike later changes, a room that cannot be saved is not created:
        // the user would otherwise lose it on the next restart.
        self.store.save(&room).await?;
        tracing::info!(room = %room.name, owner = %actor.username, private = room.is_private(), "chat room created");
        inner.touch(id, actor.id);
        let view = inner.view(&room, actor, None);
        inner.rooms.insert(id, room);
        Ok(view)
    }

    fn check_can_create(&self, inner: &Inner, actor: &User, name: &str) -> Result<(), RoomError> {
        if inner.rooms.values().any(|r| r.name == name) {
            return Err(RoomError::NameTaken(name.to_owned()));
        }
        if inner.rooms.len() >= MAX_ROOMS {
            return Err(RoomError::Limit(format!(
                "this server already has {MAX_ROOMS} rooms; delete one first"
            )));
        }
        let owned = inner.rooms.values().filter(|r| r.owner == actor.id).count();
        if owned >= MAX_OWNED_ROOMS {
            return Err(RoomError::Limit(format!(
                "you already own {MAX_OWNED_ROOMS} rooms; delete one first"
            )));
        }
        Ok(())
    }

    /// Delete a room and its history. Owner or captain only.
    pub async fn delete(&self, actor: &User, id: RoomId) -> Result<(), RoomError> {
        let mut inner = self.inner.lock().await;
        let room = inner.visible_room(id, actor.id)?;
        if !room.may_moderate(actor) {
            return Err(RoomError::NotOwner);
        }
        let name = room.name.clone();
        // Disk first: a room that comes back after a restart would be worse
        // than an error now.
        self.store.remove(id).await?;
        inner.rooms.remove(&id);
        inner.presence.remove(&id);
        tracing::info!(room = %name, by = %actor.username, "chat room deleted");
        Ok(())
    }

    /// Join a public room (or open one the actor is already in). Private
    /// rooms are joined with [`Self::join_private`].
    pub async fn join(&self, actor: &User, id: RoomId) -> Result<RoomView, RoomError> {
        let mut inner = self.inner.lock().await;
        inner.visible_room(id, actor.id)?;
        self.add_member(&mut inner, actor, id).await
    }

    /// Join a private room by its name and password. A wrong password and
    /// an unknown name look the same, in the answer and in timing, so the
    /// endpoint cannot be used to discover private rooms.
    pub async fn join_private(
        &self,
        actor: &User,
        name: &str,
        secret: &str,
    ) -> Result<RoomView, RoomError> {
        let name = normalize_room_name(name).map_err(|_| RoomError::BadCredentials)?;
        if secret.chars().count() > MAX_PASSWORD_CHARS {
            return Err(RoomError::BadCredentials);
        }
        let target = {
            let mut inner = self.inner.lock().await;
            let target = inner
                .rooms
                .values()
                .find(|r| r.name == name)
                .and_then(|r| Some((r.id, r.password_hash.clone()?)));
            if let Some((id, _)) = target
                && inner.room(id)?.members.contains_key(&actor.id)
            {
                // Already in: no password needed, nothing to count.
                return self.add_member(&mut inner, actor, id).await;
            }
            inner.reserve_join_attempt(actor.id)?;
            target
        };
        // Hash outside the lock: Argon2 takes long enough to stall every
        // room otherwise. An unknown name is checked against a dummy hash,
        // so it takes as long as a wrong password.
        let (id, hash) = match target {
            Some((id, hash)) => (Some(id), hash),
            None => {
                let dummy = self
                    .dummy_hash
                    .get_or_try_init(|| self.hash("inkult-dummy-room-password"))
                    .await?;
                (None, dummy.clone())
            }
        };
        let matched = self.verify(secret, hash.clone()).await?;
        let mut inner = self.inner.lock().await;
        // The room may have been deleted, or replaced under the same name,
        // while hashing: only a room still holding that hash is joined.
        let joined = id.filter(|id| {
            matched
                && inner
                    .rooms
                    .get(id)
                    .is_some_and(|r| r.password_hash.as_ref() == Some(&hash))
        });
        let Some(id) = joined else {
            tracing::warn!(user = %actor.username, "wrong private room name or password");
            return Err(RoomError::BadCredentials);
        };
        inner.release_join_attempt(actor.id);
        self.add_member(&mut inner, actor, id).await
    }

    async fn add_member(
        &self,
        inner: &mut Inner,
        actor: &User,
        id: RoomId,
    ) -> Result<RoomView, RoomError> {
        let room = inner.room_mut(id)?;
        if !room.members.contains_key(&actor.id) {
            let joined = room.push(actor, MessageKind::Joined, String::new());
            room.members.insert(
                actor.id,
                Member {
                    name: actor.username.clone(),
                    // Joining shows the backlog, but does not flag it unread.
                    last_read: joined.seq,
                },
            );
            let snapshot = room.clone();
            self.persist(&snapshot).await;
        }
        inner.touch(id, actor.id);
        let room = inner.room(id)?;
        Ok(inner.view(room, actor, None))
    }

    pub async fn leave(&self, actor: &User, id: RoomId) -> Result<(), RoomError> {
        let mut inner = self.inner.lock().await;
        let room = inner.visible_room_mut(id, actor.id)?;
        if room.members.remove(&actor.id).is_some() {
            room.push(actor, MessageKind::Left, String::new());
            let snapshot = room.clone();
            self.persist(&snapshot).await;
        }
        if let Some(presence) = inner.presence.get_mut(&id) {
            presence.remove(&actor.id);
        }
        self.drop_if_abandoned(&mut inner, id).await;
        Ok(())
    }

    /// The room's messages after `after` (or the most recent ones), marking
    /// them read. Members only, like an IRC channel.
    pub async fn view(
        &self,
        actor: &User,
        id: RoomId,
        after: Option<u64>,
    ) -> Result<RoomView, RoomError> {
        let mut inner = self.inner.lock().await;
        let room = inner.visible_room_mut(id, actor.id)?;
        let latest = room.latest_seq();
        let member = room
            .members
            .get_mut(&actor.id)
            .ok_or(RoomError::NotMember)?;
        // Read positions are saved with the room's next change rather than
        // on every poll; after a crash a few messages may show unread again.
        member.last_read = member.last_read.max(latest);
        inner.touch(id, actor.id);
        let room = inner.room(id)?;
        Ok(inner.view(room, actor, after))
    }

    /// Post a message. `/me text` is sent as an action, as on IRC.
    pub async fn post(
        &self,
        actor: &User,
        id: RoomId,
        raw: &str,
    ) -> Result<MessageView, RoomError> {
        let (kind, body) = match raw.trim_start().strip_prefix("/me ") {
            Some(rest) => (MessageKind::Action, rest),
            None => (MessageKind::Text, raw),
        };
        let text = clean_message(body)?;
        self.append(actor, id, kind, text, None).await
    }

    /// Share a library track. The caller must already have checked that
    /// `actor` may access the file: sharing grants every member access.
    pub async fn share_track(
        &self,
        actor: &User,
        id: RoomId,
        track: SharedTrack,
    ) -> Result<MessageView, RoomError> {
        self.append(actor, id, MessageKind::Track, String::new(), Some(track))
            .await
    }

    /// Share several library tracks, one message each, as one send for
    /// flood control. Same caller duty as [`Rooms::share_track`].
    pub async fn share_tracks(
        &self,
        actor: &User,
        id: RoomId,
        tracks: Vec<SharedTrack>,
    ) -> Result<Vec<MessageView>, RoomError> {
        if tracks.len() > MAX_SHARED_AT_ONCE {
            return Err(RoomError::Limit(format!(
                "at most {MAX_SHARED_AT_ONCE} tracks can be sent at once"
            )));
        }
        let messages = tracks
            .into_iter()
            .map(|track| (MessageKind::Track, String::new(), Some(track)))
            .collect();
        self.append_many(actor, id, messages).await
    }

    async fn append(
        &self,
        actor: &User,
        id: RoomId,
        kind: MessageKind,
        text: String,
        track: Option<SharedTrack>,
    ) -> Result<MessageView, RoomError> {
        let mut sent = self
            .append_many(actor, id, vec![(kind, text, track)])
            .await?;
        sent.pop()
            .ok_or_else(|| RoomError::InvalidInput("nothing to send".into()))
    }

    async fn append_many(
        &self,
        actor: &User,
        id: RoomId,
        messages: Vec<(MessageKind, String, Option<SharedTrack>)>,
    ) -> Result<Vec<MessageView>, RoomError> {
        if messages.is_empty() {
            return Err(RoomError::InvalidInput("nothing to send".into()));
        }
        let mut inner = self.inner.lock().await;
        if !inner
            .visible_room(id, actor.id)?
            .members
            .contains_key(&actor.id)
        {
            return Err(RoomError::NotMember);
        }
        inner.allow_message(actor.id)?;
        let room = inner.room_mut(id)?;
        let sent: Vec<StoredMessage> = messages
            .into_iter()
            .map(|(kind, text, track)| room.push_with_track(actor, kind, text, track))
            .collect();
        if let (Some(member), Some(last)) = (room.members.get_mut(&actor.id), sent.last()) {
            member.last_read = last.seq;
        }
        let snapshot = room.clone();
        self.persist(&snapshot).await;
        inner.touch(id, actor.id);
        Ok(sent.iter().map(|m| MessageView::new(m, actor.id)).collect())
    }

    /// The track shared in message `seq`, for a member of the room. This is
    /// the permission check for using another member's library file.
    pub async fn shared_track(
        &self,
        actor: &User,
        id: RoomId,
        seq: u64,
    ) -> Result<SharedTrack, RoomError> {
        let inner = self.inner.lock().await;
        let room = inner.visible_room(id, actor.id)?;
        if !room.members.contains_key(&actor.id) {
            return Err(RoomError::NotMember);
        }
        room.messages
            .iter()
            .find(|m| m.seq == seq)
            .and_then(|m| m.track.clone())
            .ok_or(RoomError::NoSuchTrack)
    }

    /// Set or clear (`None` / blank) the topic. Owner or captain only.
    pub async fn set_topic(
        &self,
        actor: &User,
        id: RoomId,
        topic: Option<&str>,
    ) -> Result<RoomSummary, RoomError> {
        let topic = topic.map(clean_topic).transpose()?.flatten();
        let mut inner = self.inner.lock().await;
        let room = inner.visible_room_mut(id, actor.id)?;
        if !room.may_moderate(actor) {
            return Err(RoomError::NotOwner);
        }
        if room.topic != topic {
            room.topic = topic.clone();
            room.push(actor, MessageKind::Topic, topic.unwrap_or_default());
            let snapshot = room.clone();
            self.persist(&snapshot).await;
        }
        let room = inner.room(id)?;
        Ok(inner.summary(room, actor))
    }

    /// A deleted account leaves every room. Its messages stay (under the
    /// handle it had) and rooms it owned remain, deletable by a captain
    /// who can see them; a private room it was the last member of is
    /// deleted.
    pub async fn forget_user(&self, user: UserId) {
        let mut inner = self.inner.lock().await;
        let mut changed = Vec::new();
        for room in inner.rooms.values_mut() {
            if room.members.remove(&user).is_some() {
                changed.push(room.clone());
            }
        }
        for presence in inner.presence.values_mut() {
            presence.remove(&user);
        }
        inner.sent.remove(&user);
        inner.join_failures.remove(&user);
        for room in &changed {
            self.persist(room).await;
            self.drop_if_abandoned(&mut inner, room.id).await;
        }
    }
}

/// Room names follow IRC channel habits: case-insensitive, no spaces, an
/// optional leading `#` that is not part of the name.
pub fn normalize_room_name(raw: &str) -> Result<String, RoomError> {
    let name = raw.trim().trim_start_matches('#').to_lowercase();
    let len = name.chars().count();
    if !(MIN_NAME_CHARS..=MAX_NAME_CHARS).contains(&len) {
        return Err(RoomError::InvalidInput(format!(
            "room name must be {MIN_NAME_CHARS}–{MAX_NAME_CHARS} characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(RoomError::InvalidInput(
            "room name may only use letters, digits, '.', '_' and '-'".into(),
        ));
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(RoomError::InvalidInput(
            "room name must start with a letter or digit".into(),
        ));
    }
    Ok(name)
}

/// Room passwords have no strength rule, unlike account passwords: a
/// private room only keeps casual eyes out (its name is half the secret,
/// its content is crew chat), and wrong guesses are throttled per account.
/// Only what would break storage or the hash cost is refused.
fn check_room_password(secret: &str) -> Result<(), RoomError> {
    if secret.is_empty() {
        return Err(RoomError::InvalidInput(
            "room password must not be empty".into(),
        ));
    }
    if secret.chars().count() > MAX_PASSWORD_CHARS {
        return Err(too_long("room password", MAX_PASSWORD_CHARS));
    }
    if secret.chars().any(char::is_control) {
        return Err(RoomError::InvalidInput(
            "room password must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// `Ok(None)` clears the topic.
fn clean_topic(raw: &str) -> Result<Option<String>, RoomError> {
    if raw.chars().count() > MAX_TOPIC_CHARS * 2 {
        return Err(too_long("topic", MAX_TOPIC_CHARS));
    }
    let topic = clean_text(raw);
    if topic
        .as_ref()
        .is_some_and(|t| t.chars().count() > MAX_TOPIC_CHARS)
    {
        return Err(too_long("topic", MAX_TOPIC_CHARS));
    }
    Ok(topic)
}

/// Keeps line breaks (a pasted tracklist should stay readable) but bounds
/// both length and line count so one message cannot bury the room.
fn clean_message(raw: &str) -> Result<String, RoomError> {
    let text =
        clean_multiline(raw).ok_or_else(|| RoomError::InvalidInput("message is empty".into()))?;
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(too_long("message", MAX_TEXT_CHARS));
    }
    if text.lines().count() > MAX_TEXT_LINES {
        return Err(RoomError::InvalidInput(format!(
            "message has more than {MAX_TEXT_LINES} lines"
        )));
    }
    Ok(text)
}

fn too_long(what: &str, max: usize) -> RoomError {
    RoomError::InvalidInput(format!("{what} is longer than {max} characters"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    fn user(id: u64, name: &str, role: Role) -> User {
        User {
            id: UserId(id),
            username: name.into(),
            role,
            created_at: 0,
            locked_at: None,
            two_factor_enabled: false,
            password_temporary: false,
            temporary_password_expires_at: None,
        }
    }

    async fn open(dir: &std::path::Path) -> Rooms {
        Rooms::open(dir.join("rooms")).await.unwrap()
    }

    #[test]
    fn room_names() {
        assert_eq!(normalize_room_name(" #Techno ").unwrap(), "techno");
        assert_eq!(normalize_room_name("dnb_2026").unwrap(), "dnb_2026");
        for bad in [
            "#",
            "a",
            "with space",
            "../etc",
            "-dash",
            "ünï",
            &"x".repeat(33),
        ] {
            assert!(normalize_room_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn message_limits() {
        assert_eq!(clean_message("  hi\u{7}  \n there ").unwrap(), "hi\n there");
        assert!(clean_message("   ").is_err());
        assert!(clean_message(&"x".repeat(MAX_TEXT_CHARS + 1)).is_err());
        assert!(clean_message(&"a\n".repeat(MAX_TEXT_LINES + 1)).is_err());
        assert_eq!(clean_topic("  ").unwrap(), None);
        assert!(clean_topic(&"t".repeat(MAX_TOPIC_CHARS + 1)).is_err());
    }

    #[tokio::test]
    async fn lifecycle_permissions_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let alice = user(1, "alice", Role::Pirate);
        let bob = user(2, "bob", Role::Pirate);
        let cap = user(3, "cap", Role::Captain);
        let rooms = open(dir.path()).await;

        let created = rooms
            .create(&alice, "#Techno", Some("4/4 only"), None)
            .await
            .unwrap();
        let id = created.room.id;
        assert_eq!(created.room.name, "techno");
        assert!(created.room.mine && created.room.joined);
        assert!(matches!(
            rooms.create(&bob, "TECHNO", None, None).await,
            Err(RoomError::NameTaken(_))
        ));

        // Members only.
        assert!(matches!(
            rooms.view(&bob, id, None).await,
            Err(RoomError::NotMember)
        ));
        assert!(matches!(
            rooms.post(&bob, id, "hi").await,
            Err(RoomError::NotMember)
        ));
        rooms.join(&bob, id).await.unwrap();
        let sent = rooms
            .post(&bob, id, "/me drops a white label")
            .await
            .unwrap();
        assert_eq!(sent.kind, MessageKind::Action);
        assert_eq!(sent.text, "drops a white label");
        rooms
            .post(&bob, id, "anyone got the B-side?")
            .await
            .unwrap();

        // Unread for alice, cleared by viewing; incremental fetch.
        assert_eq!(rooms.unread_total(&alice).await, 2);
        let seen = rooms.view(&alice, id, None).await.unwrap();
        assert_eq!(rooms.unread_total(&alice).await, 0);
        let last = seen.messages.last().unwrap().seq;
        assert!(
            rooms
                .view(&alice, id, Some(last))
                .await
                .unwrap()
                .messages
                .is_empty()
        );
        assert_eq!(seen.users.len(), 2);
        assert!(seen.users.iter().any(|u| u.name == "alice" && u.owner));

        // Only the owner (or a captain) moderates.
        assert!(matches!(
            rooms.set_topic(&bob, id, Some("x")).await,
            Err(RoomError::NotOwner)
        ));
        assert!(matches!(
            rooms.delete(&bob, id).await,
            Err(RoomError::NotOwner)
        ));
        rooms
            .set_topic(&alice, id, Some("deep cuts"))
            .await
            .unwrap();

        // History survives a restart; presence does not.
        drop(rooms);
        let rooms = open(dir.path()).await;
        let view = rooms.view(&bob, id, None).await.unwrap();
        assert_eq!(view.room.topic.as_deref(), Some("deep cuts"));
        assert!(
            view.messages
                .iter()
                .any(|m| m.text == "anyone got the B-side?")
        );

        rooms.leave(&bob, id).await.unwrap();
        assert!(matches!(
            rooms.view(&bob, id, None).await,
            Err(RoomError::NotMember)
        ));
        rooms.delete(&cap, id).await.unwrap();
        assert!(rooms.list(&alice).await.is_empty());
        let reopened = open(dir.path()).await;
        assert!(reopened.list(&alice).await.is_empty());
    }

    #[tokio::test]
    async fn flood_and_owner_limits() {
        let dir = tempfile::tempdir().unwrap();
        let alice = user(1, "alice", Role::Pirate);
        let rooms = open(dir.path()).await;
        let id = rooms
            .create(&alice, "spam", None, None)
            .await
            .unwrap()
            .room
            .id;
        for i in 0..FLOOD_MAX {
            rooms.post(&alice, id, &format!("msg {i}")).await.unwrap();
        }
        assert!(matches!(
            rooms.post(&alice, id, "one more").await,
            Err(RoomError::Limit(_))
        ));
        for i in 1..MAX_OWNED_ROOMS {
            rooms
                .create(&alice, &format!("room{i}"), None, None)
                .await
                .unwrap();
        }
        assert!(matches!(
            rooms.create(&alice, "toomany", None, None).await,
            Err(RoomError::Limit(_))
        ));
    }

    #[tokio::test]
    async fn tracks_sent_together_count_as_one_message() {
        use crate::domain::provider::ProviderKind;
        use crate::domain::quality::Codec;

        let dir = tempfile::tempdir().unwrap();
        let alice = user(1, "alice", Role::Pirate);
        let rooms = open(dir.path()).await;
        let id = rooms
            .create(&alice, "crate", None, None)
            .await
            .unwrap()
            .room
            .id;
        let track = |n: usize| SharedTrack {
            key: TrackKey {
                provider: ProviderKind::SoundCloud,
                id: n.to_string(),
            },
            output: OutputFormat::Original,
            title: format!("Track {n}"),
            artists: vec!["Someone".into()],
            album: None,
            artwork_url: None,
            source_url: format!("https://soundcloud.com/someone/{n}"),
            quality: QualityReport::new(Codec::Aac, "m4a".into(), Some(160), None, None, None),
        };
        let tracks: Vec<_> = (0..MAX_SHARED_AT_ONCE).map(track).collect();
        let sent = rooms.share_tracks(&alice, id, tracks).await.unwrap();
        assert_eq!(sent.len(), MAX_SHARED_AT_ONCE);
        assert!(sent.windows(2).all(|w| w[1].seq == w[0].seq + 1));
        // One flood slot used: the rest of the window is still free.
        for i in 1..FLOOD_MAX {
            rooms.post(&alice, id, &format!("msg {i}")).await.unwrap();
        }
        let too_many: Vec<_> = (0..=MAX_SHARED_AT_ONCE).map(track).collect();
        assert!(matches!(
            rooms.share_tracks(&alice, id, too_many).await,
            Err(RoomError::Limit(_))
        ));
        assert!(matches!(
            rooms.share_tracks(&alice, id, Vec::new()).await,
            Err(RoomError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn history_is_bounded_and_deleted_users_leave() {
        let dir = tempfile::tempdir().unwrap();
        let alice = user(1, "alice", Role::Pirate);
        let bob = user(2, "bob", Role::Pirate);
        let rooms = open(dir.path()).await;
        let id = rooms
            .create(&alice, "big", None, None)
            .await
            .unwrap()
            .room
            .id;
        rooms.join(&bob, id).await.unwrap();
        {
            let mut inner = rooms.inner.lock().await;
            let room = inner.room_mut(id).unwrap();
            for _ in 0..MAX_HISTORY + 10 {
                room.push(&alice, MessageKind::Text, "x".into());
            }
            assert_eq!(room.messages.len(), MAX_HISTORY);
        }
        assert_eq!(
            rooms.view(&alice, id, None).await.unwrap().messages.len(),
            RECENT_MESSAGES
        );

        rooms.forget_user(bob.id).await;
        let view = rooms.view(&alice, id, None).await.unwrap();
        assert_eq!(view.users.len(), 1);
    }

    #[tokio::test]
    async fn shared_tracks_are_for_members_only() {
        use crate::domain::provider::ProviderKind;
        use crate::domain::quality::Codec;

        let dir = tempfile::tempdir().unwrap();
        let alice = user(1, "alice", Role::Pirate);
        let bob = user(2, "bob", Role::Pirate);
        let rooms = open(dir.path()).await;
        let id = rooms
            .create(&alice, "ids", None, None)
            .await
            .unwrap()
            .room
            .id;
        let track = SharedTrack {
            key: TrackKey {
                provider: ProviderKind::SoundCloud,
                id: "123".into(),
            },
            output: OutputFormat::Original,
            title: "Unreleased ID".into(),
            artists: vec!["Someone".into()],
            album: None,
            artwork_url: None,
            source_url: "https://soundcloud.com/someone/id".into(),
            quality: QualityReport::new(Codec::Aac, "m4a".into(), Some(160), None, None, None),
        };
        let sent = rooms.share_track(&alice, id, track.clone()).await.unwrap();
        assert_eq!(sent.kind, MessageKind::Track);

        // Not a member yet: no access, and sharing needs membership too.
        assert!(matches!(
            rooms.shared_track(&bob, id, sent.seq).await,
            Err(RoomError::NotMember)
        ));
        assert!(matches!(
            rooms.share_track(&bob, id, track.clone()).await,
            Err(RoomError::NotMember)
        ));
        rooms.join(&bob, id).await.unwrap();
        assert_eq!(rooms.shared_track(&bob, id, sent.seq).await.unwrap(), track);
        assert_eq!(rooms.unread_total(&bob).await, 0);
        // Only track messages grant anything.
        assert!(matches!(
            rooms.shared_track(&bob, id, 1).await,
            Err(RoomError::NoSuchTrack)
        ));

        // Survives a restart; ends with the room.
        drop(rooms);
        let rooms = open(dir.path()).await;
        let view = rooms.view(&bob, id, None).await.unwrap();
        let shared = view.messages.iter().find(|m| m.seq == sent.seq).unwrap();
        assert_eq!(shared.track.as_ref(), Some(&track));
        rooms.delete(&alice, id).await.unwrap();
        assert!(matches!(
            rooms.shared_track(&bob, id, sent.seq).await,
            Err(RoomError::NotFound)
        ));
    }

    // Weak on purpose: room passwords have no strength rule.
    const ROOM_PASSWORD: &str = "vinyl";

    #[tokio::test]
    async fn private_rooms_are_hidden_and_joined_by_name_and_password() {
        let dir = tempfile::tempdir().unwrap();
        let alice = user(1, "alice", Role::Pirate);
        let bob = user(2, "bob", Role::Pirate);
        let cap = user(3, "cap", Role::Captain);
        let rooms = open(dir.path()).await;

        for bad in ["", "tab\there", &"x".repeat(MAX_PASSWORD_CHARS + 1)] {
            assert!(matches!(
                rooms.create(&alice, "vault", None, Some(bad)).await,
                Err(RoomError::InvalidInput(_))
            ));
        }
        let created = rooms
            .create(&alice, "vault", None, Some(ROOM_PASSWORD))
            .await
            .unwrap();
        let id = created.room.id;
        assert!(created.room.private && created.room.joined);
        // The creator is in without typing the password again.
        rooms.post(&alice, id, "dubplates only").await.unwrap();

        // Invisible to non-members, captains included, and by id it looks
        // like no room at all.
        assert_eq!(rooms.list(&alice).await.len(), 1);
        assert!(rooms.list(&bob).await.is_empty());
        assert!(rooms.list(&cap).await.is_empty());
        for result in [
            rooms.join(&bob, id).await.map(|_| ()),
            rooms.view(&bob, id, None).await.map(|_| ()),
            rooms.post(&bob, id, "hi").await.map(|_| ()),
            rooms.delete(&cap, id).await,
            rooms.set_topic(&cap, id, Some("x")).await.map(|_| ()),
        ] {
            assert!(matches!(result, Err(RoomError::NotFound)), "{result:?}");
        }

        // Unknown name and wrong password answer the same.
        for (name, secret) in [("vault", "wrong horse battery"), ("nope", ROOM_PASSWORD)] {
            assert!(matches!(
                rooms.join_private(&bob, name, secret).await,
                Err(RoomError::BadCredentials)
            ));
        }
        // A public room cannot be joined as a private one.
        rooms.create(&alice, "open", None, None).await.unwrap();
        assert!(matches!(
            rooms.join_private(&bob, "open", ROOM_PASSWORD).await,
            Err(RoomError::BadCredentials)
        ));

        let joined = rooms
            .join_private(&bob, "#Vault", ROOM_PASSWORD)
            .await
            .unwrap();
        assert!(joined.messages.iter().any(|m| m.text == "dubplates only"));
        // Membership survives a restart; no password needed afterwards.
        drop(rooms);
        let rooms = open(dir.path()).await;
        assert!(
            rooms
                .list(&bob)
                .await
                .iter()
                .any(|r| r.id == id && r.private)
        );
        rooms.view(&bob, id, None).await.unwrap();

        // Once everyone has left, the room is gone, on disk too.
        rooms.leave(&alice, id).await.unwrap();
        rooms.view(&bob, id, None).await.unwrap();
        rooms.leave(&bob, id).await.unwrap();
        drop(rooms);
        let rooms = open(dir.path()).await;
        assert!(matches!(
            rooms.join_private(&bob, "vault", ROOM_PASSWORD).await,
            Err(RoomError::BadCredentials)
        ));
        assert!(rooms.create(&bob, "vault", None, None).await.is_ok());
    }

    #[tokio::test]
    async fn private_room_guessing_is_throttled() {
        let dir = tempfile::tempdir().unwrap();
        let alice = user(1, "alice", Role::Pirate);
        let bob = user(2, "bob", Role::Pirate);
        let rooms = open(dir.path()).await;
        rooms
            .create(&alice, "vault", None, Some(ROOM_PASSWORD))
            .await
            .unwrap();
        for _ in 0..JOIN_FAILURES_MAX {
            assert!(matches!(
                rooms
                    .join_private(&bob, "vault", "wrong horse battery")
                    .await,
                Err(RoomError::BadCredentials)
            ));
        }
        // Even the right password waits out the window now.
        assert!(matches!(
            rooms.join_private(&bob, "vault", ROOM_PASSWORD).await,
            Err(RoomError::Throttled)
        ));
        // Others are not affected; a right password costs no slot.
        let carol = user(3, "carol", Role::Pirate);
        rooms
            .join_private(&carol, "vault", ROOM_PASSWORD)
            .await
            .unwrap();
        assert!(rooms.inner.lock().await.join_failures[&carol.id].is_empty());
    }

    #[tokio::test]
    async fn corrupt_room_file_stops_startup() {
        let dir = tempfile::tempdir().unwrap();
        let rooms_dir = dir.path().join("rooms");
        std::fs::create_dir_all(&rooms_dir).unwrap();
        std::fs::write(rooms_dir.join("42.json"), b"{oops").unwrap();
        // Leftover temp files from an interrupted save are ignored.
        std::fs::write(rooms_dir.join("7.json.tmp"), b"{partial").unwrap();
        assert!(matches!(
            Rooms::open(rooms_dir.clone()).await,
            Err(RoomError::Store(StoreError::Corrupt { .. }))
        ));
        assert_eq!(std::fs::read(rooms_dir.join("42.json")).unwrap(), b"{oops");
    }
}
