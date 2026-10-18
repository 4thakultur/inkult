//! The per-track processing pipeline:
//!
//! ```text
//! resolve metadata → duplicate check → select source format → download
//!   → [re-encode, only if the user asked for MP3/AIFF] → verify + tag + artwork
//!   → place into library (no clobber)
//! ```
//!
//! Converting a library file runs a shorter pipeline ([`convert_existing`]):
//!
//! ```text
//! duplicate check → pick the best copy on disk → re-encode → verify + copy tags
//!   → place next to the source (no clobber)
//! ```
//!
//! A quality upgrade ([`upgrade`]) hands the search and transfer to the
//! user's Soulseek session (see `crate::soulseek::upgrade`), which files
//! the verified copy as the track's Lossless format; the job reports it.
//!
//! All work happens in a per-track staging directory; nothing touches the
//! library until the file is complete, verified and tagged, and a library
//! file used as a conversion source is only ever read.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lofty::picture::MimeType;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{JobHandle, JobState, RetryPolicy};
use crate::domain::DownloadProgress;
use crate::domain::length::TooLong;
use crate::domain::provider::TrackKey;
use crate::domain::quality::{
    AudioFormat, Codec, Conversion, OutputFormat, QualityReport, SpectrumVerdict,
};
use crate::library::sanitize::sanitize_component;
use crate::library::{LibraryEntry, LibraryError};
use crate::metadata::tagging;
use crate::providers::{
    DownloadedMedia, MusicProvider, ProviderError, TrackDetails, is_provider_image_url,
};
use crate::soulseek::{Session as SoulseekSession, UpgradeError, UpgradeProgress, UpgradeWanted};
use crate::spectrum;

/// Artwork larger than this is ignored rather than embedded.
const MAX_ARTWORK_BYTES: u64 = 16 * 1024 * 1024;

/// What a job was asked to do.
pub struct JobRequest {
    pub url: Url,
    /// Album position from the listing the track was queued from.
    pub track_number_hint: Option<u32>,
    pub output: OutputFormat,
}

/// Convert a library track to another format.
pub struct ConversionRequest {
    pub key: TrackKey,
    pub source_url: String,
    pub output: OutputFormat,
}

/// Find a lossless copy of a track on Soulseek.
pub struct UpgradeRequest {
    /// The track's source, for duplicate detection and display.
    pub source_url: String,
    pub basis: UpgradeBasis,
    pub session: Arc<SoulseekSession>,
}

/// What an upgrade starts from.
pub enum UpgradeBasis {
    /// A library track: what to look for is already known.
    Library(Box<UpgradeWanted>),
    /// A provider's track (a search result), which may not be in the
    /// library: its metadata is resolved first, nothing is downloaded.
    Source {
        provider: Arc<dyn MusicProvider>,
        url: Url,
    },
}

