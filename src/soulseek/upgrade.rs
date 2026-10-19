//! Quality upgrade: find a lossless copy of a library track on Soulseek and
//! add it to the library as another format of the same track.
//!
//! ```text
//! search "artist title" ─► collect replies ─► match + rank candidates
//!   ─► queue the best ─► (fails, refused, never starts) ─► next candidate
//!   ─► received: verify lossless + length ─► adopt the library's tags
//!   ─► filed next to the track as its Lossless format
//! ```
//!
//! Matching is deliberately conservative: an upgrade that silently files a
//! different recording (a remix, a live take, another edit) under the
//! track is worse than no upgrade. A candidate must
//!
//! * be a lossless file type (FLAC, WAV, AIFF; `.m4a` could be ALAC or AAC,
//!   which a search reply cannot tell, so it is skipped),
//! * contain every word of the title and every word of one of the artists
//!   somewhere in its path (folders often carry the artist),
//! * not name a version the track does not ("Remix", "Live", "Extended
//!   Mix", …; "Original Mix" is the plain version and is allowed),
//! * and last as long as the library's copy, within a few seconds, when the
//!   peer reports a length.
//!
//! The received file is checked again (lossless codec, length) before it
//! reaches the library, since peers' claims are untrusted. A lossless
//! codec only proves the file is lossless *now*; see `import` on lossy
//! sources transcoded to FLAC.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::downloads::DownloadState;
use super::messages::SharedFile;
use super::search::Reply;
use super::session::Session;
use super::{MAX_QUERY_CHARS, SoulseekError, extension, split_path};
use crate::domain::metadata::TrackMetadata;
use crate::domain::provider::TrackKey;
use crate::domain::quality::OutputFormat;
use crate::library::{Library, LibraryEntry};

/// How far the received file's length may be from the library copy's. A
/// different edit or a version with a longer intro is further off than this.
pub(super) const MAX_LENGTH_DIFFERENCE_SECS: f64 = 3.0;
/// Peers report whole seconds.
const LISTED_LENGTH_SLACK_SECS: f64 = 1.0;
/// Smaller "lossless" files are previews, damaged or not music.
const MIN_FILE_BYTES: u64 = 1024 * 1024;
/// Replies are collected at least this long before choosing, so that a
/// slow peer with a better copy still has a chance.
const SEARCH_MIN_WAIT: Duration = Duration::from_secs(10);
/// Then the search is over, found or not.
const SEARCH_MAX_WAIT: Duration = Duration::from_secs(30);
/// Copies tried in all, over every search.
const MAX_ATTEMPTS: usize = 8;
/// When every copy found has been tried, Soulseek is searched once more
/// for users not tried yet: others may have come online since, and a queue
/// that never moves is no reason to give up on the track.
const MAX_SEARCHES: usize = 2;
/// Default for how long a copy may wait to start before the next one is
/// tried (`INKULT_SOULSEEK_UPGRADE_WAIT`). A user with a free slot starts
/// within seconds; one who is firewalled, not really sharing or has a long
/// queue may never start, and the upgrade would wait on them forever.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(60);
/// A running transfer's speed is measured over this long…
const SPEED_WINDOW: Duration = Duration::from_secs(60);
/// …and, if it would still need longer than this to finish, it is given up
/// for the next copy (only when there is one: a slow copy beats none).
/// Transfers that stop altogether fail on their own (see `downloads`).
const MAX_REMAINING: Duration = Duration::from_secs(15 * 60);
const POLL: Duration = Duration::from_secs(1);

/// Extensions of files that can only hold lossless audio.
const LOSSLESS_EXTENSIONS: &[&str] = &["flac", "wav", "aif", "aiff"];

/// Bracketed title parts that describe the upload, not the recording.
const NOISE_WORDS: &[&str] = &[
    "official",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "visualizer",
    "visualiser",
    "hd",
    "hq",
    "4k",
    "premiere",
    "explicit",
    "remastered",
    "remaster",
    "download",
    "teaser",
];

/// Words naming another version of a recording.
const VERSION_WORDS: &[&str] = &[
    "remix",
    "rmx",
    "mix",
    "edit",
    "dub",
    "vip",
    "instrumental",
    "acapella",
    "acappella",
    "live",
    "extended",
    "radio",
    "bootleg",
    "rework",
    "remake",
    "cover",
    "karaoke",
    "sped",
    "slowed",
    "nightcore",
    "demo",
    "flip",
    "reprise",
    "version",
];

const FEATURING: &[&str] = &["feat", "ft", "featuring"];

