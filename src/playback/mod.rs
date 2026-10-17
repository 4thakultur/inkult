//! Listening to tracks in the browser, before or after they are downloaded.
//!
//! Playing is a two-step protocol. The UI first *prepares* a stream, which
//! may take seconds (yt-dlp must resolve the provider's CDN URL) and reports
//! failures as JSON. The `<audio>` element then plays `/api/stream/{id}`.
//! Prepared streams live in a small in-memory table, so the many range
//! requests a seeking browser makes never re-run yt-dlp.
//!
//! Listening never re-encodes lossy audio and never writes to disk:
//!
//! * progressive provider files are proxied byte-for-byte, with ranges;
//! * HLS provider streams are re-wrapped by ffmpeg (codec copied);
//! * library files are served as-is, with ranges, except containers that
//!   browsers cannot decode (AIFF, ALAC), which are decoded to FLAC on the
//!   fly — lossless, so what you hear is what is in the file.
//!
//! Library streams are bound to the user who prepared them: stream ids are
//! sequential, and another user must not be able to reach a file outside
//! their own library by guessing one. Provider streams are public material
//! and may be shared.
//!
//! Streams produced by ffmpeg have no byte ranges. The player seeks in them
//! by requesting the stream again from a time offset (`start`).
//!
//! HLS provider streams in AAC (all of SoundCloud) are downloaded once into
//! a [`Capture`] and played from it, so seeking does not touch the network
//! (see [`capture`]).
//!
//! Each prepared stream also has a [`Waveform`], measured by decoding the
//! same audio the listener hears, and cached per track rather than per
//! stream so playing a track again does not measure it again. Measuring
//! runs in the background and the UI polls it, drawing bars as they are
//! measured rather than waiting for the whole track.

pub mod capture;
mod hls;
pub mod range;
pub mod waveform;

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use url::Url;

use crate::auth::UserId;
use crate::domain::provider::ProviderKind;
use crate::domain::quality::{Codec, QualityReport};
use crate::metadata::tagging::probe_duration;
use crate::providers::{MusicProvider, ProviderError, StreamSource, StreamTransport};
use crate::tools::ffmpeg::{AudioPipe, Ffmpeg, PipeFormat, StreamInput};
use crate::tools::{AbortOnDrop, ToolError};
use capture::{Capture, CaptureError, CaptureReader, MAX_CAPTURE_BYTES};
use range::ByteRange;
use waveform::{Envelope, Waveform};

/// Prepared streams kept at once; the least recently used is dropped.
const MAX_PREPARED: usize = 64;
/// Preparing the same provider track again within this window reuses the
/// resolved CDN URL instead of running yt-dlp again.
const REUSE_WINDOW: Duration = Duration::from_secs(20 * 60);
/// CDN URLs older than this are resolved again before use. YouTube's last
/// about six hours; others are shorter, and expiry is also handled when a
/// request fails.
const MAX_SOURCE_AGE: Duration = Duration::from_secs(60 * 60);
/// Longest offset accepted for time-based seeking.
const MAX_START_SECS: f64 = 24.0 * 3600.0;
/// Waveforms kept at once (a couple of KB each); the least recently used
/// is dropped.
const MAX_WAVEFORMS: usize = 256;
/// A waveform nobody has asked about for this long is no longer measured:
/// the listener moved on, and it would hold an analysis slot.
const WAVEFORM_ABANDONED: Duration = Duration::from_secs(20);
/// Captured tracks kept in memory at once (a few MB each at most
/// [`MAX_CAPTURE_BYTES`]); the least recently used is dropped.
const MAX_CAPTURES: usize = 4;
/// Longest wait for a capture to reach a requested position, and for the
/// CDN to send more audio while capturing.
const CAPTURE_WAIT: Duration = Duration::from_secs(30);

pub type StreamId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamOrigin {
    /// Streamed from the provider; nothing is downloaded.
    Provider,
    /// Played from a file in the library.
    Library,
}

/// What the UI needs to know to play a prepared stream.
#[derive(Debug, Clone, Serialize)]
pub struct PreparedStream {
    pub id: StreamId,
    pub origin: StreamOrigin,
    pub provider: ProviderKind,
    /// Quality of the audio being played, e.g. "Opus · 130 kbps".
    pub summary: String,
    pub duration_secs: Option<f64>,
    /// Whether the browser can seek with byte ranges. Otherwise it must
    /// request the stream again with a `start` offset.
    pub seekable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("this stream is no longer available; press play again")]
    UnknownStream,
    #[error("the file is no longer in the library")]
    FileMissing,
    #[error("requested range is outside the file")]
    RangeNotSatisfiable { total: u64 },
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("{provider}: stream failed: {reason}")]
    Upstream {
        provider: ProviderKind,
        reason: String,
    },
    #[error("this track has no audio to draw")]
    NoAudio,
    #[error("cannot draw this track: {0}")]
    Waveform(String),
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Body of a stream response.
pub enum StreamBody {
    File(tokio::io::Take<tokio::fs::File>),
    Pipe(AudioPipe),
    Capture(CaptureReader),
    Upstream(reqwest::Response),
}

