//! Provider audio kept in memory while it is listened to.
//!
//! HLS renditions (all of SoundCloud) cannot be seeked with byte ranges, so
//! each seek used to start a new ffmpeg that fetched the playlist and
//! segments again from the CDN, while the waveform downloaded the whole
//! track a second time. A seek therefore depended on the network, the CDN
//! and, when ffmpeg failed for any reason, on yt-dlp resolving the track
//! again — and the waveform competed with it for bandwidth.
//!
//! A capture downloads the track once, re-wrapped as ADTS (the AAC frames
//! copied, never re-encoded), and indexes where every frame starts.
//! Playback from any position, and the waveform, read from that copy: a
//! seek into audio already captured is answered from memory at once, and a
//! seek ahead of the download waits for it rather than racing it.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::watch;

/// Largest capture kept in memory, about 40 minutes at 160 kbps. Longer
/// tracks are streamed from the CDN as before.
pub const MAX_CAPTURE_BYTES: usize = 48 * 1024 * 1024;

/// AAC sample rates by ADTS sampling frequency index.
const ADTS_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];
const ADTS_HEADER: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("the track is too long to keep in memory")]
    TooLarge,
    #[error("the captured audio is not valid ADTS")]
    Malformed,
    #[error("{0}")]
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Running,
    Done,
    Failed(CaptureError),
}

/// Where each ADTS frame starts, built as bytes arrive.
#[derive(Debug, Default)]
struct FrameIndex {
    /// Bytes up to here are split into frames.
    parsed: usize,
    sample_rate: Option<u32>,
    /// Byte offset and first sample of every complete frame.
    frames: Vec<(u64, u64)>,
    /// Samples in all complete frames.
    samples: u64,
}

impl FrameIndex {
    fn extend(&mut self, bytes: &[u8]) -> Result<(), CaptureError> {
        while let Some(header) = bytes.get(self.parsed..self.parsed + ADTS_HEADER) {
            if header[0] != 0xFF || header[1] & 0xF0 != 0xF0 {
                return Err(CaptureError::Malformed);
            }
            let rate = ADTS_RATES
                .get(usize::from((header[2] >> 2) & 0x0F))
                .copied()
                .ok_or(CaptureError::Malformed)?;
            let len = (usize::from(header[3] & 0x03) << 11)
                | (usize::from(header[4]) << 3)
                | usize::from(header[5] >> 5);
            if len < ADTS_HEADER || *self.sample_rate.get_or_insert(rate) != rate {
                return Err(CaptureError::Malformed);
            }
            if self.parsed + len > bytes.len() {
                break;
            }
            let blocks = u64::from(header[6] & 0x03) + 1;
            self.frames.push((self.parsed as u64, self.samples));
            self.samples += blocks * 1024;
            self.parsed += len;
        }
        Ok(())
    }

    /// Byte offset of the frame playing at `secs`, if it is indexed.
    fn offset_at(&self, secs: f64) -> Option<u64> {
        let rate = self.sample_rate?;
        let target = (secs.max(0.0) * f64::from(rate)) as u64;
        if target >= self.samples {
            return None;
        }
        let i = self.frames.partition_point(|&(_, first)| first <= target);
        Some(self.frames[i.saturating_sub(1)].0)
    }

    fn duration_secs(&self) -> Option<f64> {
        Some(self.samples as f64 / f64::from(self.sample_rate?))
    }
}

#[derive(Debug)]
struct Inner {
    bytes: Vec<u8>,
    index: FrameIndex,
    state: State,
}

/// A provider track's audio as ADTS, filled by one download.
pub struct Capture {
    inner: Mutex<Inner>,
    /// Bumped whenever bytes arrive or the capture ends.
    changed: watch::Sender<()>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("Capture")
            .field("bytes", &inner.bytes.len())
            .field("state", &inner.state)
            .finish()
    }
}