/// What an upgrade is for, persisted with its download so that a transfer
/// resumed after a restart is still filed under the right track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpgradeTarget {
    pub key: TrackKey,
    /// For display.
    pub title: String,
    /// Length of the library's copy, to check the received file against.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// How to tag the file when the library has no copy of the track (an
    /// upgrade asked for straight from a search result): the provider's
    /// metadata, and its cover saved in the staging directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<TrackMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_file: Option<PathBuf>,
}

/// The track to find a lossless copy of.
#[derive(Debug, Clone)]
pub struct UpgradeWanted {
    pub key: TrackKey,
    pub title: String,
    pub artists: Vec<String>,
    pub duration_secs: Option<f64>,
    /// Set when the library has no copy of the track: see [`UpgradeTarget`].
    pub metadata: Option<TrackMetadata>,
    pub artwork_file: Option<PathBuf>,
}

impl UpgradeWanted {
    /// What to look for to upgrade `key`'s copy in the library (its best
    /// one), or `None` if the library has none. The length is measured from
    /// the file.
    pub async fn from_library(library: &Library, key: &TrackKey) -> Option<Self> {
        let (best, path) = library.best_source(key).await?;
        let duration_secs = tokio::task::spawn_blocking(move || {
            crate::metadata::tagging::probe_duration(&path).map(|d| d.as_secs_f64())
        })
        .await
        .ok()
        .flatten();
        Some(Self {
            key: best.key,
            title: best.title,
            artists: best.artists,
            duration_secs,
            metadata: None,
            artwork_file: None,
        })
    }

    fn target(&self) -> UpgradeTarget {
        UpgradeTarget {
            key: self.key.clone(),
            title: self.title.clone(),
            duration_ms: self.duration_secs.map(|s| (s * 1000.0).round() as u64),
            metadata: self.metadata.clone(),
            artwork_file: self.artwork_file.clone(),
        }
    }
}

/// A file that looks like the wanted track in lossless quality.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub username: String,
    pub path: String,
    pub size: u64,
    pub slot_free: bool,
    pub queue_length: u32,
    pub avg_speed: u32,
    /// The peer stated a length, and it matches.
    pub length_confirmed: bool,
    /// What the peer says about it, e.g. "FLAC · 16-bit · 44.1 kHz".
    pub summary: String,
    bit_depth: u32,
    sample_rate_hz: u32,
}

impl Candidate {
    /// Higher is better: a confirmed length first (a wrong file costs a
    /// whole transfer), then the best quality (bit depth, sample rate),
    /// then the fastest peer, then a free upload slot, then FLAC over AIFF
    /// over WAV (same audio; tags travel best in FLAC), then the shorter
    /// queue. A fast peer whose upload does not start soon is given up on
    /// after the start timeout, so speed may outrank a free slot.
    fn rank(&self) -> (bool, u32, u32, u32, bool, u8, std::cmp::Reverse<u32>) {
        let container = match extension(&self.path).as_str() {
            "flac" => 2,
            "aif" | "aiff" => 1,
            _ => 0,
        };
        (
            self.length_confirmed,
            self.bit_depth,
            self.sample_rate_hz,
            self.avg_speed,
            self.slot_free,
            container,
            std::cmp::Reverse(self.queue_length),
        )
    }
}

/// Progress of an upgrade, for the job that runs it.
#[derive(Debug, Clone)]
pub enum UpgradeProgress {
    Searching {
        query: String,
        replies: usize,
        candidates: usize,
    },
    Trying {
        attempt: usize,
        of: usize,
        candidate: Candidate,
    },
    /// Waiting for the peer: contacting it, or queued in its uploads.
    Waiting {
        note: Option<String>,
        /// Another copy is tried if the upload has not started by then.
        switch_in: Duration,
    },
    Transferring {
        bytes: u64,
        size: u64,
        speed_bytes_per_sec: Option<f64>,
    },
    Importing,
}

#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    #[error("no lossless copy of this track was found on Soulseek ({0})")]
    NotFound(String),
    #[error("tried {tried} lossless {copies}, none worked; last: {last}", copies = if *tried == 1 { "copy" } else { "copies" })]
    AllFailed { tried: usize, last: String },
    #[error("the track has too few words to search for")]
    NothingToSearch,
    #[error("the Soulseek session ended; a transfer already started resumes at the next sign-in")]
    SessionEnded,
    #[error(transparent)]
    Soulseek(#[from] SoulseekError),
    #[error("cancelled")]
    Cancelled,
}