pub enum Outcome {
    Completed(LibraryEntry),
    /// The track was already on disk; nothing was downloaded.
    Skipped {
        reason: String,
        existing: LibraryEntry,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("downloaded file failed verification: {0}")]
    InvalidMedia(String),
    #[error("could not write metadata: {0}")]
    Tagging(String),
    #[error(transparent)]
    TooLong(#[from] TooLong),
    #[error("this source offers no AAC stream; choose “Best quality” or “MP3 320 kbps” instead")]
    NoAacSource,
    #[error("conversion to {target} failed: {reason}")]
    Conversion {
        target: &'static str,
        reason: String,
    },
    #[error("no copy of this track is left in the library to convert from")]
    SourceMissing,
    #[error("cannot convert to {target}: {reason}")]
    NotConvertible {
        target: &'static str,
        reason: String,
    },
    #[error("could not add file to library: {0}")]
    Library(#[from] LibraryError),
    #[error(transparent)]
    Upgrade(UpgradeError),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

impl PipelineError {
    pub fn is_retryable(&self) -> bool {
        match self {
            PipelineError::Provider(e) => e.is_retryable(),
            _ => false,
        }
    }

    fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| PipelineError::Io { context, source }
    }
}

pub(super) async fn run(
    job: &JobHandle,
    provider: Arc<dyn MusicProvider>,
    request: JobRequest,
    cancel: &CancellationToken,
) -> Result<Outcome, PipelineError> {
    let ctx = job.ctx();
    let output = request.output;
    job.set_state(JobState::Resolving);
    let mut details = retrying(ctx.retry, job, cancel, || {
        provider.track_details(&request.url)
    })
    .await?;
    if details.metadata.track_number.is_none() && details.metadata.album.is_some() {
        details.metadata.track_number = request.track_number_hint;
    }
    let meta = &details.metadata;
    tracing::info!(job = job.id(), key = %meta.key(), title = %meta.title, "resolved track");
    job.update(|s| {
        s.track_key = Some(meta.key());
        s.title = meta.title.clone();
        s.artists = meta.artists.clone();
        s.thumbnail_url = meta.artwork_url.clone().or(s.thumbnail_url.take());
        let mut shown = meta.clone();
        shown.description = None;
        s.metadata = Some(shown);
    });

    // Before the library lookup too: a long track that is already on disk
    // (from before the limit) is not handed out to more users.
    ctx.max_track_length.get().check(meta.duration_secs)?;

    let key = meta.key();
    let Some(_in_flight) = job.claim_or_wait((key.clone(), output), cancel).await else {
        return Err(PipelineError::Cancelled);
    };
    // The library is shared by all users: a file someone else already
    // downloaded is simply added to this user's library (in `finish`).
    if let Some(existing) = ctx.library.find(&key, output).await {
        return Ok(Outcome::Skipped {
            reason: format!(
                "Already on disk as {}; added to your library without downloading again",
                output.label()
            ),
            existing,
        });
    }

    if details.formats.is_empty() {
        return Err(ProviderError::NoAudio {
            provider: provider.kind(),
        }
        .into());
    }
    let format = output
        .select_source(&details.formats)
        .cloned()
        .ok_or(PipelineError::NoAacSource)?;
    tracing::info!(job = job.id(), format = %format.id, quality = %format.summary(), "selected source format");
    job.update(|s| {
        s.source_format_summary = Some(format.summary());
        s.source_format = Some(format.clone());
    });

    // Keyed by track identity and requested output (the in-flight claim
    // above makes that pair unique among running jobs), so an interrupted
    // download resumes on retry and concurrent jobs never share a directory.
    let work_dir = ctx.staging_dir.join(sanitize_component(
        &format!("{}-{}-{}", key.provider.slug(), key.id, output.slug()),
        "track",
    ));
    tokio::fs::create_dir_all(&work_dir)
        .await
        .map_err(PipelineError::io(format!(
            "creating {}",
            work_dir.display()
        )))?;

    let result = download_and_import(
        job,
        provider.as_ref(),
        &details,
        &format,
        output,
        &work_dir,
        cancel,
    )
    .await;
    let keep_partial_download = result.as_ref().is_err_and(PipelineError::is_retryable);
    if !keep_partial_download && let Err(e) = tokio::fs::remove_dir_all(&work_dir).await {
        tracing::warn!(dir = %work_dir.display(), error = %e, "could not clean staging directory");
    }
    result.map(Outcome::Completed)
}

async fn download_and_import(
    job: &JobHandle,
    provider: &dyn MusicProvider,
    details: &TrackDetails,
    format: &AudioFormat,
    output: OutputFormat,
    work_dir: &Path,
    cancel: &CancellationToken,
) -> Result<LibraryEntry, PipelineError> {
    let ctx = job.ctx();
    job.set_state(JobState::Downloading);
    let on_progress = |p: DownloadProgress| {
        job.update(|s| {
            s.progress_fraction = p.fraction();
            s.progress = Some(p);
        })
    };
    let media = retrying(ctx.retry, job, cancel, || {
        provider.download(details, format, work_dir, &on_progress, cancel)
    })
    .await?;

    job.set_state(JobState::Processing);
    // The provider did not state a length up front: judge the real file.
    if details.metadata.duration_secs.is_none() {
        let audio = media.audio_path.clone();
        let probed = tokio::task::spawn_blocking(move || tagging::probe_duration(&audio))
            .await
            .ok()
            .flatten();
        ctx.max_track_length
            .get()
            .check(probed.map(|d| d.as_secs_f64()))?;
    }
    let (audio_path, conversion) = match output.conversion() {
        Some(conversion) => {
            let target = media
                .audio_path
                .with_file_name(format!("converted.{}", conversion.extension()));
            convert(
                job,
                &media.audio_path,
                &target,
                &format.summary(),
                conversion,
                cancel,
            )
            .await?;
            (target, Some(conversion))
        }
        None => (media.audio_path.clone(), None),
    };
    let artwork = prepare_artwork(job, details, &media).await;
    let artwork_embedded = artwork.is_some();
    let expected_codec = conversion.map_or(format.codec, Conversion::codec);
    let mut quality = verify_and_tag(&audio_path, details, expected_codec, artwork).await?;
    if conversion.is_some() {
        quality = quality.converted_from(format);
    } else if quality.codec.is_lossless() {
        // A lossless original (e.g. a SoundCloud WAV upload) is often an
        // MP3 the uploader decoded: check before calling it lossless.
        job.update(|s| s.note = Some("Checking the spectrum".into()));
        let secs = details.metadata.duration_secs;
        if let Some(check) =
            spectrum::check_lossless(&ctx.ffmpeg, &audio_path, &quality, secs).await
        {
            if check.verdict == SpectrumVerdict::Lossy {
                job.update(|s| {
                    s.warnings
                        .push(format!("Not really lossless: {}", check.summary))
                });
            }
            quality = quality.with_spectrum(check);
        }
    }
    tracing::info!(job = job.id(), quality = %quality.summary, "verified and tagged");
    job.update(|s| {
        s.progress = None;
        s.quality = Some(quality.clone());
        s.artwork_embedded = Some(artwork_embedded);
    });

    if cancel.is_cancelled() {
        return Err(PipelineError::Cancelled);
    }
    job.set_state(JobState::Organizing);
    let extension = audio_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("audio")
        .to_ascii_lowercase();
    Ok(ctx
        .library
        .import(
            audio_path,
            &details.metadata,
            quality,
            output,
            &extension,
            job.requesters(),
        )
        .await?)
}

fn conversion_label(conversion: Conversion) -> &'static str {
    match conversion {
        Conversion::Mp3Cbr320 => "MP3 320 kbps",
        Conversion::AiffPcm16 => "AIFF",
    }
}

/// Re-encode `source` into `output` (inside a work dir). The source is left
/// in place, so a failed conversion never loses it.
async fn convert(
    job: &JobHandle,
    source: &Path,
    output: &Path,
    from: &str,
    conversion: Conversion,
    cancel: &CancellationToken,
) -> Result<(), PipelineError> {
    let target = conversion_label(conversion);
    tracing::info!(job = job.id(), %from, to = target, "converting");
    job.update(|s| s.note = Some(format!("Converting {from} → {target}")));

    // A leftover from an interrupted earlier attempt would make ffmpeg's
    // no-overwrite mode fail; it is our own partial file, so remove it.
    let _ = tokio::fs::remove_file(output).await;
    tokio::select! {
        result = job.ctx().ffmpeg.convert(source, output, conversion) => {
            result.map_err(|e| PipelineError::Conversion { target, reason: e.to_string() })?;
        }
        // Dropping the conversion future kills ffmpeg (kill_on_drop).
        _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
    }
    job.update(|s| s.note = None);
    Ok(())
}

/// Convert a track that is already in the library into `request.output`.
pub(super) async fn convert_existing(
    job: &JobHandle,
    request: ConversionRequest,
    cancel: &CancellationToken,
) -> Result<Outcome, PipelineError> {
    let ctx = job.ctx();
    let ConversionRequest { key, output, .. } = request;
    job.set_state(JobState::Processing);
    let Some(_in_flight) = job.claim_or_wait((key.clone(), output), cancel).await else {
        return Err(PipelineError::Cancelled);
    };
    if let Some(existing) = ctx.library.find(&key, output).await {
        return Ok(Outcome::Skipped {
            reason: format!(
                "Already on disk as {}; added to your library without converting again",
                output.label()
            ),
            existing,
        });
    }

    let (source, source_path) = ctx
        .library
        .best_source(&key)
        .await
        .ok_or(PipelineError::SourceMissing)?;
    let conversion = output
        .library_conversion(&source.quality)
        .map_err(|reason| PipelineError::NotConvertible {
            target: output.label(),
            reason,
        })?;
    job.update(|s| {
        s.source_format_summary = Some(source.quality.summary.clone());
        s.quality = None;
    });

    let work_dir = ctx.staging_dir.join(sanitize_component(
        &format!(
            "convert-{}-{}-{}",
            key.provider.slug(),
            key.id,
            output.slug()
        ),
        "track",
    ));
    tokio::fs::create_dir_all(&work_dir)
        .await
        .map_err(PipelineError::io(format!(
            "creating {}",
            work_dir.display()
        )))?;
    let result = convert_into_library(
        job,
        &source,
        &source_path,
        conversion,
        output,
        &work_dir,
        cancel,
    )
    .await;
    if let Err(e) = tokio::fs::remove_dir_all(&work_dir).await {
        tracing::warn!(dir = %work_dir.display(), error = %e, "could not clean staging directory");
    }
    result.map(Outcome::Completed)
}

/// Upgrade a library track with a lossless copy from Soulseek.
pub(super) async fn upgrade(
    job: &JobHandle,
    request: UpgradeRequest,
    cancel: &CancellationToken,
) -> Result<Outcome, PipelineError> {
    let UpgradeRequest { basis, session, .. } = request;
    let output = OutputFormat::Lossless;
    let ctx = job.ctx();
    job.set_state(JobState::Resolving);
    let (key, details) = match &basis {
        UpgradeBasis::Library(wanted) => (wanted.key.clone(), None),
        UpgradeBasis::Source { provider, url } => {
            job.update(|s| s.note = Some("Looking the track up at its source".into()));
            let details = retrying(ctx.retry, job, cancel, || provider.track_details(url)).await?;
            let meta = &details.metadata;
            job.update(|s| {
                s.track_key = Some(meta.key());
                s.title = meta.title.clone();
                s.artists = meta.artists.clone();
                s.thumbnail_url = meta.artwork_url.clone().or(s.thumbnail_url.take());
                let mut shown = meta.clone();
                shown.description = None;
                s.metadata = Some(shown);
            });
            ctx.max_track_length.get().check(meta.duration_secs)?;
            (meta.key(), Some(details))
        }
    };
    let Some(_in_flight) = job.claim_or_wait((key.clone(), output), cancel).await else {
        return Err(PipelineError::Cancelled);
    };
    if let Some(existing) = ctx.library.find(&key, output).await {
        return Ok(Outcome::Skipped {
            reason: "A lossless copy is already on disk; added to your library".into(),
            existing,
        });
    }
    let wanted = match (basis, details) {
        (UpgradeBasis::Library(wanted), _) => *wanted,
        // The provider's copy may be in the library already: then the
        // upgrade is tagged like that copy, as one asked for from the library.
        (UpgradeBasis::Source { .. }, Some(details)) => {
            match UpgradeWanted::from_library(&ctx.library, &key).await {
                Some(wanted) => wanted,
                None => wanted_from_source(job, details).await,
            }
        }
        (UpgradeBasis::Source { .. }, None) => return Err(PipelineError::SourceMissing),
    };

    // Which copy is being tried, for the notes of the later stages.
    let trying = std::sync::Mutex::new(String::new());
    let report = |progress: UpgradeProgress| {
        match progress {
        UpgradeProgress::Searching {
            query,
            replies,
            candidates,
        } => job.update(|s| {
            s.state = JobState::Resolving;
            s.note = Some(match replies {
                0 => format!("Searching Soulseek for “{query}”"),
                _ => format!(
                    "Searching Soulseek for “{query}”: {replies} {} answered, {candidates} lossless {}",
                    if replies == 1 { "user" } else { "users" },
                    if candidates == 1 { "match" } else { "matches" },
                ),
            });
        }),
        UpgradeProgress::Trying {
            attempt,
            of,
            candidate,
        } => {
            let (_, file) = crate::soulseek::split_path(&candidate.path);
            let label = if of > 1 {
                format!("Copy {attempt} of {of}: {file} from {}", candidate.username)
            } else {
                format!("{file} from {}", candidate.username)
            };
            *super::lock(&trying) = label.clone();
            job.update(|s| {
                s.state = JobState::Downloading;
                s.source_format_summary = Some(candidate.summary.clone());
                s.progress = None;
                s.progress_fraction = None;
                s.note = Some(label);
            });
        }
        UpgradeProgress::Waiting { note } => {
            let label = super::lock(&trying).clone();
            job.update(|s| {
                s.state = JobState::Downloading;
                s.note = Some(match note {
                    Some(note) => format!("{label} · {note}"),
                    None => label,
                });
            });
        }
        UpgradeProgress::Transferring {
            bytes,
            size,
            speed_bytes_per_sec,
        } => {
            let label = super::lock(&trying).clone();
            let progress = DownloadProgress {
                downloaded_bytes: Some(bytes),
                total_bytes: Some(size),
                speed_bytes_per_sec,
            };
            job.update(|s| {
                s.state = JobState::Downloading;
                s.progress_fraction = progress.fraction();
                s.progress = Some(progress);
                s.note = Some(label);
            });
        }
        UpgradeProgress::Importing => job.update(|s| {
            s.state = JobState::Processing;
            s.progress = None;
            s.note = Some("Checking that it is lossless and the same length, then tagging".into());
        }),
    }
    };
    let result = session.upgrade(&wanted, cancel, &report).await;
    // A cover fetched for this upgrade is used up once it is filed (the
    // import removes it). After a failure it is not needed either, unless
    // a transfer will resume with the next sign-in.
    if let Some(cover) = &wanted.artwork_file
        && result
            .as_ref()
            .is_err_and(|e| !matches!(e, UpgradeError::SessionEnded))
    {
        let _ = tokio::fs::remove_file(cover).await;
    }
    match result {
        Ok(entry) => {
            job.update(|s| {
                s.quality = Some(entry.quality.clone());
                s.source_format_summary = Some(entry.quality.summary.clone());
                s.note = None;
            });
            Ok(Outcome::Completed(entry))
        }
        Err(UpgradeError::Cancelled) => Err(PipelineError::Cancelled),
        Err(e) => Err(PipelineError::Upgrade(e)),
    }
}

/// What to look for, and how to tag the result, for a track known only
/// from its provider. The cover is fetched now (no audio is downloaded)
/// and kept in the staging directory until the upgrade is filed.
async fn wanted_from_source(job: &JobHandle, details: TrackDetails) -> UpgradeWanted {
    let artwork_file = fetch_cover(job, &details).await;
    let mut meta = details.metadata;
    // Saved with the Soulseek download; descriptions can be long.
    meta.description = None;
    UpgradeWanted {
        key: meta.key(),
        title: meta.title.clone(),
        artists: meta.artists.clone(),
        duration_secs: meta.duration_secs,
        metadata: Some(meta),
        artwork_file,
    }
}

/// Download the provider's cover for a track, cropped like downloads'
/// covers. Problems are job warnings; a missing cover never fails the job.
async fn fetch_cover(job: &JobHandle, details: &TrackDetails) -> Option<PathBuf> {
    let ctx = job.ctx();
    let warn = |message: String| {
        tracing::warn!(job = job.id(), "{message}");
        job.update(|s| s.warnings.push(message));
    };
    let Some(url) = details.metadata.artwork_url.as_deref() else {
        warn("No artwork was available from the provider".into());
        return None;
    };
    // Provider metadata is untrusted: only fetch from the providers' CDNs.
    if !is_provider_image_url(url) {
        warn("Artwork skipped: not a provider image address".into());
        return None;
    }
    let data = match fetch_limited(&ctx.http, url, MAX_ARTWORK_BYTES).await {
        Ok(data) => data,
        Err(e) => {
            warn(format!("Artwork skipped: {e}"));
            return None;
        }
    };
    let key = details.metadata.key();
    let dir = ctx.staging_dir.join("upgrade-covers");
    let base = sanitize_component(&format!("{}-{}", key.provider.slug(), key.id), "cover");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        warn(format!("Artwork skipped: {e}"));
        return None;
    }
    let raw = dir.join(format!("{base}.download"));
    if let Err(e) = tokio::fs::write(&raw, &data).await {
        warn(format!("Artwork skipped: {e}"));
        return None;
    }
    // ffmpeg picks the image format from the extension; JPEG and PNG
    // covers are kept as they are (the tagger reads the type from the data).
    let target = dir.join(format!("{base}.jpg"));
    let result = if details.artwork_needs_square_crop {
        ctx.ffmpeg
            .crop_to_square(&raw, &target)
            .await
            .map_err(|e| format!("could not crop it: {e}"))
    } else if tagging::artwork_mime(&data).is_some() {
        tokio::fs::rename(&raw, &target)
            .await
            .map_err(|e| e.to_string())
    } else {
        ctx.ffmpeg
            .image_to_jpeg(&raw, &target)
            .await
            .map_err(|e| format!("could not convert it to JPEG: {e}"))
    };
    let _ = tokio::fs::remove_file(&raw).await;
    match result {
        Ok(()) => Some(target),
        Err(e) => {
            warn(format!("Artwork skipped: {e}"));
            None
        }
    }
}