pub struct StreamResponse {
    pub content_type: &'static str,
    pub content_length: Option<u64>,
    /// `Content-Range` value when answering a range request (status 206).
    pub content_range: Option<String>,
    /// Whether to advertise `Accept-Ranges: bytes`.
    pub ranges: bool,
    pub body: StreamBody,
}

#[derive(Clone)]
enum Source {
    Remote {
        provider: Arc<dyn MusicProvider>,
        page_url: Url,
        stream: Arc<StreamSource>,
        resolved_at: Instant,
    },
    Local {
        path: PathBuf,
        delivery: LocalDelivery,
        /// The only user allowed to open this stream.
        viewer: UserId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalDelivery {
    /// The browser decodes the file itself.
    AsIs { mime: &'static str },
    /// Decoded to FLAC on the fly.
    Flac,
}

struct Entry {
    info: PreparedStream,
    source: Source,
    last_used: Instant,
}

/// Why a remote request failed: an expired CDN URL is worth one refresh.
enum RemoteFailure {
    Expired(String),
    Other(PlaybackError),
}

/// Identifies the audio a waveform was measured from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WaveformKey {
    Remote(Url),
    /// A library file as it was when measured; replacing it changes the key.
    Local {
        path: PathBuf,
        len: u64,
        modified: Option<std::time::SystemTime>,
    },
}

/// A waveform being measured in the background, or its outcome.
struct WaveformJob {
    progress: Mutex<WaveformProgress>,
}

struct WaveformProgress {
    envelope: Envelope,
    /// Length the track should have, so partial bars sit where they play.
    expected_secs: Option<f64>,
    outcome: Option<Result<Arc<Waveform>, String>>,
    last_polled: Instant,
}

impl WaveformJob {
    fn new(expected_secs: Option<f64>) -> Self {
        Self {
            progress: Mutex::new(WaveformProgress {
                envelope: Envelope::new(),
                expected_secs,
                outcome: None,
                last_polled: Instant::now(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WaveformProgress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The finished waveform, or what is measured so far.
    fn poll(&self) -> Result<Arc<Waveform>, PlaybackError> {
        let mut progress = self.lock();
        progress.last_polled = Instant::now();
        match &progress.outcome {
            Some(Ok(waveform)) => Ok(Arc::clone(waveform)),
            Some(Err(reason)) => Err(PlaybackError::Waveform(reason.clone())),
            None => Ok(Arc::new(
                progress
                    .envelope
                    .snapshot(
                        waveform::SAMPLE_RATE,
                        waveform::BARS,
                        progress.expected_secs,
                    )
                    .unwrap_or(Waveform {
                        duration_secs: 0.0,
                        measured_secs: 0.0,
                        bars: Vec::new(),
                        complete: false,
                    }),
            )),
        }
    }

    fn push(&self, samples: &[i16]) {
        self.lock().envelope.push(samples);
    }

    /// Start over, e.g. before measuring again from a fresh URL.
    fn reset(&self) {
        self.lock().envelope = Envelope::new();
    }

    async fn abandoned(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if self.lock().last_polled.elapsed() > WAVEFORM_ABANDONED {
                return;
            }
        }
    }
}

struct CachedWaveform {
    key: WaveformKey,
    /// Shared so concurrent requests for one track measure it once.
    job: Arc<WaveformJob>,
    last_used: Instant,
}

struct CachedCapture {
    page_url: Url,
    capture: Arc<Capture>,
    created: Instant,
    last_used: Instant,
}

pub struct Playback {
    ffmpeg: Arc<Ffmpeg>,
    http: reqwest::Client,
    entries: Mutex<Vec<Entry>>,
    waveforms: Mutex<Vec<CachedWaveform>>,
    captures: Mutex<Vec<CachedCapture>>,
    next_id: AtomicU64,
}

impl Playback {
    pub fn new(ffmpeg: Arc<Ffmpeg>) -> Result<Self, reqwest::Error> {
        // No overall timeout: a stream lasts as long as the listener wants.
        // Stalls are bounded by the read timeout instead.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        Ok(Self {
            ffmpeg,
            http,
            entries: Mutex::new(Vec::new()),
            waveforms: Mutex::new(Vec::new()),
            captures: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        })
    }

    fn with_entries<T>(&self, f: impl FnOnce(&mut Vec<Entry>) -> T) -> T {
        // Critical sections never panic midway, so a poisoned lock still
        // holds a consistent table.
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut entries)
    }

    fn insert(
        &self,
        origin: StreamOrigin,
        provider: ProviderKind,
        summary: String,
        duration_secs: Option<f64>,
        seekable: bool,
        source: Source,
    ) -> PreparedStream {
        let info = PreparedStream {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            origin,
            provider,
            summary,
            duration_secs,
            seekable,
        };
        self.with_entries(|entries| {
            if entries.len() >= MAX_PREPARED
                && let Some(oldest) = entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(i, _)| i)
            {
                entries.remove(oldest);
            }
            entries.push(Entry {
                info: info.clone(),
                source,
                last_used: Instant::now(),
            });
        });
        info
    }

    /// Prepare listening to a provider track that need not be downloaded.
    pub async fn prepare_remote(
        &self,
        provider: Arc<dyn MusicProvider>,
        page_url: Url,
    ) -> Result<PreparedStream, PlaybackError> {
        let reused = self.with_entries(|entries| {
            let entry = entries.iter_mut().find(|e| {
                matches!(&e.source, Source::Remote { page_url: u, resolved_at, .. }
                    if *u == page_url && resolved_at.elapsed() < REUSE_WINDOW)
            })?;
            entry.last_used = Instant::now();
            Some(entry.info.clone())
        });
        if let Some(info) = reused {
            return Ok(info);
        }

        let stream = provider.stream_source(&page_url).await?;
        tracing::info!(
            provider = %provider.kind(),
            url = %page_url,
            format = %stream.format.id,
            transport = ?stream.transport,
            "prepared provider stream"
        );
        let kind = provider.kind();
        Ok(self.insert(
            StreamOrigin::Provider,
            kind,
            stream.format.summary(),
            stream.duration_secs,
            stream.transport == StreamTransport::Progressive,
            Source::Remote {
                provider,
                page_url,
                stream: Arc::new(stream),
                resolved_at: Instant::now(),
            },
        ))
    }

    /// Prepare listening to a file in the library, for `viewer` only. The
    /// caller has checked that the viewer may access the file.
    pub async fn prepare_local(
        &self,
        provider: ProviderKind,
        path: PathBuf,
        quality: &QualityReport,
        viewer: UserId,
    ) -> Result<PreparedStream, PlaybackError> {
        let delivery = local_delivery(&path, quality.codec);
        let probe_path = path.clone();
        let duration_secs = tokio::task::spawn_blocking(move || probe_duration(&probe_path))
            .await
            .ok()
            .flatten()
            .map(|d| d.as_secs_f64());
        let summary = match delivery {
            LocalDelivery::AsIs { .. } => quality.summary.clone(),
            LocalDelivery::Flac => format!("{} (played as FLAC)", quality.summary),
        };
        Ok(self.insert(
            StreamOrigin::Library,
            provider,
            summary,
            duration_secs,
            matches!(delivery, LocalDelivery::AsIs { .. }),
            Source::Local {
                path,
                delivery,
                viewer,
            },
        ))
    }

    /// The source and expected length of a prepared stream that `viewer`
    /// may open.
    fn source(&self, id: StreamId, viewer: UserId) -> Result<(Source, Option<f64>), PlaybackError> {
        self.with_entries(|entries| {
            let entry = entries.iter_mut().find(|e| {
                e.info.id == id
                    && !matches!(&e.source, Source::Local { viewer: v, .. } if *v != viewer)
            })?;
            entry.last_used = Instant::now();
            Some((entry.source.clone(), entry.info.duration_secs))
        })
        .ok_or(PlaybackError::UnknownStream)
    }

    /// Replace the CDN URL of a provider stream after it expired.
    async fn refresh_remote(
        &self,
        id: StreamId,
        provider: &dyn MusicProvider,
        page_url: &Url,
    ) -> Result<Arc<StreamSource>, PlaybackError> {
        let fresh = Arc::new(provider.stream_source(page_url).await?);
        self.with_entries(|entries| {
            if let Some(Entry {
                source:
                    Source::Remote {
                        stream,
                        resolved_at,
                        ..
                    },
                ..
            }) = entries.iter_mut().find(|e| e.info.id == id)
            {
                *stream = Arc::clone(&fresh);
                *resolved_at = Instant::now();
            }
        });
        Ok(fresh)
    }

    /// Open a prepared stream: a byte range of it, or (for streams that are
    /// not seekable by range) the audio from `start_secs` on.
    pub async fn open(
        self: &Arc<Self>,
        id: StreamId,
        viewer: UserId,
        range: Option<ByteRange>,
        start_secs: Option<f64>,
    ) -> Result<StreamResponse, PlaybackError> {
        let (source, _) = self.source(id, viewer)?;
        let start = start_secs
            .filter(|s| s.is_finite())
            .map_or(0.0, |s| s.clamp(0.0, MAX_START_SECS));

        match source {
            Source::Local {
                path,
                delivery: LocalDelivery::AsIs { mime },
                ..
            } => serve_file(&path, mime, range).await,
            Source::Local {
                path,
                delivery: LocalDelivery::Flac,
                ..
            } => {
                if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    return Err(PlaybackError::FileMissing);
                }
                let pipe = self
                    .ffmpeg
                    .stream_audio(StreamInput::LocalFile(&path), start, PipeFormat::Flac)
                    .await?;
                Ok(pipe_response(pipe, PipeFormat::Flac))
            }
            Source::Remote {
                provider,
                page_url,
                stream,
                resolved_at,
            } => {
                let kind = provider.kind();
                if let Some(capture) =
                    self.capture_for(id, &provider, &page_url, &stream, resolved_at)
                {
                    match capture.offset_at(start, CAPTURE_WAIT).await {
                        Ok(Some(offset)) => return Ok(capture_response(capture.reader(offset))),
                        Ok(None) => {
                            return Err(PlaybackError::Upstream {
                                provider: kind,
                                reason: "no audio at this position".into(),
                            });
                        }
                        // Streaming straight from the CDN may still work.
                        Err(e) => {
                            tracing::info!(provider = %kind, url = %page_url, reason = %e, "capture unavailable; streaming directly");
                        }
                    }
                }
                if resolved_at.elapsed() < MAX_SOURCE_AGE {
                    match self.open_remote(kind, &stream, range, start).await {
                        Ok(response) => return Ok(response),
                        Err(RemoteFailure::Other(e)) => return Err(e),
                        Err(RemoteFailure::Expired(reason)) => {
                            tracing::info!(provider = %kind, url = %page_url, %reason, "stream URL rejected; resolving again");
                        }
                    }
                }
                let fresh = self
                    .refresh_remote(id, provider.as_ref(), &page_url)
                    .await?;
                self.open_remote(kind, &fresh, range, start).await.map_err(
                    |failure| match failure {
                        RemoteFailure::Other(e) => e,
                        RemoteFailure::Expired(reason) => PlaybackError::Upstream {
                            provider: kind,
                            reason,
                        },
                    },
                )
            }
        }
    }

    /// The waveform of a prepared stream: complete, or the part measured so
    /// far. The first request starts measuring in the background; it stops
    /// if nobody asks again for a while.
    pub async fn waveform(
        self: &Arc<Self>,
        id: StreamId,
        viewer: UserId,
    ) -> Result<Arc<Waveform>, PlaybackError> {
        let (source, expected_secs) = self.source(id, viewer)?;
        let key = match &source {
            Source::Remote { page_url, .. } => WaveformKey::Remote(page_url.clone()),
            Source::Local { path, .. } => {
                let meta = tokio::fs::metadata(path)
                    .await
                    .map_err(|_| PlaybackError::FileMissing)?;
                WaveformKey::Local {
                    path: path.clone(),
                    len: meta.len(),
                    modified: meta.modified().ok(),
                }
            }
        };
        let (job, started) = self.waveform_job(key, expected_secs);
        if started {
            tokio::spawn(Arc::clone(self).run_waveform(id, source, Arc::clone(&job)));
        }
        job.poll()
    }

    /// The job measuring `key`, and whether it was just created.
    fn waveform_job(
        &self,
        key: WaveformKey,
        expected_secs: Option<f64>,
    ) -> (Arc<WaveformJob>, bool) {
        let mut cache = self
            .waveforms
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = cache.iter_mut().find(|c| c.key == key) {
            cached.last_used = Instant::now();
            return (Arc::clone(&cached.job), false);
        }
        if cache.len() >= MAX_WAVEFORMS
            && let Some(oldest) = cache
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(i, _)| i)
        {
            cache.remove(oldest);
        }
        let job = Arc::new(WaveformJob::new(expected_secs));
        cache.push(CachedWaveform {
            key,
            job: Arc::clone(&job),
            last_used: Instant::now(),
        });
        (job, true)
    }