/// Lowercase words, with apostrophes dropped so "Don't" matches "Dont".
fn words(text: &str) -> Vec<String> {
    text.chars()
        .filter(|c| !matches!(c, '\'' | '’' | '`'))
        .collect::<String>()
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Drop "original mix" (the plain version, as labels name it).
fn without_original_mix(words: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        if words[i] == "original" && words.get(i + 1).is_some_and(|w| w == "mix") {
            i += 2;
            continue;
        }
        out.push(words[i].clone());
        i += 1;
    }
    out
}

/// A title split into its plain part and bracketed parts.
fn title_parts(title: &str) -> Vec<(bool, &str)> {
    let mut parts = Vec::new();
    let mut rest = title;
    while let Some(start) = rest.find(['(', '[', '{']) {
        let close = match &rest[start..start + 1] {
            "(" => ')',
            "[" => ']',
            _ => '}',
        };
        let Some(len) = rest[start + 1..].find(close) else {
            break;
        };
        parts.push((false, &rest[..start]));
        parts.push((true, &rest[start + 1..start + 1 + len]));
        rest = &rest[start + len + 2..];
    }
    parts.push((false, rest));
    parts
}

/// The words a copy of `title` must contain: upload descriptions ("Official
/// Video") and featured artists (written in many ways) are left out.
fn title_words(title: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (bracketed, part) in title_parts(title) {
        let part_words = words(part);
        if bracketed && part_words.iter().any(|w| NOISE_WORDS.contains(&w.as_str())) {
            continue;
        }
        for word in part_words {
            if FEATURING.contains(&word.as_str()) {
                break;
            }
            out.push(word);
        }
    }
    without_original_mix(&out)
        .into_iter()
        .filter(|w| w != "and")
        .collect()
}

/// The Soulseek search for `wanted`: the first artist and the title words.
/// Every word must appear in a result, so fewer, surer words find more.
pub(super) fn upgrade_query(wanted: &UpgradeWanted) -> Option<String> {
    let mut seen = HashSet::new();
    let artist = wanted.artists.first().map(|a| words(a)).unwrap_or_default();
    let query: Vec<String> = artist
        .into_iter()
        .chain(title_words(&wanted.title))
        .filter(|w| seen.insert(w.clone()))
        .collect();
    if query.is_empty() {
        return None;
    }
    let mut out = String::new();
    for word in query {
        if out.chars().count() + word.chars().count() + 1 > MAX_QUERY_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
    }
    Some(out)
}

fn is_lossless_name(path: &str) -> bool {
    LOSSLESS_EXTENSIONS.contains(&extension(path).as_str())
}

/// Whether `file` (from a search reply) is plausibly `wanted`, in lossless
/// quality. Returns whether its listed length confirmed it.
fn is_match(wanted: &UpgradeWanted, required: &[String], file: &SharedFile) -> Option<bool> {
    if !is_lossless_name(&file.name) || file.size < MIN_FILE_BYTES {
        return None;
    }
    let path_words: HashSet<String> = words(&file.name).into_iter().collect();
    if !required.iter().all(|w| path_words.contains(w)) {
        return None;
    }
    let artist_found = wanted.artists.iter().any(|artist| {
        let artist = words(artist);
        !artist.is_empty() && artist.iter().all(|w| path_words.contains(w))
    });
    if !wanted.artists.is_empty() && !artist_found {
        return None;
    }

    // Versions are named in the file name; folders ("Remixes", "Live at
    // …") describe their neighbours too.
    let (_, name) = split_path(&file.name);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let ours: HashSet<String> = words(&wanted.title).into_iter().collect();
    let other_version = without_original_mix(&words(stem))
        .iter()
        .any(|w| VERSION_WORDS.contains(&w.as_str()) && !ours.contains(w));
    if other_version {
        return None;
    }

    match (wanted.duration_secs, file.attrs.duration_secs) {
        (Some(want), Some(listed)) => {
            let difference = (f64::from(listed) - want).abs();
            (difference <= MAX_LENGTH_DIFFERENCE_SECS + LISTED_LENGTH_SLACK_SECS).then_some(true)
        }
        // Without lengths, a name match alone needs an artist to back it.
        (None, _) if wanted.artists.is_empty() => None,
        _ => Some(false),
    }
}

fn describe(file: &SharedFile) -> String {
    let mut parts = vec![match extension(&file.name).as_str() {
        "flac" => "FLAC".to_owned(),
        "wav" => "WAV".to_owned(),
        _ => "AIFF".to_owned(),
    }];
    if let Some(depth) = file.attrs.bit_depth {
        parts.push(format!("{depth}-bit"));
    }
    if let Some(rate) = file.attrs.sample_rate_hz {
        let khz = f64::from(rate) / 1000.0;
        parts.push(if rate % 1000 == 0 {
            format!("{khz:.0} kHz")
        } else {
            format!("{khz:.1} kHz")
        });
    }
    parts.join(" · ")
}

