//! Typed Soulseek protocol messages (server, peer init, peer).
//!
//! Only the messages Inkult uses are modelled; anything else decodes to an
//! `Other`/`Unknown` variant and is ignored. Field order and semantics follow
//! the Nicotine+ protocol documentation (`doc/SLSKPROTOCOL.md`).

use std::net::Ipv4Addr;

use super::wire::{Reader, WireError, Writer, md5_hex, zlib_compress, zlib_decompress};

/// Our client identity. 177 is the major version the protocol documentation
/// sets aside for new and experimental clients, so Inkult never impersonates
/// an established client; the minor version is Inkult's own.
pub const MAJOR_VERSION: u32 = 177;
pub const MINOR_VERSION: u32 = 1;

/// Largest server message accepted. Server messages are small; the biggest
/// (room lists, privileged user lists) stay well below this.
pub const MAX_SERVER_FRAME: usize = 16 * 1024 * 1024;
/// Largest compressed share list / folder listing accepted from a peer.
pub const MAX_SHARES_FRAME: usize = 64 * 1024 * 1024;
/// Largest inflated share list (roughly a million files).
pub const MAX_SHARES_INFLATED: usize = 256 * 1024 * 1024;
/// Largest search response, compressed and inflated.
pub const MAX_SEARCH_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_SEARCH_INFLATED: usize = 16 * 1024 * 1024;
/// Largest user info response (it carries a picture).
pub const MAX_INFO_FRAME: usize = 4 * 1024 * 1024;
/// Any other peer message.
pub const MAX_PEER_FRAME: usize = 256 * 1024;

/// Kind of peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnKind {
    /// Peer messages.
    Peer,
    /// A file transfer.
    File,
    /// Distributed search network (Inkult does not take part).
    Distributed,
}

impl ConnKind {
    pub fn code(self) -> &'static str {
        match self {
            ConnKind::Peer => "P",
            ConnKind::File => "F",
            ConnKind::Distributed => "D",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "P" => Some(ConnKind::Peer),
            "F" => Some(ConnKind::File),
            "D" => Some(ConnKind::Distributed),
            _ => None,
        }
    }
}

/// A user's presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Offline,
    Away,
    Online,
}

impl UserStatus {
    fn from_code(code: u32) -> Self {
        match code {
            1 => UserStatus::Away,
            2 => UserStatus::Online,
            _ => UserStatus::Offline,
        }
    }
}

// ---------- server ----------

pub mod server_code {
    pub const LOGIN: u32 = 1;
    pub const SET_WAIT_PORT: u32 = 2;
    pub const GET_PEER_ADDRESS: u32 = 3;
    pub const GET_USER_STATUS: u32 = 7;
    pub const CONNECT_TO_PEER: u32 = 18;
    pub const MESSAGE_USER: u32 = 22;
    pub const MESSAGE_ACKED: u32 = 23;
    pub const FILE_SEARCH: u32 = 26;
    pub const SET_STATUS: u32 = 28;
    pub const SERVER_PING: u32 = 32;
    pub const SHARED_FOLDERS_FILES: u32 = 35;
    pub const GET_USER_STATS: u32 = 36;
    pub const RELOGGED: u32 = 41;
    pub const USER_SEARCH: u32 = 42;
    pub const ADMIN_MESSAGE: u32 = 66;
    pub const WISHLIST_SEARCH: u32 = 103;
    pub const WISHLIST_INTERVAL: u32 = 104;
    pub const SEND_UPLOAD_SPEED: u32 = 121;
    pub const CANT_CONNECT_TO_PEER: u32 = 1001;
}

/// Messages we send to the server.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerRequest {
    Login {
        username: String,
        password: String,
    },
    SetWaitPort(u32),
    GetPeerAddress(String),
    GetUserStatus(String),
    ConnectToPeer {
        token: u32,
        username: String,
        kind: ConnKind,
    },
    MessageUser {
        username: String,
        message: String,
    },
    MessageAcked(u32),
    FileSearch {
        token: u32,
        query: String,
    },
    UserSearch {
        username: String,
        token: u32,
        query: String,
    },
    WishlistSearch {
        token: u32,
        query: String,
    },
    /// 1 = away, 2 = online.
    SetStatus(u32),
    ServerPing,
    SharedFoldersFiles {
        dirs: u32,
        files: u32,
    },
    GetUserStats(String),
    SendUploadSpeed(u32),
    CantConnectToPeer {
        token: u32,
        username: String,
    },
}