/// GET `url`, refusing bodies over `max` bytes.
async fn fetch_limited(http: &reqwest::Client, url: &str, max: u64) -> Result<Vec<u8>, String> {
    let mut response = http
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.without_url().to_string())?;
    if response.content_length().is_some_and(|len| len > max) {
        return Err("too large".into());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        data.extend_from_slice(&chunk);
        if data.len() as u64 > max {
            return Err("too large".into());
        }
    }
    Ok(data)
}

async fn convert_into_library(
    job: &JobHandle,
    source: &LibraryEntry,
    source_path: &Path,
    conversion: Conversion,
    output: OutputFormat,
    work_dir: &Path,
    cancel: &CancellationToken,
) -> Result<LibraryEntry, PipelineError> {
    let target = work_dir.join(format!("converted.{}", conversion.extension()));
    convert(
        job,
        source_path,
        &target,
        &source.quality.summary,
        conversion,
        cancel,
    )
    .await?;

    let (tag_source, tag_target) = (source_path.to_path_buf(), target.clone());
    let (track_key, source_url) = (source.key.to_string(), source.source_url.clone());
    let codec = conversion.codec();
    let (quality, artwork) = tokio::task::spawn_blocking(move || {
        let quality = tagging::probe_quality(&tag_target, codec)
            .map_err(|e| PipelineError::InvalidMedia(e.to_string()))?;
        let artwork = tagging::copy_tags(&tag_source, &tag_target, &track_key, &source_url)
            .map_err(|e| PipelineError::Tagging(e.to_string()))?;
        tagging::probe_quality(&tag_target, codec)
            .map_err(|e| PipelineError::InvalidMedia(format!("after tagging: {e}")))?;
        Ok::<_, PipelineError>((quality, artwork))
    })
    .await
    .map_err(|e| PipelineError::io("tagging task")(std::io::Error::other(e)))??;
    let quality = quality.converted_from_file(&source.quality);
    tracing::info!(job = job.id(), quality = %quality.summary, "converted and tagged");
    job.update(|s| {
        s.quality = Some(quality.clone());
        s.artwork_embedded = Some(artwork);
    });

    if cancel.is_cancelled() {
        return Err(PipelineError::Cancelled);
    }
    job.set_state(JobState::Organizing);
    Ok(job
        .ctx()
        .library
        .import_converted(
            target,
            source,
            quality,
            output,
            conversion.extension(),
            job.requesters(),
        )
        .await?)
}