/// Matching files from `replies`, best first, at most one per user (a
/// second file from a user who failed would fail the same way).
pub(super) fn candidates(wanted: &UpgradeWanted, replies: &[Reply]) -> Vec<Candidate> {
    let required = title_words(&wanted.title);
    if required.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<Candidate> = Vec::new();
    for reply in replies {
        let best = reply
            .files
            .iter()
            .filter_map(|file| {
                let length_confirmed = is_match(wanted, &required, file)?;
                Some(Candidate {
                    username: reply.username.clone(),
                    path: file.name.clone(),
                    size: file.size,
                    slot_free: reply.slot_free,
                    queue_length: reply.queue_length,
                    avg_speed: reply.avg_speed,
                    length_confirmed,
                    summary: describe(file),
                    bit_depth: file.attrs.bit_depth.unwrap_or(0),
                    sample_rate_hz: file.attrs.sample_rate_hz.unwrap_or(0),
                })
            })
            .max_by_key(Candidate::rank);
        if let Some(candidate) = best {
            match found.iter_mut().find(|c| c.username == candidate.username) {
                Some(existing) if existing.rank() < candidate.rank() => *existing = candidate,
                Some(_) => {}
                None => found.push(candidate),
            }
        }
    }
    found.sort_by_key(|c| std::cmp::Reverse(c.rank()));
    found
}

/// The user's requests to stop waiting for the copy being tried and move
/// on to the next one. A count rather than a flag: a copy is skipped when
/// the count changes while it waits, so a stale request (made while
/// another copy was being tried) never skips a later one.
#[derive(Debug, Clone, Default)]
pub struct SkipRequests(Arc<AtomicU64>);