impl ServerRequest {
    /// The complete frame, ready to write to the server socket.
    pub fn encode(&self) -> Vec<u8> {
        use server_code::*;
        let mut w = Writer::new();
        let code = match self {
            ServerRequest::Login { username, password } => {
                w.str(username)
                    .str(password)
                    .u32(MAJOR_VERSION)
                    .str(&md5_hex(&format!("{username}{password}")))
                    .u32(MINOR_VERSION);
                LOGIN
            }
            ServerRequest::SetWaitPort(port) => {
                w.u32(*port);
                SET_WAIT_PORT
            }
            ServerRequest::GetPeerAddress(user) => {
                w.str(user);
                GET_PEER_ADDRESS
            }
            ServerRequest::GetUserStatus(user) => {
                w.str(user);
                GET_USER_STATUS
            }
            ServerRequest::ConnectToPeer {
                token,
                username,
                kind,
            } => {
                w.u32(*token).str(username).str(kind.code());
                CONNECT_TO_PEER
            }
            ServerRequest::MessageUser { username, message } => {
                w.str(username).str(message);
                MESSAGE_USER
            }
            ServerRequest::MessageAcked(id) => {
                w.u32(*id);
                MESSAGE_ACKED
            }
            ServerRequest::FileSearch { token, query } => {
                w.u32(*token).str(query);
                FILE_SEARCH
            }
            ServerRequest::UserSearch {
                username,
                token,
                query,
            } => {
                w.str(username).u32(*token).str(query);
                USER_SEARCH
            }
            ServerRequest::WishlistSearch { token, query } => {
                w.u32(*token).str(query);
                WISHLIST_SEARCH
            }
            ServerRequest::SetStatus(status) => {
                w.u32(*status);
                SET_STATUS
            }
            ServerRequest::ServerPing => SERVER_PING,
            ServerRequest::SharedFoldersFiles { dirs, files } => {
                w.u32(*dirs).u32(*files);
                SHARED_FOLDERS_FILES
            }
            ServerRequest::GetUserStats(user) => {
                w.str(user);
                GET_USER_STATS
            }
            ServerRequest::SendUploadSpeed(speed) => {
                w.u32(*speed);
                SEND_UPLOAD_SPEED
            }
            ServerRequest::CantConnectToPeer { token, username } => {
                w.u32(*token).str(username);
                CANT_CONNECT_TO_PEER
            }
        };
        Writer::frame_u32(code, &w.into_inner())
    }
}

/// Messages the server sends us.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    LoginOk {
        greeting: String,
        own_ip: Ipv4Addr,
    },
    LoginRejected {
        reason: String,
        detail: Option<String>,
    },
    PeerAddress {
        username: String,
        ip: Ipv4Addr,
        port: u32,
    },
    UserStatus {
        username: String,
        status: UserStatus,
        privileged: bool,
    },
    ConnectToPeer {
        username: String,
        kind: Option<ConnKind>,
        ip: Ipv4Addr,
        port: u32,
        token: u32,
    },
    PrivateMessage {
        id: u32,
        timestamp: u32,
        username: String,
        message: String,
    },
    /// Someone searches our shares (a user search aimed at us).
    FileSearch {
        username: String,
        token: u32,
        query: String,
    },
    UserStats {
        username: String,
        avg_speed: u32,
        uploads: u32,
        files: u32,
        dirs: u32,
    },
    /// Someone else logged in with our account; the server disconnects us.
    Relogged,
    AdminMessage(String),
    WishlistInterval(u32),
    CantConnectToPeer {
        token: u32,
    },
    Other(u32),
}

fn ip(raw: u32) -> Ipv4Addr {
    // The address travels as a little-endian integer whose most significant
    // byte is the first octet.
    Ipv4Addr::from(raw)
}