/// Load (and if needed crop) the downloaded cover. Problems here are
/// reported as warnings; a missing cover never fails the download.
async fn prepare_artwork(
    job: &JobHandle,
    details: &TrackDetails,
    media: &DownloadedMedia,
) -> Option<(Vec<u8>, MimeType)> {
    let warn = |message: String| {
        tracing::warn!(job = job.id(), "{message}");
        job.update(|s| s.warnings.push(message));
    };
    let Some(original) = &media.artwork_path else {
        warn("No artwork was available from the provider".into());
        return None;
    };

    let mut path: PathBuf = original.clone();
    if details.artwork_needs_square_crop {
        let cropped = original.with_file_name("cover-square.jpg");
        match job.ctx().ffmpeg.crop_to_square(original, &cropped).await {
            Ok(()) => path = cropped,
            Err(e) => warn(format!(
                "Could not crop artwork, embedding it uncropped: {e}"
            )),
        }
    }

    let size = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if size == 0 || size > MAX_ARTWORK_BYTES {
        warn(format!("Artwork skipped: unexpected size ({size} bytes)"));
        return None;
    }
    let data = match tokio::fs::read(&path).await {
        Ok(data) => data,
        Err(e) => {
            warn(format!("Artwork skipped: {e}"));
            return None;
        }
    };
    match tagging::artwork_mime(&data) {
        Some(mime) => Some((data, mime)),
        None => {
            warn("Artwork skipped: not a JPEG or PNG image".into());
            None
        }
    }
}