impl SkipRequests {
    pub fn request(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn count(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// How one candidate's download ended.
enum Attempt {
    Imported,
    Failed(String),
}

/// Decides when a candidate is taking too long and the next one should be
/// tried: it has not started within the start timeout, or it is so slow it
/// would need more than [`MAX_REMAINING`] to finish.
struct Watch {
    start_timeout: Duration,
    /// When the current wait for the upload to start began.
    waiting_since: Instant,
    /// Bytes received at the start of the current speed window, while
    /// transferring.
    window: Option<(Instant, u64)>,
}

impl Watch {
    fn new(start_timeout: Duration, now: Instant) -> Self {
        Self {
            start_timeout,
            waiting_since: now,
            window: None,
        }
    }

    /// The upload has not started (or went back to waiting, e.g. to be
    /// retried after a dropped connection): how long until it is given up,
    /// or why it is given up now.
    fn waiting(&mut self, now: Instant) -> Result<Duration, String> {
        if self.window.take().is_some() {
            self.waiting_since = now;
        }
        let waited = now.duration_since(self.waiting_since);
        self.start_timeout.checked_sub(waited).ok_or_else(|| {
            format!(
                "the upload did not start within {}",
                minutes(self.start_timeout)
            )
        })
    }

    /// Bytes are arriving. With `more` copies left to try, gives up on one
    /// that would take longer than [`MAX_REMAINING`] to finish.
    fn transferring(
        &mut self,
        now: Instant,
        bytes: u64,
        size: u64,
        more: bool,
    ) -> Result<(), String> {
        let &mut (since, from) = self.window.get_or_insert((now, bytes));
        let elapsed = now.duration_since(since);
        if elapsed < SPEED_WINDOW {
            return Ok(());
        }
        self.window = Some((now, bytes));
        let rate = bytes.saturating_sub(from) as f64 / elapsed.as_secs_f64();
        let left = size.saturating_sub(bytes) as f64;
        let too_slow = left > rate * MAX_REMAINING.as_secs_f64();
        if !(more && too_slow) {
            return Ok(());
        }
        Err(format!(
            "too slow ({:.1} KB/s), it would need more than {}",
            rate / 1000.0,
            minutes(MAX_REMAINING)
        ))
    }
}

fn minutes(d: Duration) -> String {
    match d.as_secs() {
        s if s % 60 != 0 => format!("{s} seconds"),
        60 => "a minute".into(),
        s => format!("{} minutes", s / 60),
    }
}

impl Session {
    /// Find a lossless copy of `wanted` and add it to the library as the
    /// track's [`OutputFormat::Lossless`] format, owned by this session's
    /// user. The search shows in the user's Soulseek searches and each
    /// transfer in their downloads, like any other. A copy still waiting
    /// to start is given up when `skip` is requested.
    pub async fn upgrade(
        self: &Arc<Self>,
        wanted: &UpgradeWanted,
        cancel: &CancellationToken,
        skip: &SkipRequests,
        report: &(dyn Fn(UpgradeProgress) + Send + Sync),
    ) -> Result<LibraryEntry, UpgradeError> {
        let query = upgrade_query(wanted).ok_or(UpgradeError::NothingToSearch)?;
        let found = self
            .collect_candidates(wanted, &query, cancel, report)
            .await?;
        if found.is_empty() {
            return Err(UpgradeError::NotFound(format!(
                "searched for “{query}”: nobody shared a FLAC, WAV or AIFF file matching its title, artist and length"
            )));
        }
        tracing::info!(owner = %self.owner, track = %wanted.key, candidates = found.len(), "quality upgrade candidates");

        let mut queue: VecDeque<Candidate> = found.into_iter().take(MAX_ATTEMPTS).collect();
        // Users asked already (or whose file is already downloading): a
        // later search does not offer them again.
        let mut asked: HashSet<String> = HashSet::new();
        let mut searches = 1;
        let mut shown = 0;
        let mut attempts = 0;
        let mut last = String::new();
        loop {
            let Some(candidate) = queue.pop_front() else {
                if searches >= MAX_SEARCHES || shown >= MAX_ATTEMPTS {
                    break;
                }
                searches += 1;
                let fresh: VecDeque<Candidate> = self
                    .collect_candidates(wanted, &query, cancel, report)
                    .await?
                    .into_iter()
                    .filter(|c| !asked.contains(&c.username))
                    .take(MAX_ATTEMPTS - shown)
                    .collect();
                tracing::info!(owner = %self.owner, track = %wanted.key, candidates = fresh.len(), "quality upgrade searched again");
                queue = fresh;
                continue;
            };
            asked.insert(candidate.username.clone());
            shown += 1;
            report(UpgradeProgress::Trying {
                attempt: shown,
                of: shown + queue.len(),
                candidate: candidate.clone(),
            });
            let target = wanted.target();
            let Some(id) = self
                .queue_upgrade(&candidate.username, &candidate.path, candidate.size, target)
                .await?
            else {
                continue; // already being downloaded on its own
            };
            attempts += 1;
            match self
                .follow(id, !queue.is_empty(), cancel, skip, report)
                .await?
            {
                Attempt::Imported => {
                    return self
                        .ctx
                        .library
                        .find(&wanted.key, OutputFormat::Lossless)
                        .await
                        .ok_or_else(|| UpgradeError::AllFailed {
                            tried: attempts,
                            last: "the imported file is no longer in the library".into(),
                        });
                }
                Attempt::Failed(reason) => {
                    tracing::info!(owner = %self.owner, track = %wanted.key, peer = %candidate.username, %reason, "quality upgrade candidate failed");
                    last = format!("{}: {reason}", candidate.username);
                }
            }
        }
        if attempts == 0 {
            return Err(UpgradeError::NotFound(
                "every matching file is already in your Soulseek downloads".into(),
            ));
        }
        Err(UpgradeError::AllFailed {
            tried: attempts,
            last,
        })
    }

    /// Search and gather candidates, for [`SEARCH_MIN_WAIT`] at least and
    /// until a sure one (length confirmed, free slot) has turned up or
    /// [`SEARCH_MAX_WAIT`] has passed.
    async fn collect_candidates(
        self: &Arc<Self>,
        wanted: &UpgradeWanted,
        query: &str,
        cancel: &CancellationToken,
        report: &(dyn Fn(UpgradeProgress) + Send + Sync),
    ) -> Result<Vec<Candidate>, UpgradeError> {
        report(UpgradeProgress::Searching {
            query: query.to_owned(),
            replies: 0,
            candidates: 0,
        });
        let search = self.search_when_online(query, cancel).await?;
        let started = tokio::time::Instant::now();
        let found = loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL) => {}
                _ = cancel.cancelled() => {
                    self.stop_search(search);
                    return Err(UpgradeError::Cancelled);
                }
            }
            let replies = self
                .search_results(search, 0)
                .map(|view| view.results)
                .unwrap_or_default();
            let found = candidates(wanted, &replies);
            report(UpgradeProgress::Searching {
                query: query.to_owned(),
                replies: replies.len(),
                candidates: found.len(),
            });
            let elapsed = started.elapsed();
            let sure = found
                .first()
                .is_some_and(|c| c.length_confirmed && c.slot_free);
            if elapsed >= SEARCH_MAX_WAIT || (elapsed >= SEARCH_MIN_WAIT && sure) {
                break found;
            }
        };
        // The results stay listed in the Soulseek tab, for picking by hand.
        self.stop_search(search);
        Ok(found)
    }