pub fn decode_server(code: u32, body: &[u8]) -> Result<ServerEvent, WireError> {
    use server_code::*;
    let mut r = Reader::new(body);
    Ok(match code {
        LOGIN => {
            if r.bool()? {
                let greeting = r.string()?;
                let own_ip = ip(r.u32()?);
                ServerEvent::LoginOk { greeting, own_ip }
            } else {
                let reason = r.string()?;
                let detail = r.string().ok();
                ServerEvent::LoginRejected { reason, detail }
            }
        }
        GET_PEER_ADDRESS => ServerEvent::PeerAddress {
            username: r.string()?,
            ip: ip(r.u32()?),
            port: r.u32()?,
        },
        GET_USER_STATUS => ServerEvent::UserStatus {
            username: r.string()?,
            status: UserStatus::from_code(r.u32()?),
            privileged: r.bool().unwrap_or(false),
        },
        CONNECT_TO_PEER => ServerEvent::ConnectToPeer {
            username: r.string()?,
            kind: ConnKind::parse(&r.string()?),
            ip: ip(r.u32()?),
            port: r.u32()?,
            token: r.u32()?,
        },
        MESSAGE_USER => ServerEvent::PrivateMessage {
            id: r.u32()?,
            timestamp: r.u32()?,
            username: r.string()?,
            message: r.string()?,
        },
        FILE_SEARCH => ServerEvent::FileSearch {
            username: r.string()?,
            token: r.u32()?,
            query: r.string()?,
        },
        GET_USER_STATS => {
            let username = r.string()?;
            let avg_speed = r.u32()?;
            let uploads = r.u32()?;
            let _unknown = r.u32()?;
            ServerEvent::UserStats {
                username,
                avg_speed,
                uploads,
                files: r.u32()?,
                dirs: r.u32()?,
            }
        }
        RELOGGED => ServerEvent::Relogged,
        ADMIN_MESSAGE => ServerEvent::AdminMessage(r.string()?),
        WISHLIST_INTERVAL => ServerEvent::WishlistInterval(r.u32()?),
        CANT_CONNECT_TO_PEER => ServerEvent::CantConnectToPeer { token: r.u32()? },
        other => ServerEvent::Other(other),
    })
}

// ---------- peer init ----------

/// The first message on a new peer connection.
#[derive(Debug, Clone, PartialEq)]
pub enum PeerInit {
    /// Answer to our indirect connection request (token from ConnectToPeer).
    PierceFirewall(u32),
    /// A direct connection from `username`.
    PeerInit {
        username: String,
        kind: Option<ConnKind>,
    },
}

pub const MAX_INIT_FRAME: usize = 4096;

impl PeerInit {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            PeerInit::PierceFirewall(token) => {
                w.u32(*token);
                Writer::frame_u8(0, &w.into_inner())
            }
            PeerInit::PeerInit { username, kind } => {
                w.str(username).str(kind.map_or("P", ConnKind::code)).u32(0);
                Writer::frame_u8(1, &w.into_inner())
            }
        }
    }

    /// Decode a frame body (the code byte included).
    pub fn decode(frame: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(frame);
        match r.u8()? {
            0 => Ok(PeerInit::PierceFirewall(r.u32()?)),
            1 => Ok(PeerInit::PeerInit {
                username: r.string()?,
                kind: ConnKind::parse(&r.string()?),
            }),
            other => Err(WireError::Invalid(format!(
                "unknown peer init message {other}"
            ))),
        }
    }
}

// ---------- peer ----------

pub mod peer_code {
    pub const GET_SHARE_FILE_LIST: u32 = 4;
    pub const SHARED_FILE_LIST: u32 = 5;
    pub const FILE_SEARCH_RESPONSE: u32 = 9;
    pub const USER_INFO_REQUEST: u32 = 15;
    pub const USER_INFO_RESPONSE: u32 = 16;
    pub const FOLDER_CONTENTS_REQUEST: u32 = 36;
    pub const FOLDER_CONTENTS_RESPONSE: u32 = 37;
    pub const TRANSFER_REQUEST: u32 = 40;
    pub const TRANSFER_RESPONSE: u32 = 41;
    pub const QUEUE_UPLOAD: u32 = 43;
    pub const PLACE_IN_QUEUE_RESPONSE: u32 = 44;
    pub const UPLOAD_FAILED: u32 = 46;
    pub const UPLOAD_DENIED: u32 = 50;
    pub const PLACE_IN_QUEUE_REQUEST: u32 = 51;
}

/// Size limit for an incoming peer frame, by message code.
pub fn peer_frame_limit(code: u32) -> usize {
    use peer_code::*;
    match code {
        SHARED_FILE_LIST | FOLDER_CONTENTS_RESPONSE => MAX_SHARES_FRAME,
        FILE_SEARCH_RESPONSE => MAX_SEARCH_FRAME,
        USER_INFO_RESPONSE => MAX_INFO_FRAME,
        _ => MAX_PEER_FRAME,
    }
}

