//! Black-box security tests: only captains may use admin actions, and
//! nothing about the server (paths, tool details) reaches pirates.
//!
//! Each test starts the real `inkult` binary on a free port with its own
//! temporary data and library directories, and talks HTTP to it. No
//! external tools or network are needed: yt-dlp and ffmpeg are pointed at
//! a path that does not exist, which also gives the "tool missing" errors a
//! known server path to look for in responses.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sha1::Sha1;

const CAPTAIN_PASSWORD: &str = "anne brass kettle under sleet at dawn";

struct Server {
    child: Child,
    base: String,
    setup_code: String,
    http: reqwest::Client,
    /// Holds the data/library directories; its path must never reach pirates.
    dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: StatusCode,
    body: Value,
    text: String,
    /// Session token from `Set-Cookie`, if a new session was issued.
    session: Option<String>,
}

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let missing_tools = dir.path().join("server-only-tools");
        let mut child = Command::new(env!("CARGO_BIN_EXE_inkult"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("INKULT_BIND", format!("127.0.0.1:{port}"))
            .env("INKULT_DATA_DIR", dir.path().join("data"))
            .env("INKULT_LIBRARY_DIR", dir.path().join("music"))
            .env("INKULT_YTDLP", missing_tools.join("yt-dlp"))
            .env("INKULT_FFMPEG", missing_tools.join("ffmpeg"))
            .env("RUST_LOG", "inkult=info")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start inkult");

        // The first-captain setup code is only printed in the log.
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((_, rest)) = line.split_once("setup code ") {
                    let code: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
                        .collect();
                    let _ = tx.send(code);
                }
            }
        });
        let setup_code = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("setup code in the log");

        let server = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            setup_code,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("http client"),
            dir,
        };
        for _ in 0..600 {
            if server
                .http
                .get(server.url("/api/auth/state"))
                .send()
                .await
                .is_ok()
            {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("inkult did not start listening");
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn secret_path(&self) -> String {
        self.dir.path().display().to_string()
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        let mut request = self.http.request(method, self.url(path));
        if let Some(token) = session {
            request = request.header("Cookie", format!("inkult_session={token}"));
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        Self::reply(request.send().await.expect("request")).await
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        session: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        let mut request = self
            .http
            .request(method, self.url(path))
            .header("Cookie", format!("inkult_session={session}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_owned());
        }
        Self::reply(request.send().await.expect("request")).await
    }

    async fn reply(response: reqwest::Response) -> Reply {
        let status = response.status();
        let session = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.strip_prefix("inkult_session="))
            .map(|v| v.split(';').next().unwrap_or("").to_owned())
            .find(|v| !v.is_empty());
        let text = response.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Reply {
            status,
            body,
            text,
            session,
        }
    }

    /// Turn on 2FA from an enrollment session; returns the full session.
    async fn enroll(&self, session: &str, password: &str) -> String {
        let setup = self
            .call(
                Method::POST,
                "/api/auth/2fa/setup",
                Some(session),
                Some(json!({})),
            )
            .await;
        assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
        let secret = setup.body["secret"].as_str().expect("secret").to_owned();
        let enabled = self
            .call(
                Method::POST,
                "/api/auth/2fa/enable",
                Some(session),
                Some(json!({ "password": password, "code": totp(&secret) })),
            )
            .await;
        assert_eq!(enabled.status, StatusCode::OK, "{}", enabled.text);
        enabled.session.expect("session after enabling 2FA")
    }

    /// Appoint the first captain and enroll 2FA (mandatory by default).
    async fn first_captain(&self) -> (u64, String) {
        let setup = self
            .call(
                Method::POST,
                "/api/auth/setup",
                None,
                Some(json!({
                    "code": self.setup_code,
                    "username": "anne",
                    "password": CAPTAIN_PASSWORD,
                })),
            )
            .await;
        assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
        let id = setup.body["id"].as_u64().expect("id");
        let session = setup.session.expect("captain session");
        (id, self.enroll(&session, CAPTAIN_PASSWORD).await)
    }

    async fn create_user(&self, captain: &str, username: &str, role: &str) -> (u64, String) {
        let created = self
            .call(
                Method::POST,
                "/api/users",
                Some(captain),
                Some(json!({ "username": username, "role": role })),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        (
            created.body["user"]["id"].as_u64().expect("id"),
            created.body["temporary_password"]
                .as_str()
                .expect("temporary password")
                .to_owned(),
        )
    }

    /// Log in with a temporary password (session that must change it).
    async fn login_temporary(&self, username: &str, temporary: &str) -> String {
        let login = self
            .call(
                Method::POST,
                "/api/auth/login",
                None,
                Some(json!({ "username": username, "password": temporary })),
            )
            .await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.text);
        assert_eq!(login.body["password_change_required"], true);
        login.session.expect("session")
    }

    /// Replace the temporary password; returns the 2FA-enrollment session.
    async fn choose_password(&self, session: &str, temporary: &str, new: &str) -> String {
        let changed = self
            .call(
                Method::POST,
                "/api/auth/password",
                Some(session),
                Some(json!({ "current": temporary, "new": new })),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
        changed.session.expect("session")
    }

    /// Full onboarding of a captain-created account; returns
    /// (full session, password).
    async fn onboard(&self, username: &str, temporary: &str) -> (String, String) {
        let password = format!("{username} gallant otter juggles nine lanterns");
        let session = self.login_temporary(username, temporary).await;
        let session = self.choose_password(&session, temporary, &password).await;
        (self.enroll(&session, &password).await, password)
    }
}

/// RFC 6238 code for now (SHA-1, 6 digits, 30 s), as an authenticator app
/// would show it.
fn totp(secret: &str) -> String {
    let key = base32_decode(secret);
    let step = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        / 30;
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).expect("hmac key");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let value = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    format!("{:06}", value % 1_000_000)
}

fn base32_decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut bits, mut buffer, mut out) = (0u32, 0u64, Vec::new());
    for c in text.bytes().filter(|c| *c != b'=') {
        let value = ALPHABET
            .iter()
            .position(|a| *a == c.to_ascii_uppercase())
            .expect("base32 secret");
        buffer = (buffer << 5) | value as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}

/// Ids the admin requests aim at.
struct Targets {
    captain: u64,
    pirate: u64,
    other_pirate: u64,
}

/// Every admin-only operation.
fn admin_operations(t: &Targets) -> Vec<(Method, String, Option<Value>)> {
    let user = |id: u64| format!("/api/users/{id}");
    vec![
        (Method::GET, "/api/users".into(), None),
        (
            Method::POST,
            "/api/users".into(),
            Some(json!({ "username": "evil1", "role": "captain" })),
        ),
        (
            Method::POST,
            "/api/users".into(),
            Some(json!({ "username": "evil2", "role": "pirate" })),
        ),
        (
            Method::PATCH,
            user(t.pirate),
            Some(json!({ "role": "captain" })),
        ),
        (
            Method::PATCH,
            user(t.captain),
            Some(json!({ "role": "pirate" })),
        ),
        (
            Method::PATCH,
            user(t.captain),
            Some(json!({ "reset_password": true })),
        ),
        (
            Method::PATCH,
            user(t.captain),
            Some(json!({ "reset_2fa": true })),
        ),
        (
            Method::PATCH,
            user(t.captain),
            Some(json!({ "unlock": true })),
        ),
        (
            Method::PATCH,
            user(t.other_pirate),
            Some(json!({ "reset_password": true })),
        ),
        (Method::DELETE, user(t.captain), None),
        (Method::DELETE, user(t.other_pirate), None),
        (Method::GET, "/api/admin/settings".into(), None),
        (
            Method::PATCH,
            "/api/admin/settings".into(),
            Some(json!({ "two_factor": "off" })),
        ),
        (
            Method::PATCH,
            "/api/admin/settings".into(),
            Some(json!({ "max_track_secs": 0 })),
        ),
        (
            Method::PATCH,
            "/api/admin/settings".into(),
            Some(json!({ "totp_issuer": "pwned" })),
        ),
        (Method::GET, "/api/library?scope=all".into(), None),
        (Method::GET, "/api/downloads?scope=all".into(), None),
        (
            Method::DELETE,
            "/api/downloads/finished?scope=all".into(),
            None,
        ),
        (
            Method::POST,
            "/api/downloads/1/cancel?scope=all".into(),
            None,
        ),
        (
            Method::POST,
            "/api/library/remove".into(),
            Some(json!({
                "provider": "youtube", "id": "x", "output": "original", "scope": "all"
            })),
        ),
    ]
}

/// Collects failed checks so one run reports all of them.
#[derive(Default)]
struct Findings(Vec<String>);

impl Findings {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if !ok {
            self.0.push(what());
        }
    }

    async fn all_denied(
        &mut self,
        server: &Server,
        who: &str,
        session: Option<&str>,
        targets: &Targets,
        expected: StatusCode,
    ) {
        for (method, path, body) in admin_operations(targets) {
            let reply = server
                .call(method.clone(), &path, session, body.clone())
                .await;
            self.check(reply.status == expected, || {
                format!(
                    "{who}: {method} {path} {} -> {} (expected {expected}): {}",
                    body.map(|b| b.to_string()).unwrap_or_default(),
                    reply.status,
                    reply.text
                )
            });
        }
    }

    /// Stop here if anything failed: once a non-captain got through, later
    /// steps (which rely on the captain's account) would only obscure it.
    fn assert_none_so_far(&self) {
        assert!(
            self.0.is_empty(),
            "{} security check(s) failed:\n  {}",
            self.0.len(),
            self.0.join("\n  ")
        );
    }

    fn assert_none(self) {
        assert!(
            self.0.is_empty(),
            "{} security check(s) failed:\n  {}",
            self.0.len(),
            self.0.join("\n  ")
        );
    }
}