    /// Start the search, waiting a little if the session is reconnecting.
    async fn search_when_online(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> Result<u64, UpgradeError> {
        let mut waited = Duration::ZERO;
        loop {
            match self.search(query, None) {
                Ok(summary) => return Ok(summary.id),
                Err(SoulseekError::Offline) if waited < SEARCH_MAX_WAIT => {}
                Err(e) => return Err(e.into()),
            }
            if self.is_stopped() {
                return Err(UpgradeError::SessionEnded);
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => waited += Duration::from_secs(2),
                _ = cancel.cancelled() => return Err(UpgradeError::Cancelled),
            }
        }
    }

    /// Follow download `id` to its end, reporting progress. It is cancelled
    /// for the next copy when it takes too long (see [`Watch`]); a slow
    /// transfer only when `more` copies are lined up. While it waits to
    /// start, the user may `skip` it.
    async fn follow(
        &self,
        id: u64,
        more: bool,
        cancel: &CancellationToken,
        skip: &SkipRequests,
        report: &(dyn Fn(UpgradeProgress) + Send + Sync),
    ) -> Result<Attempt, UpgradeError> {
        let mut watch = Watch::new(self.ctx.config.upgrade_start_timeout, Instant::now());
        let mut skips_seen = skip.count();
        let give_up = |reason: String| {
            self.abandon_download(id, format!("Stopped by the quality upgrade: {reason}"));
            Ok(Attempt::Failed(reason))
        };
        loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL) => {}
                _ = cancel.cancelled() => {
                    self.cancel_download(id);
                    return Err(UpgradeError::Cancelled);
                }
            }
            if self.is_stopped() {
                return Err(UpgradeError::SessionEnded);
            }
            let Some(view) = self.download_view(id) else {
                return Ok(Attempt::Failed("the download was removed".into()));
            };
            match view.state {
                DownloadState::Completed | DownloadState::Skipped => return Ok(Attempt::Imported),
                DownloadState::Failed => {
                    return Ok(Attempt::Failed(
                        view.error.unwrap_or_else(|| "the download failed".into()),
                    ));
                }
                DownloadState::Cancelled => {
                    return Ok(Attempt::Failed(
                        "cancelled in the Soulseek downloads".into(),
                    ));
                }
                DownloadState::Transferring => {
                    // Skipping is offered while waiting only.
                    skips_seen = skip.count();
                    if let Err(reason) =
                        watch.transferring(Instant::now(), view.bytes, view.size, more)
                    {
                        return give_up(reason);
                    }
                    report(UpgradeProgress::Transferring {
                        bytes: view.bytes,
                        size: view.size,
                        speed_bytes_per_sec: view.speed_bytes_per_sec,
                    });
                }
                DownloadState::Importing => report(UpgradeProgress::Importing),
                DownloadState::Queued
                | DownloadState::Requesting
                | DownloadState::Remote
                | DownloadState::Offered => {
                    if skip.count() != skips_seen {
                        return give_up("skipped by the user".into());
                    }
                    let switch_in = match watch.waiting(Instant::now()) {
                        Ok(left) => left,
                        Err(reason) => return give_up(reason),
                    };
                    let note = match view.place {
                        Some(place) => Some(format!("Place {place} in the user's upload queue")),
                        None => view.note,
                    };
                    report(UpgradeProgress::Waiting { note, switch_in });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::ProviderKind;
    use crate::soulseek::messages::FileAttrs;

    fn wanted(title: &str, artists: &[&str], secs: Option<f64>) -> UpgradeWanted {
        UpgradeWanted {
            key: TrackKey {
                provider: ProviderKind::YouTube,
                id: "abc".into(),
            },
            title: title.into(),
            artists: artists.iter().map(|a| (*a).to_owned()).collect(),
            duration_secs: secs,
            metadata: None,
            artwork_file: None,
        }
    }

    fn file(name: &str, secs: Option<u32>, depth: Option<u32>, rate: Option<u32>) -> SharedFile {
        SharedFile {
            name: name.into(),
            size: 30 * 1024 * 1024,
            attrs: FileAttrs {
                duration_secs: secs,
                bit_depth: depth,
                sample_rate_hz: rate,
                ..FileAttrs::default()
            },
        }
    }

    fn reply(user: &str, slot_free: bool, files: Vec<SharedFile>) -> Reply {
        Reply {
            username: user.into(),
            slot_free,
            avg_speed: 1_000_000,
            queue_length: 0,
            files,
            received_at_ms: 0,
        }
    }

    #[test]
    fn queries_leave_out_upload_noise_and_featured_artists() {
        let w = wanted(
            "Don't Stop (feat. Someone) [Official Music Video]",
            &["Artist & Band"],
            None,
        );
        assert_eq!(upgrade_query(&w).as_deref(), Some("artist band dont stop"));
        let w = wanted("Glue ft. Guest", &["Bicep"], None);
        assert_eq!(upgrade_query(&w).as_deref(), Some("bicep glue"));
        // A remix is part of what is wanted.
        let w = wanted("Atlas (Ben Böhmer Remix)", &["Bicep"], None);
        assert_eq!(
            upgrade_query(&w).as_deref(),
            Some("bicep atlas ben böhmer remix")
        );
        assert_eq!(upgrade_query(&wanted("(Official Video)", &[], None)), None);
    }

    #[test]
    fn matches_only_lossless_copies_of_the_same_recording() {
        let w = wanted("Glue", &["Bicep"], Some(269.4));
        let replies = vec![
            reply(
                "mp3",
                true,
                vec![file(
                    "@@a\\Bicep\\Bicep\\03 Glue.mp3",
                    Some(269),
                    None,
                    None,
                )],
            ),
            reply(
                "remix",
                true,
                vec![file(
                    "@@b\\Bicep - Glue (Hammer Remix).flac",
                    Some(269),
                    Some(16),
                    Some(44_100),
                )],
            ),
            reply(
                "other",
                true,
                vec![file(
                    "@@c\\Someone Else\\Glue.flac",
                    Some(269),
                    Some(16),
                    Some(44_100),
                )],
            ),
            reply(
                "long",
                true,
                vec![file(
                    "@@d\\Bicep\\Glue (Extended).flac",
                    Some(400),
                    Some(16),
                    Some(44_100),
                )],
            ),
            reply(
                "length",
                true,
                vec![file(
                    "@@d\\Bicep\\Glue.flac",
                    Some(300),
                    Some(16),
                    Some(44_100),
                )],
            ),
            reply(
                "ok",
                true,
                vec![file(
                    "@@e\\Music\\Bicep\\Bicep (2017)\\03 - Glue (Original Mix).flac",
                    Some(270),
                    Some(16),
                    Some(44_100),
                )],
            ),
            reply(
                "tiny",
                true,
                vec![SharedFile {
                    size: 1000,
                    ..file("@@f\\Bicep - Glue.flac", Some(269), None, None)
                }],
            ),
        ];
        let found = candidates(&w, &replies);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].username, "ok");
        assert!(found[0].length_confirmed);
        assert_eq!(found[0].summary, "FLAC · 16-bit · 44.1 kHz");
    }

