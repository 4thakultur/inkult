//! End-to-end tests against a fake Soulseek server and a fake peer, both on
//! loopback: sign-in, search, queueing, a resumed transfer, library import,
//! and sharing (browse and upload).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::downloads::{DownloadState, file_key};
use super::messages::{
    FileAttrs, MAX_INIT_FRAME, PeerInit, PeerMessage, SearchReply, SharedFile, peer_frame_limit,
    server_code,
};
use super::uploads::UploadState;
use super::wire::{Reader, Writer, read_coded_frame, read_frame};
use super::{Soulseek, SoulseekConfig};
use crate::auth::UserId;
use crate::domain::length::{MaxTrackLength, TrackLengthLimit};
use crate::domain::quality::{Codec, OutputFormat};
use crate::library::Library;

const ME: UserId = UserId(1);
const SHARE_PATH: &str = "@@abcde\\Music\\Bicep\\Bicep (2017) [WAV]\\03 - Glue.wav";

/// A valid 16-bit stereo 44.1 kHz WAV with 0.1 s of silence.
fn silent_wav() -> Vec<u8> {
    let data_len: u32 = 4410 * 4;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&44_100u32.to_le_bytes());
    wav.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(wav.len() + data_len as usize, 0);
    wav
}

/// Accepts one client, signs it in, answers address lookups with the fake
/// peer's port and reports every other message.
async fn fake_server(
    listener: TcpListener,
    peer_port: u16,
    seen: mpsc::UnboundedSender<(u32, Vec<u8>)>,
) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let (code, _) = read_coded_frame(&mut stream, |_| 1 << 20).await.unwrap();
    assert_eq!(code, server_code::LOGIN);
    let mut ok = Writer::new();
    ok.bool(true)
        .str("welcome")
        .u32(0x7F00_0001)
        .str("hash")
        .bool(false);
    stream
        .write_all(&Writer::frame_u32(server_code::LOGIN, &ok.into_inner()))
        .await
        .unwrap();
    while let Ok((code, body)) = read_coded_frame(&mut stream, |_| 1 << 20).await {
        if code == server_code::GET_PEER_ADDRESS {
            let username = Reader::new(&body).string().unwrap();
            let mut answer = Writer::new();
            answer
                .str(&username)
                .u32(0x7F00_0001)
                .u32(u32::from(peer_port))
                .u32(0)
                .raw(&[0, 0]);
            stream
                .write_all(&Writer::frame_u32(code, &answer.into_inner()))
                .await
                .unwrap();
        }
        let _ = seen.send((code, body));
    }
}

async fn next_server_message(
    seen: &mut mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
    wanted: u32,
) -> Vec<u8> {
    loop {
        let (code, body) = tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("server message in time")
            .expect("server running");
        if code == wanted {
            return body;
        }
    }
}

async fn read_peer_message(stream: &mut TcpStream) -> PeerMessage {
    let (code, body) = tokio::time::timeout(
        Duration::from_secs(10),
        read_coded_frame(stream, peer_frame_limit),
    )
    .await
    .expect("peer message in time")
    .expect("peer message");
    PeerMessage::decode(code, &body).unwrap()
}

async fn read_init(stream: &mut TcpStream) -> PeerInit {
    let frame = read_frame(stream, MAX_INIT_FRAME).await.unwrap();
    PeerInit::decode(&frame).unwrap()
}