    async fn run_waveform(self: Arc<Self>, id: StreamId, source: Source, job: Arc<WaveformJob>) {
        let started = Instant::now();
        let measured = tokio::select! {
            result = self.measure(id, source, &job) => result.map_err(|e| e.to_string()),
            () = job.abandoned() => Err("nobody is listening any more".to_owned()),
        };
        let mut progress = job.lock();
        let outcome = measured.and_then(|()| {
            std::mem::take(&mut progress.envelope)
                .finish(waveform::SAMPLE_RATE, waveform::BARS)
                .map(Arc::new)
                .ok_or_else(|| PlaybackError::NoAudio.to_string())
        });
        match &outcome {
            Ok(waveform) => tracing::debug!(
                stream = id,
                secs = waveform.duration_secs,
                took_ms = started.elapsed().as_millis() as u64,
                "measured waveform"
            ),
            Err(reason) => tracing::info!(stream = id, %reason, "waveform not measured"),
        }
        let failed = outcome.is_err();
        progress.outcome = Some(outcome);
        drop(progress);
        if failed {
            // Playing the track again measures it again.
            self.waveforms
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|c| !Arc::ptr_eq(&c.job, &job));
        }
    }

    /// Decode the whole track into `job`. Provider audio is read from its
    /// capture when it has one, and otherwise fetched again for this,
    /// separately from the listener's stream.
    async fn measure(
        self: &Arc<Self>,
        id: StreamId,
        source: Source,
        job: &WaveformJob,
    ) -> Result<(), PlaybackError> {
        let sink = |samples: &[i16]| job.push(samples);
        let (provider, page_url, mut stream, resolved_at) = match source {
            Source::Local { path, .. } => {
                self.ffmpeg
                    .decode_mono(StreamInput::LocalFile(&path), waveform::SAMPLE_RATE, sink)
                    .await?;
                return Ok(());
            }
            Source::Remote {
                provider,
                page_url,
                stream,
                resolved_at,
            } => (provider, page_url, stream, resolved_at),
        };
        if let Some(capture) = self.capture_for(id, &provider, &page_url, &stream, resolved_at) {
            self.ffmpeg
                .decode_mono_from(capture.reader(0), "aac", waveform::SAMPLE_RATE, sink)
                .await?;
            return Ok(());
        }
        let mut refreshed = false;
        if resolved_at.elapsed() >= MAX_SOURCE_AGE {
            stream = self
                .refresh_remote(id, provider.as_ref(), &page_url)
                .await?;
            refreshed = true;
        }
        loop {
            let input = match stream.transport {
                StreamTransport::Progressive => StreamInput::Https {
                    url: &stream.media_url,
                    headers: &stream.headers,
                },
                StreamTransport::Hls => StreamInput::Hls {
                    url: &stream.media_url,
                    headers: &stream.headers,
                },
            };
            match self
                .ffmpeg
                .decode_mono(input, waveform::SAMPLE_RATE, sink)
                .await
            {
                // Most likely an expired CDN URL; worth one refresh.
                Err(ToolError::Failed { message, .. }) if !refreshed => {
                    tracing::info!(provider = %provider.kind(), url = %page_url, reason = %message, "waveform source rejected; resolving again");
                    job.reset();
                    stream = self
                        .refresh_remote(id, provider.as_ref(), &page_url)
                        .await?;
                    refreshed = true;
                }
                result => return result.map(|_| ()).map_err(Into::into),
            }
        }
    }

    /// The capture of a provider stream, started on first use. `None` when
    /// the stream is not captured: only AAC over HLS is, as long as it fits
    /// in memory.
    fn capture_for(
        self: &Arc<Self>,
        id: StreamId,
        provider: &Arc<dyn MusicProvider>,
        page_url: &Url,
        stream: &Arc<StreamSource>,
        resolved_at: Instant,
    ) -> Option<Arc<Capture>> {
        if stream.transport != StreamTransport::Hls || stream.format.codec != Codec::Aac {
            return None;
        }
        if let (Some(secs), Some(kbps)) = (stream.duration_secs, stream.format.bitrate_kbps) {
            // With some room for ADTS headers and bitrate peaks.
            let expected_bytes = secs * kbps * 1000.0 / 8.0 * 1.15;
            if expected_bytes > MAX_CAPTURE_BYTES as f64 {
                return None;
            }
        }
        let mut cache = self.captures.lock().unwrap_or_else(PoisonError::into_inner);
        cache.retain(|c| c.created.elapsed() < REUSE_WINDOW);
        if let Some(cached) = cache.iter_mut().find(|c| c.page_url == *page_url) {
            cached.last_used = Instant::now();
            return Some(Arc::clone(&cached.capture));
        }
        if cache.len() >= MAX_CAPTURES
            && let Some(oldest) = cache
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(i, _)| i)
        {
            cache.remove(oldest);
        }
        let capture = Capture::new();
        cache.push(CachedCapture {
            page_url: page_url.clone(),
            capture: Arc::clone(&capture),
            created: Instant::now(),
            last_used: Instant::now(),
        });
        drop(cache);
        tokio::spawn(Arc::clone(self).run_capture(
            id,
            Arc::clone(provider),
            page_url.clone(),
            Arc::clone(stream),
            resolved_at,
            Arc::clone(&capture),
        ));
        Some(capture)
    }

    async fn run_capture(
        self: Arc<Self>,
        id: StreamId,
        provider: Arc<dyn MusicProvider>,
        page_url: Url,
        stream: Arc<StreamSource>,
        resolved_at: Instant,
        capture: Arc<Capture>,
    ) {
        let kind = provider.kind();
        let started = Instant::now();
        let result = self
            .download_capture(
                id,
                provider.as_ref(),
                &page_url,
                stream,
                resolved_at,
                &capture,
            )
            .await;
        match result {
            Ok(()) => {
                capture.finish();
                tracing::debug!(
                    provider = %kind,
                    url = %page_url,
                    secs = capture.duration_secs(),
                    took_ms = started.elapsed().as_millis() as u64,
                    "captured provider stream"
                );
            }
            Err(e) => {
                tracing::warn!(provider = %kind, url = %page_url, reason = %e, "capturing provider stream failed");
                capture.fail(e.clone());
                // Too long stays known for this track; anything else is
                // worth another try when it is played again.
                if e != CaptureError::TooLarge {
                    self.captures
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .retain(|c| !Arc::ptr_eq(&c.capture, &capture));
                }
            }
        }
    }

    async fn download_capture(
        &self,
        id: StreamId,
        provider: &dyn MusicProvider,
        page_url: &Url,
        mut stream: Arc<StreamSource>,
        resolved_at: Instant,
        capture: &Capture,
    ) -> Result<(), CaptureError> {
        let failed = |e: &dyn std::fmt::Display| CaptureError::Failed(e.to_string());
        let mut refreshed = false;
        if resolved_at.elapsed() >= MAX_SOURCE_AGE {
            stream = self
                .refresh_remote(id, provider, page_url)
                .await
                .map_err(|e| failed(&e))?;
            refreshed = true;
        }
        let mut fast = self.segments_pipe(&stream).await;
        if let Err(SegmentsError::Fetch(e)) = &fast
            && e.is_expired()
            && !refreshed
        {
            tracing::info!(provider = %provider.kind(), url = %page_url, reason = %e, "capture source rejected; resolving again");
            stream = self
                .refresh_remote(id, provider, page_url)
                .await
                .map_err(|e| failed(&e))?;
            refreshed = true;
            fast = self.segments_pipe(&stream).await;
        }
        let (mut pipe, fetcher) = match fast {
            Ok(fast) => (fast.0, Some(fast.1)),
            Err(reason) => {
                tracing::debug!(provider = %provider.kind(), url = %page_url, %reason, "capturing through ffmpeg's HLS reader");
                (
                    self.hls_pipe(id, provider, page_url, stream, refreshed)
                        .await
                        .map_err(|e| failed(&e))?,
                    None,
                )
            }
        };
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let read = tokio::time::timeout(CAPTURE_WAIT, pipe.read(&mut buf))
                .await
                .map_err(|_| CaptureError::Failed("the CDN stopped sending audio".into()))?
                .map_err(|e| failed(&e))?;
            if read == 0 {
                // Only a clean exit, with every segment delivered, means
                // the whole track arrived.
                pipe.wait().await.map_err(|e| failed(&e))?;
                if let Some(mut fetcher) = fetcher {
                    (&mut fetcher.0)
                        .await
                        .map_err(|e| failed(&e))?
                        .map_err(|e| failed(&e))?;
                }
                return Ok(());
            }
            capture.append(&buf[..read])?;
        }
    }

    /// ffmpeg re-wrapping segments that Inkult fetches in parallel (see
    /// [`hls`]). Fails, with the reason, when the playlist is not one
    /// [`hls`] handles or cannot be fetched.
    async fn segments_pipe(
        &self,
        stream: &StreamSource,
    ) -> Result<(AudioPipe, AbortOnDrop<Result<(), hls::FetchError>>), SegmentsError> {
        let text = hls::fetch_playlist(&self.http, &stream.media_url, &stream.headers).await?;
        let playlist = hls::parse(&text, &stream.media_url).map_err(hls::FetchError::from)?;
        let demuxer = playlist.demuxer();
        let (writer, reader) = tokio::io::duplex(1024 * 1024);
        let http = self.http.clone();
        let headers = stream.headers.clone();
        let fetcher = AbortOnDrop(tokio::spawn(async move {
            hls::write_segments(&http, &playlist, &headers, MAX_CAPTURE_BYTES, writer).await
        }));
        let pipe = self
            .ffmpeg
            .stream_audio_from(reader, demuxer, PipeFormat::Adts)
            .await?;
        Ok((pipe, fetcher))
    }

    /// ffmpeg reading the HLS stream itself, resolving it again once if
    /// the CDN rejects it.
    async fn hls_pipe(
        &self,
        id: StreamId,
        provider: &dyn MusicProvider,
        page_url: &Url,
        mut stream: Arc<StreamSource>,
        mut refreshed: bool,
    ) -> Result<AudioPipe, PlaybackError> {
        loop {
            let input = StreamInput::Hls {
                url: &stream.media_url,
                headers: &stream.headers,
            };
            match self.ffmpeg.stream_audio(input, 0.0, PipeFormat::Adts).await {
                Ok(pipe) => return Ok(pipe),
                // Most likely an expired playlist URL; worth one refresh.
                Err(ToolError::Failed { message, .. }) if !refreshed => {
                    tracing::info!(provider = %provider.kind(), url = %page_url, reason = %message, "capture source rejected; resolving again");
                    stream = self.refresh_remote(id, provider, page_url).await?;
                    refreshed = true;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    async fn open_remote(
        &self,
        provider: ProviderKind,
        stream: &StreamSource,
        range: Option<ByteRange>,
        start: f64,
    ) -> Result<StreamResponse, RemoteFailure> {
        match stream.transport {
            StreamTransport::Progressive => self.proxy(provider, stream, range).await,
            StreamTransport::Hls => {
                let format = hls_pipe_format(stream.format.codec).ok_or_else(|| {
                    RemoteFailure::Other(PlaybackError::Upstream {
                        provider,
                        reason: format!("cannot stream {} from HLS", stream.format.codec),
                    })
                })?;
                let input = StreamInput::Hls {
                    url: &stream.media_url,
                    headers: &stream.headers,
                };
                match self.ffmpeg.stream_audio(input, start, format).await {
                    Ok(pipe) => Ok(pipe_response(pipe, format)),
                    // Most likely an expired playlist URL; worth one refresh.
                    Err(ToolError::Failed { message, .. }) => Err(RemoteFailure::Expired(message)),
                    Err(e) => Err(RemoteFailure::Other(e.into())),
                }
            }
        }
    }

    /// Forward a (range) request to the CDN and pass its answer through.
    async fn proxy(
        &self,
        provider: ProviderKind,
        stream: &StreamSource,
        range: Option<ByteRange>,
    ) -> Result<StreamResponse, RemoteFailure> {
        let mut request = self.http.get(stream.media_url.clone());
        for (name, value) in &stream.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(range) = range {
            request = request.header(reqwest::header::RANGE, range.header_value());
        }
        // `without_url`: signed CDN URLs must not reach logs or the UI.
        let response = request.send().await.map_err(|e| {
            RemoteFailure::Other(PlaybackError::Upstream {
                provider,
                reason: e.without_url().to_string(),
            })
        })?;

        let status = response.status();
        match status {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {}
            StatusCode::RANGE_NOT_SATISFIABLE => {
                return Err(RemoteFailure::Other(PlaybackError::RangeNotSatisfiable {
                    total: total_from_content_range(response.headers()).unwrap_or(0),
                }));
            }
            StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::GONE => {
                return Err(RemoteFailure::Expired(format!("CDN answered {status}")));
            }
            _ => {
                return Err(RemoteFailure::Other(PlaybackError::Upstream {
                    provider,
                    reason: format!("CDN answered {status}"),
                }));
            }
        }
        let content_range = (status == StatusCode::PARTIAL_CONTENT)
            .then(|| response.headers().get(reqwest::header::CONTENT_RANGE))
            .flatten()
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok(StreamResponse {
            content_type: container_mime(stream.format.container.as_deref()),
            content_length: response.content_length(),
            content_range,
            ranges: true,
            body: StreamBody::Upstream(response),
        })
    }
}

/// Why segments could not be fetched in parallel.
#[derive(Debug, thiserror::Error)]
enum SegmentsError {
    #[error(transparent)]
    Fetch(#[from] hls::FetchError),
    #[error(transparent)]
    Tool(#[from] ToolError),
}

fn capture_response(reader: CaptureReader) -> StreamResponse {
    StreamResponse {
        content_type: PipeFormat::Adts.mime(),
        content_length: None,
        content_range: None,
        ranges: false,
        body: StreamBody::Capture(reader),
    }
}

fn pipe_response(pipe: AudioPipe, format: PipeFormat) -> StreamResponse {
    StreamResponse {
        content_type: format.mime(),
        content_length: None,
        content_range: None,
        ranges: false,
        body: StreamBody::Pipe(pipe),
    }
}

async fn serve_file(
    path: &Path,
    mime: &'static str,
    range: Option<ByteRange>,
) -> Result<StreamResponse, PlaybackError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| PlaybackError::FileMissing)?;
    let io = |source| PlaybackError::Io {
        path: path.to_path_buf(),
        source,
    };
    let total = file.metadata().await.map_err(io)?.len();
    let (start, len, content_range) = match range.map(|r| r.resolve(total)) {
        None => (0, total, None),
        Some(None) => return Err(PlaybackError::RangeNotSatisfiable { total }),
        Some(Some((start, end))) => (
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{total}")),
        ),
    };
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await.map_err(io)?;
    }
    Ok(StreamResponse {
        content_type: mime,
        content_length: Some(len),
        content_range,
        ranges: true,
        body: StreamBody::File(file.take(len)),
    })
}

/// How a library file reaches the browser. AIFF and ALAC are not decoded
/// by Chrome or Firefox; they are converted losslessly to FLAC instead.
fn local_delivery(path: &Path, codec: Codec) -> LocalDelivery {
    let mime = path
        .file_name()
        .map(|n| audio_mime(&n.to_string_lossy()))
        .unwrap_or("application/octet-stream");
    match (codec, mime) {
        (Codec::Alac | Codec::Other, _) | (_, "audio/aiff" | "application/octet-stream") => {
            LocalDelivery::Flac
        }
        (_, mime) => LocalDelivery::AsIs { mime },
    }
}

/// Container to stream an HLS rendition in without re-encoding.
fn hls_pipe_format(codec: Codec) -> Option<PipeFormat> {
    match codec {
        Codec::Aac => Some(PipeFormat::Adts),
        Codec::Mp3 => Some(PipeFormat::Mp3),
        Codec::Opus => Some(PipeFormat::Ogg),
        _ => None,
    }
}

fn container_mime(container: Option<&str>) -> &'static str {
    match container.map(str::to_ascii_lowercase).as_deref() {
        Some("webm") => "audio/webm",
        Some("m4a" | "mp4") => "audio/mp4",
        Some(other) => audio_mime(&format!("x.{other}")),
        None => "application/octet-stream",
    }
}

/// MIME type of an audio file from its extension.
pub fn audio_mime(file_name: &str) -> &'static str {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "opus" | "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "aiff" | "aif" => "audio/aiff",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "aac" => "audio/aac",
        _ => "application/octet-stream",
    }
}