    #[test]
    fn a_remix_finds_the_remix() {
        let w = wanted("Atlas (Ben Böhmer Remix)", &["Bicep"], None);
        let replies = vec![
            reply(
                "plain",
                true,
                vec![file("x\\Bicep - Atlas.flac", None, None, None)],
            ),
            reply(
                "remix",
                true,
                vec![file(
                    "x\\Bicep - Atlas (Ben Böhmer Remix).wav",
                    None,
                    None,
                    None,
                )],
            ),
        ];
        let found = candidates(&w, &replies);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].username, "remix");
        assert!(!found[0].length_confirmed);
    }

    #[test]
    fn ranks_sure_then_best_quality_copies_first() {
        let w = wanted("Glue", &["Bicep"], Some(269.0));
        let replies = vec![
            reply(
                "unknown-length",
                true,
                vec![file("a\\Bicep - Glue.flac", None, Some(24), Some(96_000))],
            ),
            reply(
                "busy-hires",
                false,
                vec![file(
                    "b\\Bicep - Glue.flac",
                    Some(269),
                    Some(24),
                    Some(96_000),
                )],
            ),
            reply(
                "free-wav",
                true,
                vec![file(
                    "c\\Bicep - Glue.wav",
                    Some(269),
                    Some(16),
                    Some(44_100),
                )],
            ),
            reply(
                "free-flac",
                true,
                vec![file(
                    "d\\Bicep - Glue.flac",
                    Some(268),
                    Some(16),
                    Some(44_100),
                )],
            ),
            reply(
                "free-hires",
                true,
                vec![
                    file("e\\Bicep - Glue.flac", Some(269), Some(16), Some(44_100)),
                    file(
                        "e\\hires\\Bicep - Glue.flac",
                        Some(269),
                        Some(24),
                        Some(48_000),
                    ),
                ],
            ),
        ];
        let order: Vec<String> = candidates(&w, &replies)
            .into_iter()
            .map(|c| c.username)
            .collect();
        assert_eq!(
            order,
            [
                "busy-hires",
                "free-hires",
                "free-flac",
                "free-wav",
                "unknown-length"
            ]
        );
        let best = &candidates(&w, &replies)[1];
        assert!(best.path.contains("hires"), "one file per user, the best");
    }

    #[test]
    fn among_equal_quality_the_fastest_peer_comes_first() {
        let w = wanted("Glue", &["Bicep"], Some(269.0));
        let copy = |user: &str, slot_free, avg_speed, depth| Reply {
            avg_speed,
            ..reply(
                user,
                slot_free,
                vec![file(
                    &format!("{user}\\Bicep - Glue.flac"),
                    Some(269),
                    Some(depth),
                    Some(44_100),
                )],
            )
        };
        let replies = vec![
            copy("slow", true, 50_000, 16),
            copy("fast-busy", false, 4_000_000, 16),
            copy("fast", true, 2_000_000, 16),
            copy("slow-hires", true, 10_000, 24),
        ];
        let order: Vec<String> = candidates(&w, &replies)
            .into_iter()
            .map(|c| c.username)
            .collect();
        assert_eq!(order, ["slow-hires", "fast-busy", "fast", "slow"]);
    }

    #[test]
    fn without_lengths_or_artists_nothing_is_sure_enough() {
        let w = wanted("Intro", &[], None);
        let replies = vec![reply(
            "u",
            true,
            vec![file("x\\Intro.flac", Some(60), None, None)],
        )];
        assert!(candidates(&w, &replies).is_empty());
        let w = wanted("Intro", &[], Some(60.0));
        assert_eq!(candidates(&w, &replies).len(), 1);
    }

    #[test]
    fn a_copy_that_never_starts_is_given_up() {
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let mut watch = Watch::new(Duration::from_secs(180), t0);
        assert_eq!(watch.waiting(at(0)), Ok(Duration::from_secs(180)));
        assert_eq!(watch.waiting(at(179)), Ok(Duration::from_secs(1)));
        assert_eq!(
            watch.waiting(at(181)).unwrap_err(),
            "the upload did not start within 3 minutes"
        );
    }

    #[test]
    fn going_back_to_waiting_restarts_the_clock() {
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let mut watch = Watch::new(Duration::from_secs(180), t0);
        assert!(watch.waiting(at(170)).is_ok());
        assert!(watch.transferring(at(171), 0, 1000, true).is_ok());
        // The connection dropped and the download is queued again.
        assert_eq!(watch.waiting(at(200)), Ok(Duration::from_secs(180)));
        assert!(watch.waiting(at(381)).is_err());
    }

    #[test]
    fn a_crawling_transfer_is_given_up_only_for_another_copy() {
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let size = 40_000_000;
        // 10 kB/s: 40 MB would take more than an hour.
        for more in [true, false] {
            let mut watch = Watch::new(Duration::from_secs(180), t0);
            assert!(watch.transferring(at(0), 0, size, more).is_ok());
            assert!(watch.transferring(at(30), 300_000, size, more).is_ok());
            let verdict = watch.transferring(at(60), 600_000, size, more);
            assert_eq!(verdict.is_err(), more, "{verdict:?}");
            if more {
                assert_eq!(
                    verdict.unwrap_err(),
                    "too slow (10.0 KB/s), it would need more than 15 minutes"
                );
            }
        }
        // 100 kB/s: under 7 minutes, kept.
        let mut watch = Watch::new(Duration::from_secs(180), t0);
        assert!(watch.transferring(at(0), 0, size, true).is_ok());
        assert!(watch.transferring(at(60), 6_000_000, size, true).is_ok());
        // Slow but nearly done: kept.
        assert!(watch.transferring(at(120), 39_500_000, size, true).is_ok());
        assert!(watch.transferring(at(180), 39_900_000, size, true).is_ok());
    }

    #[test]
    fn waits_are_spelled_out() {
        assert_eq!(minutes(Duration::from_secs(60)), "a minute");
        assert_eq!(minutes(Duration::from_secs(180)), "3 minutes");
        assert_eq!(minutes(Duration::from_secs(90)), "90 seconds");
    }
}