async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(value) = check() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Harness {
    soulseek: Arc<Soulseek>,
    library: Arc<Library>,
    peer_listener: TcpListener,
    our_port: u16,
    seen: mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
    staging: std::path::PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn harness() -> Harness {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let library = Arc::new(
        Library::open(root.path().into(), data.path().join("library.json"))
            .await
            .unwrap(),
    );
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server.local_addr().unwrap();
    let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_port = peer_listener.local_addr().unwrap().port();
    let (seen_tx, seen) = mpsc::unbounded_channel();
    tokio::spawn(fake_server(server, peer_port, seen_tx));

    let our_port = free_port();
    let staging = data.path().join("staging");
    let soulseek = Arc::new(
        Soulseek::new(
            SoulseekConfig {
                server: server_addr.to_string(),
                ports: vec![our_port],
                upload_slots: 2,
                max_track_length: TrackLengthLimit::new(MaxTrackLength::DEFAULT),
                staging_dir: staging.clone(),
                store_path: data.path().join("soulseek.json"),
                allow_loopback: true,
            },
            Arc::clone(&library),
        )
        .await
        .unwrap(),
    );
    Harness {
        soulseek,
        library,
        peer_listener,
        our_port,
        seen,
        staging,
        _dirs: (root, data),
    }
}

#[tokio::test]
async fn search_queue_resume_and_import() {
    let mut h = harness().await;
    let status = h
        .soulseek
        .sign_in(ME, "digger", "secret", false)
        .await
        .unwrap();
    assert_eq!(status.listen_port, Some(h.our_port));
    let port = next_server_message(&mut h.seen, server_code::SET_WAIT_PORT).await;
    assert_eq!(Reader::new(&port).u32().unwrap(), u32::from(h.our_port));
    let session = h.soulseek.require(ME).unwrap();

    // Search: the server gets the query; a peer answers on its own connection.
    let search = session.search("bicep glue", None).unwrap();
    let body = next_server_message(&mut h.seen, server_code::FILE_SEARCH).await;
    let mut r = Reader::new(&body);
    let token = r.u32().unwrap();
    assert_eq!(r.string().unwrap(), "bicep glue");

    let wav = silent_wav();
    let size = wav.len() as u64;
    let mut inbound = TcpStream::connect(("127.0.0.1", h.our_port)).await.unwrap();
    let init = PeerInit::PeerInit {
        username: "alice".into(),
        kind: Some(super::messages::ConnKind::Peer),
    };
    inbound.write_all(&init.encode()).await.unwrap();
    let reply = PeerMessage::SearchReply(SearchReply {
        username: "someone-else".into(), // ignored: the connection says alice
        token,
        files: vec![SharedFile {
            name: SHARE_PATH.into(),
            size,
            attrs: FileAttrs {
                duration_secs: Some(1),
                sample_rate_hz: Some(44_100),
                bit_depth: Some(16),
                ..FileAttrs::default()
            },
        }],
        slot_free: false,
        avg_speed: 100_000,
        queue_length: 4,
    });
    inbound.write_all(&reply.encode()).await.unwrap();
    let results = eventually("search results", || {
        session
            .search_results(search.id, 0)
            .filter(|v| !v.results.is_empty())
    })
    .await;
    assert_eq!(results.results[0].username, "alice");
    assert_eq!(results.results[0].queue_length, 4);
    // The peer hangs up; downloading must open a new connection.
    drop(inbound);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Half the file is already here from an earlier, interrupted attempt.
    let partial = h
        .staging
        .join("soulseek")
        .join(ME.to_string())
        .join(format!("{}.part", file_key("alice", SHARE_PATH).id));
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, &wav[..1000]).unwrap();

    let queued = session
        .download_files(
            "alice",
            vec![super::FileRequest {
                path: SHARE_PATH.into(),
                size,
            }],
        )
        .await
        .unwrap();
    assert_eq!(queued, 1);

    // We connect to alice and ask to be queued.
    let (mut conn, _) = tokio::time::timeout(Duration::from_secs(15), h.peer_listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        read_init(&mut conn).await,
        PeerInit::PeerInit {
            username: "digger".into(),
            kind: Some(super::messages::ConnKind::Peer)
        }
    );
    assert_eq!(
        read_peer_message(&mut conn).await,
        PeerMessage::QueueUpload {
            filename: SHARE_PATH.into()
        }
    );
    assert_eq!(
        read_peer_message(&mut conn).await,
        PeerMessage::PlaceInQueueRequest {
            filename: SHARE_PATH.into()
        }
    );
    conn.write_all(
        &PeerMessage::PlaceInQueueResponse {
            filename: SHARE_PATH.into(),
            place: 3,
        }
        .encode(),
    )
    .await
    .unwrap();
    eventually("queue place", || {
        session
            .downloads()
            .first()
            .filter(|d| d.place == Some(3) && d.state == DownloadState::Remote)
            .cloned()
    })
    .await;

    // Our turn: alice offers the upload; we accept.
    conn.write_all(
        &PeerMessage::TransferRequest {
            direction: 1,
            token: 555,
            filename: SHARE_PATH.into(),
            size: Some(size),
        }
        .encode(),
    )
    .await
    .unwrap();
    assert_eq!(
        read_peer_message(&mut conn).await,
        PeerMessage::TransferResponse {
            token: 555,
            allowed: true,
            reason: None
        }
    );

    // Alice opens the file connection; we resume at the partial length.
    let mut file_conn = TcpStream::connect(("127.0.0.1", h.our_port)).await.unwrap();
    let init = PeerInit::PeerInit {
        username: "alice".into(),
        kind: Some(super::messages::ConnKind::File),
    };
    file_conn.write_all(&init.encode()).await.unwrap();
    file_conn.write_all(&555u32.to_le_bytes()).await.unwrap();
    let offset = file_conn.read_u64_le().await.unwrap();
    assert_eq!(offset, 1000);
    file_conn.write_all(&wav[offset as usize..]).await.unwrap();
    // The downloader closes the connection when it has everything.
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), file_conn.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();

    let done = eventually("import", || {
        session
            .downloads()
            .into_iter()
            .find(|d| d.state.is_terminal())
    })
    .await;
    assert_eq!(done.state, DownloadState::Completed, "{:?}", done.error);
    let quality = done.quality.unwrap();
    assert_eq!(quality.codec, Codec::Pcm);
    assert!(quality.lossless);
    assert_eq!(
        done.destination.as_deref(),
        Some("Bicep/Bicep/03 - Glue.wav")
    );
    assert!(done.inferred.contains(&"title".to_owned()));
    assert!(!partial.exists());

    let key = file_key("alice", SHARE_PATH);
    let entry = h.library.find(&key, OutputFormat::Original).await.unwrap();
    assert_eq!(entry.owners, BTreeSet::from([ME]));
    assert_eq!(entry.title, "Glue");
    assert_eq!(entry.artists, ["Bicep"]);
    assert!(
        entry
            .source_url
            .starts_with("slsk://alice/@@abcde/Music/Bicep")
    );

    // Asking again for a file already in the library downloads nothing.
    session
        .download_files(
            "alice",
            vec![super::FileRequest {
                path: SHARE_PATH.into(),
                size,
            }],
        )
        .await
        .unwrap();
    let again = session.downloads();
    assert_eq!(again[0].state, DownloadState::Skipped);

    h.soulseek.shutdown().await;
}