/// Audio properties a peer reports for a shared file.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileAttrs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vbr: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
}

/// A file in a peer's share. `name` is the peer's full virtual path
/// (backslash separated) in search results, and only the file name within
/// its directory in share and folder listings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SharedFile {
    pub name: String,
    pub size: u64,
    pub attrs: FileAttrs,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SharedDir {
    pub path: String,
    pub files: Vec<SharedFile>,
}

/// One peer's answer to a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReply {
    pub username: String,
    pub token: u32,
    pub files: Vec<SharedFile>,
    pub slot_free: bool,
    pub avg_speed: u32,
    pub queue_length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub description: String,
    pub picture: Option<Vec<u8>>,
    pub total_uploads: u32,
    pub queue_size: u32,
    pub slots_free: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PeerMessage {
    GetShareFileList,
    SharedFileList(Vec<SharedDir>),
    SearchReply(SearchReply),
    UserInfoRequest,
    UserInfo(UserInfo),
    FolderContentsRequest {
        token: u32,
        folder: String,
    },
    FolderContents {
        token: u32,
        folder: String,
        dirs: Vec<SharedDir>,
    },
    /// direction 0 = they want to download from us (legacy request),
    /// 1 = they are ready to upload to us.
    TransferRequest {
        direction: u32,
        token: u32,
        filename: String,
        size: Option<u64>,
    },
    TransferResponse {
        token: u32,
        allowed: bool,
        reason: Option<String>,
    },
    QueueUpload {
        filename: String,
    },
    PlaceInQueueResponse {
        filename: String,
        place: u32,
    },
    UploadFailed {
        filename: String,
    },
    UploadDenied {
        filename: String,
        reason: String,
    },
    PlaceInQueueRequest {
        filename: String,
    },
    Unknown(u32),
}

fn write_file(w: &mut Writer, file: &SharedFile) {
    let extension = file.name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    w.u8(1).str(&file.name).u64(file.size).str(extension);
    let attrs: Vec<(u32, u32)> = [
        (0, file.attrs.bitrate_kbps),
        (1, file.attrs.duration_secs),
        (2, file.attrs.vbr.map(u32::from)),
        (4, file.attrs.sample_rate_hz),
        (5, file.attrs.bit_depth),
    ]
    .into_iter()
    .filter_map(|(code, value)| value.map(|v| (code, v)))
    .collect();
    w.u32(attrs.len() as u32);
    for (code, value) in attrs {
        w.u32(code).u32(value);
    }
}

/// Smallest possible encoded file entry: code, empty name, size, empty
/// extension, no attributes.
const MIN_FILE_BYTES: usize = 1 + 4 + 8 + 4 + 4;

fn read_file(r: &mut Reader) -> Result<SharedFile, WireError> {
    let _code = r.u8()?;
    let name = r.string()?;
    let size = r.u64()?;
    let _extension = r.bytes()?;
    let mut attrs = FileAttrs::default();
    for _ in 0..r.count(8)? {
        let (code, value) = (r.u32()?, r.u32()?);
        match code {
            0 => attrs.bitrate_kbps = Some(value),
            1 => attrs.duration_secs = Some(value),
            2 => attrs.vbr = Some(value != 0),
            4 => attrs.sample_rate_hz = Some(value),
            5 => attrs.bit_depth = Some(value),
            _ => {}
        }
    }
    Ok(SharedFile { name, size, attrs })
}

fn read_files(r: &mut Reader) -> Result<Vec<SharedFile>, WireError> {
    let n = r.count(MIN_FILE_BYTES)?;
    let mut files = Vec::with_capacity(n);
    for _ in 0..n {
        files.push(read_file(r)?);
    }
    Ok(files)
}

fn read_dirs(r: &mut Reader) -> Result<Vec<SharedDir>, WireError> {
    let n = r.count(8)?;
    let mut dirs = Vec::with_capacity(n);
    for _ in 0..n {
        let path = r.string()?;
        dirs.push(SharedDir {
            path,
            files: read_files(r)?,
        });
    }
    Ok(dirs)
}

fn write_dirs(w: &mut Writer, dirs: &[SharedDir]) {
    w.u32(dirs.len() as u32);
    for dir in dirs {
        w.str(&dir.path).u32(dir.files.len() as u32);
        for file in &dir.files {
            write_file(w, file);
        }
    }
}