/// Measure the real quality, write tags, and re-read the file to make sure
/// tagging left it intact.
async fn verify_and_tag(
    audio: &Path,
    details: &TrackDetails,
    codec: Codec,
    artwork: Option<(Vec<u8>, MimeType)>,
) -> Result<QualityReport, PipelineError> {
    let audio = audio.to_path_buf();
    let meta = details.metadata.clone();
    tokio::task::spawn_blocking(move || {
        let quality = tagging::probe_quality(&audio, codec)
            .map_err(|e| PipelineError::InvalidMedia(e.to_string()))?;
        tagging::write_tags(&audio, &meta, artwork)
            .map_err(|e| PipelineError::Tagging(e.to_string()))?;
        tagging::probe_quality(&audio, codec)
            .map_err(|e| PipelineError::InvalidMedia(format!("after tagging: {e}")))?;
        Ok(quality)
    })
    .await
    .map_err(|e| PipelineError::io("tagging task")(std::io::Error::other(e)))?
}

/// Run a provider operation, retrying transient failures with backoff.
async fn retrying<T, F, Fut>(
    policy: RetryPolicy,
    job: &JobHandle,
    cancel: &CancellationToken,
    mut operation: F,
) -> Result<T, PipelineError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut retries = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled);
        }
        match operation().await {
            Ok(value) => return Ok(value),
            Err(ProviderError::Cancelled) => return Err(PipelineError::Cancelled),
            Err(error) if error.is_retryable() && retries < policy.max_retries => {
                retries += 1;
                let delay = policy.delay(retries);
                tracing::warn!(
                    job = job.id(),
                    retry = retries,
                    max = policy.max_retries,
                    delay_secs = delay.as_secs(),
                    error = %error,
                    "retrying after transient failure"
                );
                job.update(|s| {
                    s.retries += 1;
                    s.note = Some(format!(
                        "Retry {retries}/{} in {}s: {error}",
                        policy.max_retries,
                        delay.as_secs()
                    ));
                });
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}