#[tokio::test]
async fn shares_library_for_browsing_and_uploads() {
    let mut h = harness().await;
    // Put a track into the user's library.
    let staged = h._dirs.1.path().join("track.wav");
    std::fs::write(&staged, silent_wav()).unwrap();
    let meta = crate::domain::metadata::TrackMetadata {
        provider: crate::domain::provider::ProviderKind::SoundCloud,
        provider_track_id: "1".into(),
        source_url: "https://soundcloud.com/a/b".into(),
        title: "Glue".into(),
        artists: vec!["Bicep".into()],
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
    };
    let quality = crate::metadata::tagging::probe_quality(&staged, Codec::Pcm).unwrap();
    h.library
        .import(
            staged,
            &meta,
            quality,
            OutputFormat::Original,
            "wav",
            BTreeSet::from([ME]),
        )
        .await
        .unwrap();

    h.soulseek
        .sign_in(ME, "digger", "secret", true)
        .await
        .unwrap();
    let session = h.soulseek.require(ME).unwrap();
    assert!(!session.status().await.share_library);
    session.set_sharing(true).await;
    let status = session.status().await;
    assert_eq!((status.shared_folders, status.shared_files), (1, 1));
    assert!(status.remembered);
    // The server learns what we share.
    loop {
        let body = next_server_message(&mut h.seen, server_code::SHARED_FOLDERS_FILES).await;
        let mut r = Reader::new(&body);
        if (r.u32().unwrap(), r.u32().unwrap()) == (1, 1) {
            break;
        }
    }

    // Bob browses us and sees only the virtual path.
    let mut bob = TcpStream::connect(("127.0.0.1", h.our_port)).await.unwrap();
    let init = PeerInit::PeerInit {
        username: "bob".into(),
        kind: Some(super::messages::ConnKind::Peer),
    };
    bob.write_all(&init.encode()).await.unwrap();
    bob.write_all(&PeerMessage::GetShareFileList.encode())
        .await
        .unwrap();
    let PeerMessage::SharedFileList(dirs) = read_peer_message(&mut bob).await else {
        panic!("expected a share list");
    };
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].path, "inkult\\Bicep\\Singles");
    assert_eq!(dirs[0].files[0].name, "Glue.wav");
    let shared_path = "inkult\\Bicep\\Singles\\Glue.wav".to_owned();

    // Something not shared is refused.
    bob.write_all(
        &PeerMessage::QueueUpload {
            filename: "inkult\\..\\..\\etc\\passwd".into(),
        }
        .encode(),
    )
    .await
    .unwrap();
    assert!(matches!(
        read_peer_message(&mut bob).await,
        PeerMessage::UploadDenied { reason, .. } if reason == "File not shared."
    ));

    // Bob queues the track; we offer it when a slot is free.
    bob.write_all(
        &PeerMessage::QueueUpload {
            filename: shared_path.clone(),
        }
        .encode(),
    )
    .await
    .unwrap();
    let PeerMessage::TransferRequest {
        direction: 1,
        token,
        filename,
        size: Some(size),
    } = read_peer_message(&mut bob).await
    else {
        panic!("expected a transfer request");
    };
    assert_eq!(filename, shared_path);
    bob.write_all(
        &PeerMessage::TransferResponse {
            token,
            allowed: true,
            reason: None,
        }
        .encode(),
    )
    .await
    .unwrap();

    // We open a file connection to bob (address from the server).
    let (mut file_conn, _) =
        tokio::time::timeout(Duration::from_secs(15), h.peer_listener.accept())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        read_init(&mut file_conn).await,
        PeerInit::PeerInit {
            username: "digger".into(),
            kind: Some(super::messages::ConnKind::File)
        }
    );
    assert_eq!(file_conn.read_u32_le().await.unwrap(), token);
    file_conn.write_all(&100u64.to_le_bytes()).await.unwrap();
    let mut received = vec![0u8; (size - 100) as usize];
    file_conn.read_exact(&mut received).await.unwrap();
    assert_eq!(received, silent_wav()[100..]);
    drop(file_conn);

    let upload = eventually("upload completion", || {
        session
            .uploads()
            .into_iter()
            .find(|u| u.state == UploadState::Completed)
    })
    .await;
    assert_eq!(upload.username, "bob");
    assert_eq!(upload.bytes, size);

    // Turning sharing off takes the files offline.
    session.set_sharing(false).await;
    assert_eq!(session.status().await.shared_files, 0);
    h.soulseek.shutdown().await;
}