/// Total length from a `Content-Range: bytes */N` (or `a-b/N`) header.
fn total_from_content_range(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality(codec: Codec, container: &str) -> QualityReport {
        QualityReport::new(codec, container.into(), None, None, None, None)
    }

    #[test]
    fn library_files_browsers_cannot_decode_become_flac() {
        let p = |name: &str| PathBuf::from(format!("/music/{name}"));
        assert_eq!(
            local_delivery(&p("a.opus"), Codec::Opus),
            LocalDelivery::AsIs { mime: "audio/ogg" }
        );
        assert_eq!(
            local_delivery(&p("a.m4a"), Codec::Aac),
            LocalDelivery::AsIs { mime: "audio/mp4" }
        );
        assert_eq!(
            local_delivery(&p("a.m4a"), Codec::Alac),
            LocalDelivery::Flac
        );
        assert_eq!(
            local_delivery(&p("a.aiff"), Codec::Pcm),
            LocalDelivery::Flac
        );
        assert_eq!(
            local_delivery(&p("a.wav"), Codec::Pcm),
            LocalDelivery::AsIs { mime: "audio/wav" }
        );
    }

    #[test]
    fn hls_is_only_rewrapped_never_reencoded() {
        assert_eq!(hls_pipe_format(Codec::Aac), Some(PipeFormat::Adts));
        assert_eq!(hls_pipe_format(Codec::Opus), Some(PipeFormat::Ogg));
        assert_eq!(hls_pipe_format(Codec::Vorbis), None);
        assert_eq!(container_mime(Some("webm")), "audio/webm");
        assert_eq!(container_mime(Some("M4A")), "audio/mp4");
        assert_eq!(container_mime(Some("mp3")), "audio/mpeg");
    }

    #[tokio::test]
    async fn serves_library_files_with_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("01 - Track.mp3");
        tokio::fs::write(&path, b"0123456789").await.unwrap();
        let playback = Arc::new(Playback::new(Arc::new(Ffmpeg::new("ffmpeg"))).unwrap());
        let prepared = playback
            .prepare_local(
                ProviderKind::YouTube,
                path.clone(),
                &quality(Codec::Mp3, "mp3"),
                UserId(1),
            )
            .await
            .unwrap();
        assert!(prepared.seekable);
        assert_eq!(prepared.origin, StreamOrigin::Library);

        let response = playback
            .open(prepared.id, UserId(1), Some(ByteRange::FromTo(2, 4)), None)
            .await
            .unwrap();
        assert_eq!(response.content_type, "audio/mpeg");
        assert_eq!(response.content_length, Some(3));
        assert_eq!(response.content_range.as_deref(), Some("bytes 2-4/10"));
        let StreamBody::File(mut body) = response.body else {
            panic!("expected a file body");
        };
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"234");

        assert!(matches!(
            playback
                .open(prepared.id, UserId(1), Some(ByteRange::From(10)), None)
                .await,
            Err(PlaybackError::RangeNotSatisfiable { total: 10 })
        ));
        // Another user cannot open a library stream by guessing its id.
        assert!(matches!(
            playback.open(prepared.id, UserId(2), None, None).await,
            Err(PlaybackError::UnknownStream)
        ));
        assert!(matches!(
            playback
                .open(prepared.id + 100, UserId(1), None, None)
                .await,
            Err(PlaybackError::UnknownStream)
        ));
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(matches!(
            playback.open(prepared.id, UserId(1), None, None).await,
            Err(PlaybackError::FileMissing)
        ));
    }

    /// 16-bit stereo WAV: `silent` seconds of silence, then `loud` seconds
    /// of a square wave.
    fn wav(rate: u32, silent: u32, loud: u32) -> Vec<u8> {
        let frames = rate * (silent + loud);
        let data_len = frames * 4;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let v: i16 = if frame < rate * silent {
                0
            } else if (frame / 50) % 2 == 0 {
                20000
            } else {
                -20000
            };
            out.extend_from_slice(&v.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    #[tokio::test]
    async fn waveform_matches_the_audio_timeline() {
        let ffmpeg = Arc::new(Ffmpeg::new("ffmpeg"));
        if !ffmpeg.status().await.available() {
            eprintln!("skipped: ffmpeg not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("01 - Track.wav");
        tokio::fs::write(&path, wav(44_100, 3, 1)).await.unwrap();
        let playback = Arc::new(Playback::new(ffmpeg).unwrap());
        let prepared = playback
            .prepare_local(
                ProviderKind::YouTube,
                path.clone(),
                &quality(Codec::Pcm, "wav"),
                UserId(1),
            )
            .await
            .unwrap();

        // Measured in the background; polled like the UI does.
        let wf = loop {
            let wf = playback.waveform(prepared.id, UserId(1)).await.unwrap();
            if wf.complete {
                break wf;
            }
            assert!(wf.measured_secs <= wf.duration_secs);
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            (wf.duration_secs - 4.0).abs() < 0.01,
            "{}",
            wf.duration_secs
        );
        // One bar per 10 ms window for clips this short.
        assert_eq!(wf.bars.len(), 400);
        // Silent for the first three quarters, loud for the last; allow a
        // bar of resampler smear at the edge.
        let edge = wf.bars.len() * 3 / 4;
        assert!(wf.bars[..edge - 1].iter().all(|&b| b == 0));
        assert!(wf.bars[edge + 1..].iter().all(|&b| b > 240));

        // Cached per file, and private to the viewer like the stream.
        let again = playback.waveform(prepared.id, UserId(1)).await.unwrap();
        assert!(Arc::ptr_eq(&wf, &again));
        assert!(matches!(
            playback.waveform(prepared.id, UserId(2)).await,
            Err(PlaybackError::UnknownStream)
        ));
    }

    #[tokio::test]
    async fn table_is_bounded() {
        let playback = Arc::new(Playback::new(Arc::new(Ffmpeg::new("ffmpeg"))).unwrap());
        let q = quality(Codec::Mp3, "mp3");
        let mut first = None;
        for i in 0..(MAX_PREPARED + 5) {
            let prepared = playback
                .prepare_local(
                    ProviderKind::YouTube,
                    PathBuf::from(format!("/nope/{i}.mp3")),
                    &q,
                    UserId(1),
                )
                .await
                .unwrap();
            first.get_or_insert(prepared.id);
        }
        assert_eq!(playback.with_entries(|e| e.len()), MAX_PREPARED);
        assert!(matches!(
            playback.open(first.unwrap(), UserId(1), None, None).await,
            Err(PlaybackError::UnknownStream)
        ));
    }
}