/// Usernames and roles of the crew, as a captain sees them.
async fn crew(server: &Server, captain: &str) -> Vec<(String, String)> {
    let reply = server
        .call(Method::GET, "/api/users", Some(captain), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let mut crew: Vec<(String, String)> = reply
        .body
        .as_array()
        .expect("crew list")
        .iter()
        .map(|m| {
            let user = m.get("user").unwrap_or(m);
            (
                user["username"].as_str().unwrap_or_default().to_owned(),
                user["role"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    crew.sort();
    crew
}

fn members(list: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = list
        .iter()
        .map(|(n, r)| ((*n).to_owned(), (*r).to_owned()))
        .collect();
    list.sort();
    list
}

#[tokio::test]
async fn only_captains_reach_admin_actions() {
    let server = Server::start().await;
    let (captain_id, captain) = server.first_captain().await;
    let (jack_id, jack_temporary) = server.create_user(&captain, "jack", "pirate").await;
    let (mary_id, mary_temporary) = server.create_user(&captain, "mary", "pirate").await;
    let (bonny_id, bonny_temporary) = server.create_user(&captain, "bonny", "captain").await;
    let targets = Targets {
        captain: captain_id,
        pirate: jack_id,
        other_pirate: mary_id,
    };
    let settings = server
        .call(Method::GET, "/api/admin/settings", Some(&captain), None)
        .await
        .body;
    let mut crew_now = vec![
        ("anne", "captain"),
        ("jack", "pirate"),
        ("mary", "pirate"),
        ("bonny", "captain"),
    ];
    // A captain's room, for the moderation checks.
    let room = server
        .call(
            Method::POST,
            "/api/rooms",
            Some(&captain),
            Some(json!({ "name": "bridge" })),
        )
        .await;
    assert!(room.status.is_success(), "{}", room.text);
    let room_id = room.body["id"]
        .as_u64()
        .or_else(|| room.body["room"]["id"].as_u64())
        .expect("room id");
    let room_path = format!("/api/rooms/{room_id}");
    let mut f = Findings::default();

    // No session, or one that was never issued.
    f.all_denied(
        &server,
        "anonymous",
        None,
        &targets,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    f.all_denied(
        &server,
        "forged session",
        Some(&"00".repeat(32)),
        &targets,
        StatusCode::UNAUTHORIZED,
    )
    .await;

    // A pirate at every step of onboarding.
    let jack_password = "jack gallant otter juggles nine lanterns";
    let jack = server.login_temporary("jack", &jack_temporary).await;
    f.all_denied(
        &server,
        "pirate, temporary password",
        Some(&jack),
        &targets,
        StatusCode::FORBIDDEN,
    )
    .await;
    let jack = server
        .choose_password(&jack, &jack_temporary, jack_password)
        .await;
    f.all_denied(
        &server,
        "pirate, 2FA not set up",
        Some(&jack),
        &targets,
        StatusCode::FORBIDDEN,
    )
    .await;
    let jack = server.enroll(&jack, jack_password).await;
    f.all_denied(
        &server,
        "pirate",
        Some(&jack),
        &targets,
        StatusCode::FORBIDDEN,
    )
    .await;

    f.assert_none_so_far();

    // Other spellings of the admin routes.
    let pirate_self = format!("/api/users/{jack_id}");
    for (method, path, body) in [
        (Method::GET, "/api/users/".to_owned(), None),
        (Method::GET, "/api//users".into(), None),
        (Method::GET, "/API/users".into(), None),
        (Method::GET, "/api/%75sers".into(), None),
        (Method::GET, "/api/./users".into(), None),
        (Method::GET, "/api/rooms/../users".into(), None),
        (Method::GET, "/api/admin%2fsettings".into(), None),
        (Method::GET, "/api/admin/settings/".into(), None),
        (Method::HEAD, "/api/users".into(), None),
        (
            Method::PATCH,
            format!("{pirate_self}/"),
            Some(r#"{"role":"captain"}"#),
        ),
        (
            Method::PATCH,
            format!("/api/users/0{jack_id}"),
            Some(r#"{"role":"captain"}"#),
        ),
        (
            Method::PATCH,
            pirate_self.clone(),
            Some(r#"{"role":"Captain"}"#),
        ),
        (
            Method::PATCH,
            pirate_self.clone(),
            Some(r#"{"role":"admin"}"#),
        ),
        (
            Method::PATCH,
            pirate_self.clone(),
            Some(r#"{"role":"pirate","role":"captain"}"#),
        ),
        (
            Method::PATCH,
            pirate_self.clone(),
            Some(r#"{"Role":"captain"}"#),
        ),
    ] {
        let reply = server.raw(method.clone(), &path, &jack, &[], body).await;
        f.check(
            !reply.status.is_success() && !reply.status.is_server_error(),
            || {
                format!(
                    "pirate: {method} {path} {body:?} -> {}: {}",
                    reply.status, reply.text
                )
            },
        );
    }

    // Headers that some stacks trust for routing or identity.
    for header in [
        ("X-HTTP-Method-Override", "PATCH"),
        ("X-Original-URL", "/api/users"),
        ("X-Rewrite-URL", "/api/users"),
        ("X-Forwarded-User", "anne"),
        ("X-User-Role", "captain"),
        ("X-Forwarded-For", "127.0.0.1"),
    ] {
        let reply = server
            .raw(Method::GET, "/api/admin/settings", &jack, &[header], None)
            .await;
        f.check(reply.status == StatusCode::FORBIDDEN, || {
            format!("pirate with {header:?}: {} {}", reply.status, reply.text)
        });
    }

    f.assert_none_so_far();

    // A role smuggled into requests a pirate may make.
    let new_password = format!("{jack_password} again");
    let changed = server
        .call(
            Method::POST,
            "/api/auth/password",
            Some(&jack),
            Some(json!({ "current": jack_password, "new": new_password, "role": "captain" })),
        )
        .await;
    let jack = changed.session.unwrap_or(jack);
    let me = server
        .call(Method::GET, "/api/auth/me", Some(&jack), None)
        .await;
    f.check(me.body["role"] == "pirate", || {
        format!("role smuggled into a password change: {}", me.text)
    });

    // The first-captain setup cannot be replayed.
    for session in [None, Some(jack.as_str())] {
        let replay = server
            .call(
                Method::POST,
                "/api/auth/setup",
                session,
                Some(json!({
                    "code": server.setup_code,
                    "username": "jack2",
                    "password": "a long passphrase nobody would ever guess right",
                })),
            )
            .await;
        f.check(
            !replay.status.is_success() && replay.session.is_none(),
            || format!("setup replay: {} {}", replay.status, replay.text),
        );
    }

    // Moderating a room is for its owner and captains.
    server
        .call(
            Method::POST,
            &format!("{room_path}/join"),
            Some(&jack),
            Some(json!({})),
        )
        .await;
    let topic = server
        .call(
            Method::PATCH,
            &room_path,
            Some(&jack),
            Some(json!({ "topic": "pwned" })),
        )
        .await;
    f.check(topic.status == StatusCode::FORBIDDEN, || {
        format!(
            "pirate set a captain's room topic: {} {}",
            topic.status, topic.text
        )
    });
    let deleted = server
        .call(Method::DELETE, &room_path, Some(&jack), None)
        .await;
    f.check(deleted.status == StatusCode::FORBIDDEN, || {
        format!(
            "pirate deleted a captain's room: {} {}",
            deleted.status, deleted.text
        )
    });

    // A forged cross-site request riding on a captain's cookie.
    let forged = server
        .raw(
            Method::POST,
            "/api/users",
            &captain,
            &[("Origin", "https://evil.example")],
            Some(r#"{"username":"csrf","role":"captain"}"#),
        )
        .await;
    f.check(forged.status == StatusCode::FORBIDDEN, || {
        format!(
            "cross-origin admin write: {} {}",
            forged.status, forged.text
        )
    });
    let forged = server
        .raw(
            Method::PATCH,
            "/api/admin/settings",
            &captain,
            &[("Sec-Fetch-Site", "cross-site")],
            Some(r#"{"two_factor":"captains"}"#),
        )
        .await;
    f.check(forged.status == StatusCode::FORBIDDEN, || {
        format!("cross-site admin write: {} {}", forged.status, forged.text)
    });

    // A demoted captain's open session loses admin at once.
    let (bonny, _) = server.onboard("bonny", &bonny_temporary).await;
    let before = server
        .call(Method::GET, "/api/users", Some(&bonny), None)
        .await;
    assert_eq!(before.status, StatusCode::OK, "control: captain bonny");
    let demoted = server
        .call(
            Method::PATCH,
            &format!("/api/users/{bonny_id}"),
            Some(&captain),
            Some(json!({ "role": "pirate" })),
        )
        .await;
    assert!(demoted.status.is_success(), "{}", demoted.text);
    crew_now[3] = ("bonny", "pirate");
    f.all_denied(
        &server,
        "demoted captain",
        Some(&bonny),
        &targets,
        StatusCode::FORBIDDEN,
    )
    .await;

    // Nothing the attempts above tried actually changed.
    let crew_after = crew(&server, &captain).await;
    f.check(crew_after == members(&crew_now), || {
        format!("crew changed: {crew_after:?}")
    });
    let settings_now = server
        .call(Method::GET, "/api/admin/settings", Some(&captain), None)
        .await
        .body;
    f.check(settings_now == settings, || {
        format!("settings changed: {settings} -> {settings_now}")
    });

    // A deleted account's session is gone.
    let (mary, _) = server.onboard("mary", &mary_temporary).await;
    let removed = server
        .call(
            Method::DELETE,
            &format!("/api/users/{mary_id}"),
            Some(&captain),
            None,
        )
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text);
    f.all_denied(
        &server,
        "deleted account",
        Some(&mary),
        &targets,
        StatusCode::UNAUTHORIZED,
    )
    .await;

    // Control: the real captain still has admin.
    let control = server
        .call(Method::GET, "/api/users", Some(&captain), None)
        .await;
    f.check(control.status == StatusCode::OK, || {
        format!("captain lost admin: {} {}", control.status, control.text)
    });
    f.assert_none();
}

#[tokio::test]
async fn pirates_never_see_server_paths() {
    let server = Server::start().await;
    let secret = server.secret_path();
    let (_, captain) = server.first_captain().await;
    let (_, temporary) = server.create_user(&captain, "jack", "pirate").await;
    let (jack, _) = server.onboard("jack", &temporary).await;
    let mut f = Findings::default();

    // Status: what works, not where or which version.
    let status = server
        .call(Method::GET, "/api/status", Some(&jack), None)
        .await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.text);
    f.check(status.body.get("library_dir").is_none(), || {
        format!("pirate status has library_dir: {}", status.text)
    });
    for tool in status.body["tools"].as_array().expect("tools") {
        f.check(tool["available"] == false, || {
            format!("tool shown available: {tool}")
        });
        for field in ["program", "version", "error"] {
            f.check(tool.get(field).is_none(), || {
                format!("pirate status shows tool {field}: {tool}")
            });
        }
    }

    // Everything a pirate can ask for, including errors that name tool
    // programs and directories.
    let reads = [
        (Method::GET, "/api/status", None),
        (Method::GET, "/api/search?q=anything", None),
        (Method::GET, "/api/library", None),
        (Method::GET, "/api/downloads", None),
        (Method::GET, "/api/auth/me", None),
        (
            Method::GET,
            "/api/artist?url=https%3A%2F%2Fsoundcloud.com%2Fsomeone",
            None,
        ),
        (
            Method::POST,
            "/api/resolve",
            Some(json!({ "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ" })),
        ),
        (
            Method::POST,
            "/api/downloads",
            Some(json!({ "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ" })),
        ),
        (
            Method::POST,
            "/api/stream",
            Some(json!({ "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ" })),
        ),
    ];
    for (method, path, body) in reads {
        let reply = server.call(method.clone(), path, Some(&jack), body).await;
        f.check(!reply.text.contains(&secret), || {
            format!("pirate {method} {path} leaks {secret}: {}", reply.text)
        });
    }

    // Control: the captain does get the details (so the checks above
    // would catch a leak).
    let status = server
        .call(Method::GET, "/api/status", Some(&captain), None)
        .await;
    f.check(
        status.body["library_dir"]
            .as_str()
            .is_some_and(|d| d.starts_with(&secret)),
        || format!("captain status lacks library_dir: {}", status.text),
    );
    let search = server
        .call(Method::GET, "/api/search?q=anything", Some(&captain), None)
        .await;
    f.check(search.text.contains(&secret), || {
        format!(
            "control: captain's tool error names no program: {}",
            search.text
        )
    });

    f.assert_none();
}