impl PeerMessage {
    pub fn encode(&self) -> Vec<u8> {
        use peer_code::*;
        let mut w = Writer::new();
        let code = match self {
            PeerMessage::GetShareFileList => GET_SHARE_FILE_LIST,
            PeerMessage::SharedFileList(dirs) => {
                let mut inner = Writer::new();
                write_dirs(&mut inner, dirs);
                inner.u32(0).u32(0); // unknown, no private directories
                w.raw(&zlib_compress(&inner.into_inner()));
                SHARED_FILE_LIST
            }
            PeerMessage::SearchReply(reply) => {
                let mut inner = Writer::new();
                inner
                    .str(&reply.username)
                    .u32(reply.token)
                    .u32(reply.files.len() as u32);
                for file in &reply.files {
                    write_file(&mut inner, file);
                }
                inner
                    .bool(reply.slot_free)
                    .u32(reply.avg_speed)
                    .u32(reply.queue_length)
                    .u32(0)
                    .u32(0);
                w.raw(&zlib_compress(&inner.into_inner()));
                FILE_SEARCH_RESPONSE
            }
            PeerMessage::UserInfoRequest => USER_INFO_REQUEST,
            PeerMessage::UserInfo(info) => {
                w.str(&info.description);
                match &info.picture {
                    Some(picture) => w.bool(true).bytes(picture),
                    None => w.bool(false),
                };
                w.u32(info.total_uploads)
                    .u32(info.queue_size)
                    .bool(info.slots_free)
                    .u32(1); // anyone may upload to us (we never accept uploads anyway)
                USER_INFO_RESPONSE
            }
            PeerMessage::FolderContentsRequest { token, folder } => {
                w.u32(*token).str(folder);
                FOLDER_CONTENTS_REQUEST
            }
            PeerMessage::FolderContents {
                token,
                folder,
                dirs,
            } => {
                let mut inner = Writer::new();
                inner.u32(*token).str(folder);
                write_dirs(&mut inner, dirs);
                w.raw(&zlib_compress(&inner.into_inner()));
                FOLDER_CONTENTS_RESPONSE
            }
            PeerMessage::TransferRequest {
                direction,
                token,
                filename,
                size,
            } => {
                w.u32(*direction).u32(*token).str(filename);
                if let Some(size) = size {
                    w.u64(*size);
                }
                TRANSFER_REQUEST
            }
            PeerMessage::TransferResponse {
                token,
                allowed,
                reason,
            } => {
                w.u32(*token).bool(*allowed);
                if let Some(reason) = reason.as_deref().filter(|_| !allowed) {
                    w.str(reason);
                }
                TRANSFER_RESPONSE
            }
            PeerMessage::QueueUpload { filename } => {
                w.str(filename);
                QUEUE_UPLOAD
            }
            PeerMessage::PlaceInQueueResponse { filename, place } => {
                w.str(filename).u32(*place);
                PLACE_IN_QUEUE_RESPONSE
            }
            PeerMessage::UploadFailed { filename } => {
                w.str(filename);
                UPLOAD_FAILED
            }
            PeerMessage::UploadDenied { filename, reason } => {
                w.str(filename).str(reason);
                UPLOAD_DENIED
            }
            PeerMessage::PlaceInQueueRequest { filename } => {
                w.str(filename);
                PLACE_IN_QUEUE_REQUEST
            }
            PeerMessage::Unknown(code) => *code,
        };
        Writer::frame_u32(code, &w.into_inner())
    }