impl Capture {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                bytes: Vec::new(),
                index: FrameIndex::default(),
                state: State::Running,
            }),
            changed: watch::Sender::new(()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Critical sections never panic midway.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add downloaded bytes. An error ends the capture.
    pub fn append(&self, data: &[u8]) -> Result<(), CaptureError> {
        let result = {
            let mut inner = self.lock();
            if inner.state != State::Running {
                return Ok(());
            }
            if inner.bytes.len() + data.len() > MAX_CAPTURE_BYTES {
                Err(CaptureError::TooLarge)
            } else {
                inner.bytes.extend_from_slice(data);
                let Inner { bytes, index, .. } = &mut *inner;
                index.extend(bytes)
            }
        };
        match result {
            Ok(()) => {
                self.changed.send_replace(());
                Ok(())
            }
            Err(e) => {
                self.fail(e.clone());
                Err(e)
            }
        }
    }

    /// The download is complete.
    pub fn finish(&self) {
        let mut inner = self.lock();
        if inner.state == State::Running {
            // A trailing partial frame is not audio anyone can play.
            let parsed = inner.index.parsed;
            inner.bytes.truncate(parsed);
            inner.bytes.shrink_to_fit();
            inner.state = if inner.index.frames.is_empty() {
                State::Failed(CaptureError::Failed("no audio was downloaded".into()))
            } else {
                State::Done
            };
        }
        drop(inner);
        self.changed.send_replace(());
    }

    pub fn fail(&self, error: CaptureError) {
        let mut inner = self.lock();
        if inner.state == State::Running {
            inner.state = State::Failed(error);
        }
        drop(inner);
        self.changed.send_replace(());
    }

    /// The error the capture ended with, if it failed.
    #[cfg(test)]
    pub fn failure(&self) -> Option<CaptureError> {
        match &self.lock().state {
            State::Failed(e) => Some(e.clone()),
            _ => None,
        }
    }

    /// Length of the captured audio once complete.
    pub fn duration_secs(&self) -> Option<f64> {
        let inner = self.lock();
        match inner.state {
            State::Done => inner.index.duration_secs(),
            _ => None,
        }
    }

    /// Byte offset of the frame playing at `secs`, waiting up to `timeout`
    /// for the download to reach it. `Ok(None)`: the track is shorter.
    pub async fn offset_at(
        &self,
        secs: f64,
        timeout: Duration,
    ) -> Result<Option<u64>, CaptureError> {
        let mut changed = self.changed.subscribe();
        let wait = async {
            loop {
                changed.borrow_and_update();
                {
                    let inner = self.lock();
                    if let Some(offset) = inner.index.offset_at(secs) {
                        return Ok(Some(offset));
                    }
                    match &inner.state {
                        State::Running => {}
                        State::Done => return Ok(None),
                        State::Failed(e) => return Err(e.clone()),
                    }
                }
                if changed.changed().await.is_err() {
                    return Err(CaptureError::Failed("capture abandoned".into()));
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or_else(|_| Err(CaptureError::Failed("the download is too slow".into())))
    }

    /// Read the captured audio from byte `offset` (a frame start) on,
    /// following the download until it completes.
    pub fn reader(self: &Arc<Self>, offset: u64) -> CaptureReader {
        CaptureReader {
            changed: self.changed.subscribe(),
            capture: Arc::clone(self),
            pos: usize::try_from(offset).unwrap_or(usize::MAX),
            wait: None,
        }
    }
}

type Wait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Reads a [`Capture`]; reports a failed download as an I/O error rather
/// than as the end of the track.
pub struct CaptureReader {
    capture: Arc<Capture>,
    changed: watch::Receiver<()>,
    pos: usize,
    wait: Option<Wait>,
}

impl AsyncRead for CaptureReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if let Some(wait) = self.wait.as_mut() {
                if wait.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
                self.wait = None;
            }
            // Marked seen before looking, so bytes arriving after the look
            // wake the wait below.
            self.changed.borrow_and_update();
            {
                let inner = self.capture.lock();
                let available = inner.bytes.len().saturating_sub(self.pos);
                if available > 0 {
                    let n = available.min(buf.remaining());
                    buf.put_slice(&inner.bytes[self.pos..self.pos + n]);
                    drop(inner);
                    self.pos += n;
                    return Poll::Ready(Ok(()));
                }
                match &inner.state {
                    State::Running => {}
                    State::Done => return Poll::Ready(Ok(())),
                    State::Failed(e) => {
                        return Poll::Ready(Err(std::io::Error::other(e.to_string())));
                    }
                }
            }
            let mut changed = self.changed.clone();
            self.wait = Some(Box::pin(async move {
                // A closed channel means the capture is gone; the next look
                // sees its final state.
                let _ = changed.changed().await;
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// An ADTS frame of `len` bytes at 44.1 kHz holding one block.
    fn frame(len: usize, fill: u8) -> Vec<u8> {
        let mut f = vec![fill; len];
        f[0] = 0xFF;
        f[1] = 0xF1;
        f[2] = 0x50; // AAC LC, 44.1 kHz
        f[3] = 0x80 | ((len >> 11) & 0x03) as u8;
        f[4] = ((len >> 3) & 0xFF) as u8;
        f[5] = (((len & 0x07) << 5) | 0x1F) as u8;
        f[6] = 0xFC;
        f
    }

    fn frames(count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|i| frame(100 + i % 7, i as u8))
            .collect()
    }

    #[test]
    fn indexes_frames_split_across_appends() {
        let capture = Capture::new();
        let audio = frames(50);
        for chunk in audio.chunks(33) {
            capture.append(chunk).unwrap();
        }
        capture.finish();
        let inner = capture.lock();
        assert_eq!(inner.index.frames.len(), 50);
        assert_eq!(inner.index.samples, 50 * 1024);
        // Frame 10 starts after frames 0..10.
        let tenth: usize = (0..10).map(|i| 100 + i % 7).sum();
        let at = 10.0 * 1024.0 / 44100.0 + 0.001;
        assert_eq!(inner.index.offset_at(at), Some(tenth as u64));
        assert_eq!(inner.index.offset_at(0.0), Some(0));
        assert_eq!(inner.index.offset_at(60.0), None);
        drop(inner);
        assert!((capture.duration_secs().unwrap() - 50.0 * 1024.0 / 44100.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_garbage_and_oversized_audio() {
        let capture = Capture::new();
        assert_eq!(
            capture.append(b"<html>nope</html>"),
            Err(CaptureError::Malformed)
        );
        assert_eq!(capture.failure(), Some(CaptureError::Malformed));

        let capture = Capture::new();
        let big = vec![0u8; MAX_CAPTURE_BYTES + 1];
        assert_eq!(capture.append(&big), Err(CaptureError::TooLarge));
    }

    #[tokio::test]
    async fn readers_follow_the_download_from_a_seek_point() {
        let capture = Capture::new();
        let audio = frames(40);
        let (head, tail) = audio.split_at(audio.len() / 2);
        capture.append(head).unwrap();

        // A seek past what has arrived waits for it.
        let late = 30.0 * 1024.0 / 44100.0 + 0.001;
        let waiting = {
            let capture = Arc::clone(&capture);
            tokio::spawn(async move { capture.offset_at(late, Duration::from_secs(5)).await })
        };
        let mut reader = capture.reader(0);
        let mut first = vec![0u8; head.len()];
        reader.read_exact(&mut first).await.unwrap();
        assert_eq!(first, head);

        capture.append(tail).unwrap();
        capture.finish();
        let offset = waiting.await.unwrap().unwrap().unwrap();
        let thirtieth: usize = (0..30).map(|i| 100 + i % 7).sum();
        assert_eq!(offset, thirtieth as u64);

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, tail);
        let mut from_seek = Vec::new();
        capture
            .reader(offset)
            .read_to_end(&mut from_seek)
            .await
            .unwrap();
        assert_eq!(from_seek, audio[thirtieth..]);
        assert_eq!(
            capture.offset_at(late * 2.0, Duration::from_secs(1)).await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn readers_see_a_failed_download_as_an_error() {
        let capture = Capture::new();
        capture.append(&frames(3)).unwrap();
        let mut reader = capture.reader(0);
        let mut buf = Vec::new();
        let read = {
            let capture = Arc::clone(&capture);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                capture.fail(CaptureError::Failed("CDN went away".into()));
            });
            reader.read_to_end(&mut buf).await
        };
        assert_eq!(read.unwrap_err().to_string(), "CDN went away");
        assert_eq!(
            capture.offset_at(100.0, Duration::from_secs(1)).await,
            Err(CaptureError::Failed("CDN went away".into()))
        );
    }
}