#[tokio::test]
async fn rejected_sign_in_is_reported_and_nothing_keeps_running() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = server.accept().await.unwrap();
        let _ = read_coded_frame(&mut stream, |_| 1 << 20).await;
        let mut no = Writer::new();
        no.bool(false).str("INVALIDPASS");
        let _ = stream
            .write_all(&Writer::frame_u32(server_code::LOGIN, &no.into_inner()))
            .await;
    });
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(
        Library::open(dir.path().join("music"), dir.path().join("library.json"))
            .await
            .unwrap(),
    );
    let port = free_port();
    let soulseek = Soulseek::new(
        SoulseekConfig {
            server: addr.to_string(),
            ports: vec![port],
            upload_slots: 1,
            max_track_length: TrackLengthLimit::new(MaxTrackLength::DEFAULT),
            staging_dir: dir.path().join("staging"),
            store_path: dir.path().join("soulseek.json"),
            allow_loopback: true,
        },
        library,
    )
    .await
    .unwrap();
    let error = soulseek
        .sign_in(ME, "digger", "wrong", true)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Soulseek rejected the sign-in: wrong password for this Soulseek username"
    );
    assert!(soulseek.session(ME).is_none());
    // The listening port went back to the pool.
    assert!(std::net::TcpListener::bind(("0.0.0.0", port)).is_ok());
    // The password of a failed sign-in is not stored.
    assert!(!dir.path().join("soulseek.json").exists());
}

#[tokio::test]
async fn remembered_account_reconnects_only_when_its_user_logs_in() {
    let mut h = harness().await;
    // Remembered from an earlier run: opening the service connects nothing.
    h.soulseek
        .ctx
        .store
        .update(ME, |a| {
            a.username = "digger".into();
            a.password = Some("secret".into());
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.soulseek.session(ME).is_none());
    assert!(h.seen.try_recv().is_err());

    // Another user logging in does not sign this account in.
    let other = UserId(2);
    h.soulseek.resume_remembered(other);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.soulseek.session(ME).is_none());
    assert!(h.soulseek.session(other).is_none());

    // Its own user logging in does, once; a second login keeps the session.
    h.soulseek.resume_remembered(ME);
    let session = eventually("the remembered sign-in", || h.soulseek.session(ME)).await;
    assert_eq!(session.username(), "digger");
    h.soulseek.resume_remembered(ME);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(Arc::ptr_eq(&session, &h.soulseek.session(ME).unwrap()));
    h.soulseek.shutdown().await;
}
