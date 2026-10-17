//! Keeps server filesystem paths out of what pirates see.
//!
//! Handlers already avoid sending paths, but error texts are built from
//! many sources (tool errors name the program, I/O errors their context,
//! yt-dlp's stderr whatever it likes). This last filter replaces every known
//! server path in a JSON response to a non-captain, so a new error message
//! cannot leak one. Captains run the server and see everything.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::auth::User;
use crate::config::Config;

/// Largest JSON body that is filtered. Real responses (a big library
/// listing) are a few MiB; anything larger is refused rather than sent
/// unfiltered.
const MAX_FILTERED_BODY: usize = 64 * 1024 * 1024;

pub struct PathRedactor {
    /// (text to find, replacement), longest first so a directory inside
    /// another (staging inside data) gets its own name.
    needles: Vec<(String, &'static str)>,
}

impl PathRedactor {
    pub fn new(config: &Config) -> Self {
        let cwd = std::env::current_dir().ok();
        let mut paths: Vec<(PathBuf, &'static str)> = Vec::new();
        for (dir, name) in [
            (&config.library_dir, "[music]"),
            (&config.staging_dir, "[staging]"),
            (&config.data_dir, "[data]"),
        ] {
            paths.extend(absolute_forms(dir, cwd.as_deref()).map(|p| (p, name)));
        }
        // A bare program name ("yt-dlp") is found on PATH and says nothing;
        // a configured path does.
        for (program, name) in [
            (&config.ytdlp_program, "yt-dlp"),
            (&config.ffmpeg_program, "ffmpeg"),
        ] {
            let program = Path::new(program);
            if program.components().count() > 1 {
                paths.extend(absolute_forms(program, cwd.as_deref()).map(|p| (p, name)));
                paths.push((program.to_path_buf(), name));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            paths.push((PathBuf::from(home), "~"));
        }
        if let Some(cwd) = cwd {
            paths.push((cwd, "[server]"));
        }
        Self::from_paths(paths)
    }

    fn from_paths(paths: Vec<(PathBuf, &'static str)>) -> Self {
        let mut needles: Vec<(String, &'static str)> = paths
            .into_iter()
            .filter_map(|(path, name)| {
                let text = path.to_str()?.trim_end_matches(['/', '\\']).to_owned();
                // "/" or "" would redact every slash.
                (text.len() > 1).then_some((text, name))
            })
            .flat_map(|(text, name)| {
                // Inside JSON the path appears with `"` and `\` escaped.
                let escaped = serde_json::to_string(&text)
                    .ok()
                    .map(|s| s[1..s.len() - 1].to_owned());
                [Some((text, name)), escaped.map(|e| (e, name))]
            })
            .flatten()
            .collect();
        needles.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        needles.dedup();
        Self { needles }
    }

    /// `text` with every server path replaced by its placeholder.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for (needle, name) in &self.needles {
            if let Some(replaced) = replace_path(&out, needle, name) {
                out = Cow::Owned(replaced);
            }
        }
        out
    }
}

/// The path as given if absolute, joined to the working directory, and
/// with symlinks resolved: error texts may use any of them.
fn absolute_forms(path: &Path, cwd: Option<&Path>) -> impl Iterator<Item = PathBuf> {
    let joined = if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        cwd.map(|cwd| cwd.join(path))
    };
    let canonical = std::fs::canonicalize(path).ok();
    joined.into_iter().chain(canonical)
}

/// Replace `needle` where it is a whole path prefix (not followed by more
/// of a file name, so `/data` leaves `/database` alone). `None` if absent.
fn replace_path(text: &str, needle: &str, replacement: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    let mut found = false;
    while let Some(at) = rest.find(needle) {
        let end = at + needle.len();
        let boundary = rest[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'));
        out.push_str(&rest[..at]);
        if boundary {
            out.push_str(replacement);
            found = true;
        } else {
            out.push_str(needle);
        }
        rest = &rest[end..];
    }
    found.then(|| {
        out.push_str(rest);
        out
    })
}

/// Middleware: filter JSON responses to anyone but a captain. Runs inside
/// `require_user`, which provides the user.
pub async fn hide_server_paths(
    State(redactor): State<Arc<PathRedactor>>,
    request: Request,
    next: Next,
) -> Response {
    let captain = request
        .extensions()
        .get::<User>()
        .is_some_and(|u| u.role.is_captain());
    let response = next.run(request).await;
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if captain || !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_FILTERED_BODY).await else {
        tracing::error!("response too large to filter for server paths; not sent");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error; the details are in the server log" })),
        )
            .into_response();
    };
    // Bodies we produce are serde_json output, so always UTF-8.
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    match redactor.redact(text) {
        Cow::Borrowed(_) => Response::from_parts(parts, Body::from(bytes)),
        Cow::Owned(filtered) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(filtered))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> PathRedactor {
        PathRedactor::from_paths(vec![
            ("/srv/inkult/data".into(), "[data]"),
            ("/srv/inkult/data/staging/".into(), "[staging]"),
            ("/srv/music".into(), "[music]"),
            ("/opt/bin/yt-dlp".into(), "yt-dlp"),
            (r#"C:\Music "DJ""#.into(), "[music]"),
            ("/".into(), "[root]"),
        ])
    }

    #[test]
    fn replaces_server_paths() {
        let r = redactor();
        assert_eq!(
            r.redact("yt-dlp is not installed (/opt/bin/yt-dlp): No such file"),
            "yt-dlp is not installed (yt-dlp): No such file"
        );
        assert_eq!(
            r.redact("moving /srv/inkult/data/staging/j1/a.m4a to /srv/music/A/B.m4a"),
            "moving [staging]/j1/a.m4a to [music]/A/B.m4a"
        );
        assert_eq!(r.redact("reading /srv/inkult/data."), "reading [data].");
    }

    #[test]
    fn leaves_other_text_alone() {
        let r = redactor();
        assert!(matches!(
            r.redact("/srv/musicbox/x and / slash"),
            Cow::Borrowed(_)
        ));
        assert_eq!(
            r.redact("/srv/music_old and /srv/music"),
            "/srv/music_old and [music]"
        );
    }

    #[test]
    fn finds_json_escaped_paths() {
        let r = redactor();
        let body = json!({ "error": r#"cannot read C:\Music "DJ"\x.flac"# }).to_string();
        let out = r.redact(&body);
        assert!(!out.contains("Music"), "{out}");
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
    }
}