    pub fn decode(code: u32, body: &[u8]) -> Result<Self, WireError> {
        use peer_code::*;
        let mut r = Reader::new(body);
        Ok(match code {
            GET_SHARE_FILE_LIST => PeerMessage::GetShareFileList,
            SHARED_FILE_LIST => {
                let data = zlib_decompress(body, MAX_SHARES_INFLATED)?;
                let mut r = Reader::new(&data);
                let mut dirs = read_dirs(&mut r)?;
                // Private ("buddies only") directories follow; they are
                // listed but downloads from them are usually refused.
                if r.u32().is_ok()
                    && let Ok(private) = read_dirs(&mut r)
                {
                    dirs.extend(private);
                }
                PeerMessage::SharedFileList(dirs)
            }
            FILE_SEARCH_RESPONSE => {
                let data = zlib_decompress(body, MAX_SEARCH_INFLATED)?;
                let mut r = Reader::new(&data);
                let username = r.string()?;
                let token = r.u32()?;
                let files = read_files(&mut r)?;
                let slot_free = r.bool()?;
                let avg_speed = r.u32()?;
                let queue_length = r.u32()?;
                PeerMessage::SearchReply(SearchReply {
                    username,
                    token,
                    files,
                    slot_free,
                    avg_speed,
                    queue_length,
                })
            }
            USER_INFO_REQUEST => PeerMessage::UserInfoRequest,
            USER_INFO_RESPONSE => {
                let description = r.string()?;
                let picture = if r.bool()? {
                    Some(r.bytes()?.to_vec())
                } else {
                    None
                };
                PeerMessage::UserInfo(UserInfo {
                    description,
                    picture,
                    total_uploads: r.u32()?,
                    queue_size: r.u32()?,
                    slots_free: r.bool()?,
                })
            }
            FOLDER_CONTENTS_REQUEST => PeerMessage::FolderContentsRequest {
                token: r.u32()?,
                folder: r.string()?,
            },
            FOLDER_CONTENTS_RESPONSE => {
                let data = zlib_decompress(body, MAX_SHARES_INFLATED)?;
                let mut r = Reader::new(&data);
                PeerMessage::FolderContents {
                    token: r.u32()?,
                    folder: r.string()?,
                    dirs: read_dirs(&mut r)?,
                }
            }
            TRANSFER_REQUEST => {
                let direction = r.u32()?;
                let token = r.u32()?;
                let filename = r.string()?;
                let size = if direction == 1 { Some(r.u64()?) } else { None };
                PeerMessage::TransferRequest {
                    direction,
                    token,
                    filename,
                    size,
                }
            }
            TRANSFER_RESPONSE => {
                let token = r.u32()?;
                let allowed = r.bool()?;
                // An allowed legacy download response carries a file size,
                // which we do not need; a rejection carries the reason.
                let reason = if allowed { None } else { r.string().ok() };
                PeerMessage::TransferResponse {
                    token,
                    allowed,
                    reason,
                }
            }
            QUEUE_UPLOAD => PeerMessage::QueueUpload {
                filename: r.string()?,
            },
            PLACE_IN_QUEUE_RESPONSE => PeerMessage::PlaceInQueueResponse {
                filename: r.string()?,
                place: r.u32()?,
            },
            UPLOAD_FAILED => PeerMessage::UploadFailed {
                filename: r.string()?,
            },
            UPLOAD_DENIED => PeerMessage::UploadDenied {
                filename: r.string()?,
                reason: r.string()?,
            },
            PLACE_IN_QUEUE_REQUEST => PeerMessage::PlaceInQueueRequest {
                filename: r.string()?,
            },
            other => PeerMessage::Unknown(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soulseek::wire::read_coded_frame;

    async fn round_trip(message: PeerMessage) -> PeerMessage {
        let frame = message.encode();
        let (code, body) = read_coded_frame(&mut frame.as_slice(), peer_frame_limit)
            .await
            .unwrap();
        PeerMessage::decode(code, &body).unwrap()
    }

    fn flac(name: &str) -> SharedFile {
        SharedFile {
            name: name.into(),
            size: 31_337_000,
            attrs: FileAttrs {
                duration_secs: Some(245),
                sample_rate_hz: Some(44_100),
                bit_depth: Some(16),
                ..FileAttrs::default()
            },
        }
    }

    #[test]
    fn login_frame_matches_documented_layout() {
        let frame = ServerRequest::Login {
            username: "username".into(),
            password: "password".into(),
        }
        .encode();
        let mut r = Reader::new(&frame);
        assert_eq!(r.u32().unwrap() as usize, frame.len() - 4);
        assert_eq!(r.u32().unwrap(), server_code::LOGIN);
        assert_eq!(r.string().unwrap(), "username");
        assert_eq!(r.string().unwrap(), "password");
        assert_eq!(r.u32().unwrap(), MAJOR_VERSION);
        assert_eq!(r.string().unwrap(), "d51c9a7e9353746a6020f9602d452929");
        assert_eq!(r.u32().unwrap(), MINOR_VERSION);
    }

    #[test]
    fn decodes_server_events() {
        let mut w = Writer::new();
        w.bool(true)
            .str("welcome")
            .u32(0x0A00_0001)
            .str("hash")
            .bool(false);
        assert_eq!(
            decode_server(server_code::LOGIN, &w.into_inner()).unwrap(),
            ServerEvent::LoginOk {
                greeting: "welcome".into(),
                own_ip: Ipv4Addr::new(10, 0, 0, 1)
            }
        );

        let mut w = Writer::new();
        w.bool(false).str("INVALIDPASS");
        assert_eq!(
            decode_server(server_code::LOGIN, &w.into_inner()).unwrap(),
            ServerEvent::LoginRejected {
                reason: "INVALIDPASS".into(),
                detail: None
            }
        );

        let mut w = Writer::new();
        w.str("bob")
            .str("F")
            .u32(0xC0A8_0102)
            .u32(2234)
            .u32(77)
            .bool(false);
        assert_eq!(
            decode_server(server_code::CONNECT_TO_PEER, &w.into_inner()).unwrap(),
            ServerEvent::ConnectToPeer {
                username: "bob".into(),
                kind: Some(ConnKind::File),
                ip: Ipv4Addr::new(192, 168, 1, 2),
                port: 2234,
                token: 77
            }
        );
        assert_eq!(decode_server(9999, &[]).unwrap(), ServerEvent::Other(9999));
    }

    #[test]
    fn peer_init_round_trip() {
        for init in [
            PeerInit::PierceFirewall(42),
            PeerInit::PeerInit {
                username: "alice".into(),
                kind: Some(ConnKind::File),
            },
        ] {
            let frame = init.encode();
            assert_eq!(
                u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize,
                frame.len() - 4
            );
            assert_eq!(PeerInit::decode(&frame[4..]).unwrap(), init);
        }
    }

    #[tokio::test]
    async fn peer_messages_round_trip() {
        let reply = SearchReply {
            username: "digger".into(),
            token: 9,
            files: vec![flac("@@music\\Artist\\Album\\01 - Track.flac")],
            slot_free: true,
            avg_speed: 1_000_000,
            queue_length: 3,
        };
        assert_eq!(
            round_trip(PeerMessage::SearchReply(reply.clone())).await,
            PeerMessage::SearchReply(reply)
        );

        let dirs = vec![SharedDir {
            path: "@@music\\Artist\\Album".into(),
            files: vec![flac("01 - Track.flac"), flac("02 - Other.flac")],
        }];
        assert_eq!(
            round_trip(PeerMessage::SharedFileList(dirs.clone())).await,
            PeerMessage::SharedFileList(dirs.clone())
        );
        assert_eq!(
            round_trip(PeerMessage::FolderContents {
                token: 5,
                folder: "@@music\\Artist\\Album".into(),
                dirs: dirs.clone()
            })
            .await,
            PeerMessage::FolderContents {
                token: 5,
                folder: "@@music\\Artist\\Album".into(),
                dirs
            }
        );

        for message in [
            PeerMessage::TransferRequest {
                direction: 1,
                token: 3,
                filename: "a\\b.flac".into(),
                size: Some(123),
            },
            PeerMessage::TransferResponse {
                token: 3,
                allowed: false,
                reason: Some("Queued".into()),
            },
            PeerMessage::TransferResponse {
                token: 3,
                allowed: true,
                reason: None,
            },
            PeerMessage::QueueUpload {
                filename: "a\\b.flac".into(),
            },
            PeerMessage::PlaceInQueueResponse {
                filename: "a\\b.flac".into(),
                place: 12,
            },
            PeerMessage::UploadDenied {
                filename: "a\\b.flac".into(),
                reason: "File not shared.".into(),
            },
            PeerMessage::UserInfo(UserInfo {
                description: "hi".into(),
                picture: Some(vec![1, 2, 3]),
                total_uploads: 10,
                queue_size: 2,
                slots_free: true,
            }),
        ] {
            assert_eq!(round_trip(message.clone()).await, message);
        }
    }

    #[test]
    fn hostile_search_reply_is_rejected_not_allocated() {
        // Claims four billion files in a tiny message.
        let mut inner = Writer::new();
        inner.str("x").u32(1).u32(u32::MAX);
        let body = zlib_compress(&inner.into_inner());
        assert!(PeerMessage::decode(peer_code::FILE_SEARCH_RESPONSE, &body).is_err());
    }
}
