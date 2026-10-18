//! Several library files as one ZIP download.
//!
//! Entries are stored, not compressed: audio is already compressed, so
//! deflate would only cost CPU. File sizes are known before anything is
//! sent, which makes the archive's exact length known too (the browser can
//! show progress). Each file's CRC-32 is computed in a read pass just before
//! the file is sent, so no data descriptors are needed; some unzip tools
//! handle those poorly together with ZIP64. ZIP64 records are written only
//! where a size or offset does not fit in 32 bits.
//!
//! A file that changes while it is being sent (other length or checksum)
//! stops the archive with an error rather than producing a ZIP whose
//! checksum lies about its content.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::auth::UserId;
use crate::auth::password::random_id;
use crate::domain::metadata::ReleaseDate;

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIR: u32 = 0x0605_4b50;
const ZIP64_END_OF_CENTRAL_DIR: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_EXTRA: u16 = 0x0001;
/// Bit 11: names are UTF-8.
const FLAG_UTF8: u16 = 0x0800;
const VERSION_PLAIN: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// "Made by" Unix, so the external attributes below are read as a mode.
const MADE_BY_UNIX: u16 = 3 << 8;
const FILE_MODE: u32 = 0o100_644;
const MAX_32: u64 = 0xFFFF_FFFF;
const MAX_16: u64 = 0xFFFF;
const BUFFER: usize = 256 * 1024;

/// One file in the archive.
#[derive(Debug, Clone)]
pub struct ArchiveFile {
    /// Name inside the archive (`/`-separated, relative).
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// The archive name of a library-relative path: its folders kept, so the
/// download unpacks into the same Artist/Album layout. `None` for anything
/// that is not a plain relative path.
pub fn entry_name(relative: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = relative
        .components()
        .map(|c| match c {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    let parts = parts.filter(|p| !p.is_empty())?;
    Some(parts.join("/"))
}

/// Make every name unique (case-insensitively, for case-insensitive
/// filesystems) by numbering repeats: `Track.flac`, `Track (2).flac`.
pub fn dedupe_names(files: &mut [ArchiveFile]) {
    let mut seen = std::collections::HashSet::new();
    for file in files {
        let (stem, ext) = match file.name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && !ext.contains('/') => {
                (stem.to_owned(), format!(".{ext}"))
            }
            _ => (file.name.clone(), String::new()),
        };
        let mut name = file.name.clone();
        let mut n = 2;
        while !seen.insert(name.to_lowercase()) {
            name = format!("{stem} ({n}){ext}");
            n += 1;
        }
        file.name = name;
    }
}

/// Exact number of bytes [`write_archive`] produces for `files`.
pub fn archive_len(files: &[ArchiveFile]) -> u64 {
    let mut offset = 0u64;
    let mut central = Vec::with_capacity(files.len());
    for file in files {
        central.push((file, offset));
        offset += local_header(file, 0).len() as u64 + file.size;
    }
    let directory: u64 = central
        .iter()
        .map(|(file, at)| central_header(file, 0, *at).len() as u64)
        .sum();
    offset + directory + end_records(files.len(), directory, offset).len() as u64
}

/// Write `files` as a ZIP archive to `out`.
pub async fn write_archive<W: AsyncWrite + Unpin>(
    files: &[ArchiveFile],
    out: &mut W,
) -> io::Result<()> {
    let mut buf = vec![0u8; BUFFER];
    let mut offset = 0u64;
    let mut central = Vec::with_capacity(files.len());
    for file in files {
        let crc = checksum(file, &mut buf).await?;
        let header = local_header(file, crc);
        out.write_all(&header).await?;
        copy_verified(file, crc, &mut buf, out).await?;
        central.push((file, crc, offset));
        offset += header.len() as u64 + file.size;
    }
    let mut directory = 0u64;
    for (file, crc, at) in central {
        let header = central_header(file, crc, at);
        out.write_all(&header).await?;
        directory += header.len() as u64;
    }
    out.write_all(&end_records(files.len(), directory, offset))
        .await?;
    out.flush().await
}

/// The file's CRC-32, checking it still has the size it was listed with.
async fn checksum(file: &ArchiveFile, buf: &mut [u8]) -> io::Result<u32> {
    let mut reader = tokio::fs::File::open(&file.path).await?;
    let mut hasher = crc32fast::Hasher::new();
    let mut read = 0u64;
    loop {
        let n = reader.read(buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
    }
    if read != file.size {
        return Err(changed(file));
    }
    Ok(hasher.finalize())
}

/// Send the file, checking it is still the one that was checksummed.
async fn copy_verified<W: AsyncWrite + Unpin>(
    file: &ArchiveFile,
    crc: u32,
    buf: &mut [u8],
    out: &mut W,
) -> io::Result<()> {
    let mut reader = tokio::fs::File::open(&file.path).await?;
    let mut hasher = crc32fast::Hasher::new();
    let mut left = file.size;
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = reader.read(&mut buf[..want]).await?;
        if n == 0 {
            return Err(changed(file));
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).await?;
        left -= n as u64;
    }
    if hasher.finalize() != crc || reader.read(&mut buf[..1]).await? != 0 {
        return Err(changed(file));
    }
    Ok(())
}

fn changed(file: &ArchiveFile) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} changed while it was being archived", file.name),
    )
}

fn local_header(file: &ArchiveFile, crc: u32) -> Vec<u8> {
    let zip64 = file.size >= MAX_32;
    let (time, date) = dos_time(file.modified);
    let mut h = Vec::with_capacity(30 + file.name.len() + 20);
    put32(&mut h, LOCAL_HEADER);
    put16(&mut h, if zip64 { VERSION_ZIP64 } else { VERSION_PLAIN });
    put16(&mut h, FLAG_UTF8);
    put16(&mut h, 0); // stored
    put16(&mut h, time);
    put16(&mut h, date);
    put32(&mut h, crc);
    let size = clamp32(file.size);
    put32(&mut h, size); // compressed
    put32(&mut h, size);
    put16(&mut h, name_len(file));
    put16(&mut h, if zip64 { 20 } else { 0 });
    h.extend_from_slice(file.name.as_bytes());
    if zip64 {
        // In a local header the ZIP64 field holds both sizes.
        put16(&mut h, ZIP64_EXTRA);
        put16(&mut h, 16);
        put64(&mut h, file.size);
        put64(&mut h, file.size);
    }
    h
}

fn central_header(file: &ArchiveFile, crc: u32, offset: u64) -> Vec<u8> {
    // Only the fields that overflow go in the ZIP64 extra, in this order.
    let mut extra = Vec::new();
    if file.size >= MAX_32 {
        put64(&mut extra, file.size);
        put64(&mut extra, file.size);
    }
    if offset >= MAX_32 {
        put64(&mut extra, offset);
    }
    let version = if extra.is_empty() {
        VERSION_PLAIN
    } else {
        VERSION_ZIP64
    };
    let (time, date) = dos_time(file.modified);
    let mut h = Vec::with_capacity(46 + file.name.len() + 28);
    put32(&mut h, CENTRAL_HEADER);
    put16(&mut h, MADE_BY_UNIX | version);
    put16(&mut h, version);
    put16(&mut h, FLAG_UTF8);
    put16(&mut h, 0);
    put16(&mut h, time);
    put16(&mut h, date);
    put32(&mut h, crc);
    put32(&mut h, clamp32(file.size));
    put32(&mut h, clamp32(file.size));
    put16(&mut h, name_len(file));
    put16(
        &mut h,
        if extra.is_empty() {
            0
        } else {
            extra.len() as u16 + 4
        },
    );
    put16(&mut h, 0); // comment
    put16(&mut h, 0); // disk
    put16(&mut h, 0); // internal attributes
    put32(&mut h, FILE_MODE << 16);
    put32(&mut h, clamp32(offset));
    h.extend_from_slice(file.name.as_bytes());
    if !extra.is_empty() {
        put16(&mut h, ZIP64_EXTRA);
        put16(&mut h, extra.len() as u16);
        h.extend_from_slice(&extra);
    }
    h
}

/// The end of central directory record, preceded by its ZIP64 form and
/// locator when the entry count, directory size or offset overflow.
fn end_records(entries: usize, directory: u64, directory_at: u64) -> Vec<u8> {
    let count = entries as u64;
    let mut h = Vec::with_capacity(98);
    if count >= MAX_16 || directory >= MAX_32 || directory_at >= MAX_32 {
        let record_at = directory_at + directory;
        put32(&mut h, ZIP64_END_OF_CENTRAL_DIR);
        put64(&mut h, 44); // size of the rest of this record
        put16(&mut h, MADE_BY_UNIX | VERSION_ZIP64);
        put16(&mut h, VERSION_ZIP64);
        put32(&mut h, 0);
        put32(&mut h, 0);
        put64(&mut h, count);
        put64(&mut h, count);
        put64(&mut h, directory);
        put64(&mut h, directory_at);
        put32(&mut h, ZIP64_LOCATOR);
        put32(&mut h, 0);
        put64(&mut h, record_at);
        put32(&mut h, 1);
    }
    let count16 = count.min(MAX_16) as u16;
    put32(&mut h, END_OF_CENTRAL_DIR);
    put16(&mut h, 0);
    put16(&mut h, 0);
    put16(&mut h, count16);
    put16(&mut h, count16);
    put32(&mut h, clamp32(directory));
    put32(&mut h, clamp32(directory_at));
    put16(&mut h, 0); // comment
    h
}

/// Names come from the library index, whose sanitized file names are far
/// below this; a longer one is cut at a character boundary.
const MAX_NAME_BYTES: usize = u16::MAX as usize;

fn name_len(file: &ArchiveFile) -> u16 {
    debug_assert!(file.name.len() <= MAX_NAME_BYTES);
    file.name.len().min(MAX_NAME_BYTES) as u16
}

/// Cut a name to what a ZIP header can hold.
pub fn fit_name(mut name: String) -> String {
    if name.len() > MAX_NAME_BYTES {
        let mut end = MAX_NAME_BYTES;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    name
}

/// MS-DOS time and date (UTC; the format has no zone). DOS dates start in
/// 1980, so anything earlier or unknown is written as 1980-01-01.
fn dos_time(modified: Option<SystemTime>) -> (u16, u16) {
    const EPOCH: (u16, u16) = (0, (1 << 5) | 1);
    let Some(secs) = modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
    else {
        return EPOCH;
    };
    let Some(date) = ReleaseDate::from_unix_timestamp(secs) else {
        return EPOCH;
    };
    let (Some(month), Some(day)) = (date.month, date.day) else {
        return EPOCH;
    };
    if !(1980..=2107).contains(&date.year) {
        return EPOCH;
    }
    let of_day = secs.rem_euclid(86_400) as u16;
    let time = ((of_day / 3600) << 11) | (((of_day / 60) % 60) << 5) | ((of_day % 60) / 2);
    let date = ((date.year - 1980) << 9) | (u16::from(month) << 5) | u16::from(day);
    (time, date)
}

fn clamp32(n: u64) -> u32 {
    n.min(MAX_32) as u32
}

fn put16(h: &mut Vec<u8>, v: u16) {
    h.extend_from_slice(&v.to_le_bytes());
}

fn put32(h: &mut Vec<u8>, v: u32) {
    h.extend_from_slice(&v.to_le_bytes());
}

fn put64(h: &mut Vec<u8>, v: u64) {
    h.extend_from_slice(&v.to_le_bytes());
}

/// How long a prepared archive can be fetched. The browser fetches it
/// right away; the margin is for download managers that retry.
const TICKET_TTL: Duration = Duration::from_secs(10 * 60);
/// Prepared archives kept per user; preparing another drops the oldest.
const TICKETS_PER_USER: usize = 4;

/// An archive whose content was checked when it was asked for, waiting for
/// the browser to fetch it with a plain link (so it is saved to disk as it
/// arrives instead of being held in page memory).
#[derive(Debug, Clone)]
pub struct PreparedArchive {
    pub owner: UserId,
    pub file_name: String,
    pub files: Vec<ArchiveFile>,
    created: Instant,
}

/// Prepared archives by unguessable id. In memory: a restart only means
/// asking again.
#[derive(Debug, Default)]
pub struct PreparedArchives {
    tickets: Mutex<HashMap<String, PreparedArchive>>,
}

impl PreparedArchives {
    pub fn insert(&self, owner: UserId, file_name: String, files: Vec<ArchiveFile>) -> String {
        let id = format!("{:016x}{:016x}", random_id(), random_id());
        let mut tickets = self.lock();
        tickets.retain(|_, t| t.created.elapsed() < TICKET_TTL);
        let mut mine: Vec<(String, Instant)> = tickets
            .iter()
            .filter(|(_, t)| t.owner == owner)
            .map(|(id, t)| (id.clone(), t.created))
            .collect();
        mine.sort_by_key(|(_, created)| *created);
        let excess = (mine.len() + 1).saturating_sub(TICKETS_PER_USER);
        for (old, _) in mine.into_iter().take(excess) {
            tickets.remove(&old);
        }
        tickets.insert(
            id.clone(),
            PreparedArchive {
                owner,
                file_name,
                files,
                created: Instant::now(),
            },
        );
        id
    }

    /// The archive `id`, if `user` prepared it and it has not expired.
    pub fn get(&self, id: &str, user: UserId) -> Option<PreparedArchive> {
        self.lock()
            .get(id)
            .filter(|t| t.owner == user && t.created.elapsed() < TICKET_TTL)
            .cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PreparedArchive>> {
        // The map holds no invariant a panicking holder could break.
        self.tickets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path, name: &str, body: &[u8]) -> ArchiveFile {
        let path = dir.join(name.replace('/', "_"));
        std::fs::write(&path, body).unwrap();
        ArchiveFile {
            name: name.to_owned(),
            path,
            size: body.len() as u64,
            modified: Some(UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000)),
        }
    }

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// Read the archive back through its central directory, as unzip
    /// tools do: (name, content) for every entry, checksums verified.
    fn read_back(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let end = zip.len() - 22;
        assert_eq!(u32_at(zip, end), END_OF_CENTRAL_DIR);
        let count = usize::from(u16_at(zip, end + 10));
        let mut at = u32_at(zip, end + 16) as usize;
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(zip, at), CENTRAL_HEADER);
            let crc = u32_at(zip, at + 16);
            let size = u32_at(zip, at + 24) as usize;
            let name_len = usize::from(u16_at(zip, at + 28));
            let extra_len = usize::from(u16_at(zip, at + 30));
            let local = u32_at(zip, at + 42) as usize;
            let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
            assert_eq!(u32_at(zip, local), LOCAL_HEADER);
            assert_eq!(u32_at(zip, local + 14), crc);
            let data_at = local
                + 30
                + usize::from(u16_at(zip, local + 26))
                + usize::from(u16_at(zip, local + 28));
            let data = zip[data_at..data_at + size].to_vec();
            assert_eq!(crc32fast::hash(&data), crc, "checksum of {name}");
            out.push((name, data));
            at += 46 + name_len + extra_len;
        }
        out
    }

    #[tokio::test]
    async fn archive_holds_every_file_and_has_the_predicted_length() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            file(dir.path(), "Artist/Album/01 - Ünïcode.flac", b"first file"),
            file(dir.path(), "Artist/Singles/Empty.mp3", b""),
            file(dir.path(), "Other/Album/02 - B.aiff", &[7u8; 300_000]),
        ];
        let mut zip = Vec::new();
        write_archive(&files, &mut zip).await.unwrap();
        assert_eq!(zip.len() as u64, archive_len(&files));
        let back = read_back(&zip);
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].0, "Artist/Album/01 - Ünïcode.flac");
        assert_eq!(back[0].1, b"first file");
        assert!(back[1].1.is_empty());
        assert_eq!(back[2].1, vec![7u8; 300_000]);
    }

    #[tokio::test]
    async fn empty_archive_is_just_the_end_record() {
        let mut zip = Vec::new();
        write_archive(&[], &mut zip).await.unwrap();
        assert_eq!(zip.len(), 22);
        assert_eq!(archive_len(&[]), 22);
    }

    #[tokio::test]
    async fn a_file_that_changed_size_stops_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let mut grown = file(dir.path(), "a.flac", b"12345");
        grown.size = 3;
        let mut zip = Vec::new();
        let err = write_archive(&[grown], &mut zip).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn large_files_and_offsets_get_zip64_fields() {
        let big = ArchiveFile {
            name: "big.wav".into(),
            path: PathBuf::new(),
            size: 5 << 30,
            modified: None,
        };
        let local = local_header(&big, 0);
        assert_eq!(u32_at(&local, 18), u32::MAX);
        assert_eq!(u16_at(&local, 28), 20);
        let central = central_header(&big, 0, 6 << 30);
        assert_eq!(u32_at(&central, 42), u32::MAX);
        // sizes + offset
        assert_eq!(u16_at(&central, 30), 4 + 24);
        let end = end_records(2, 100, 6 << 30);
        assert_eq!(u32_at(&end, 0), ZIP64_END_OF_CENTRAL_DIR);
        assert_eq!(end.len(), 56 + 20 + 22);
        let small = ArchiveFile { size: 10, ..big };
        assert_eq!(central_header(&small, 0, 0).len(), 46 + "big.wav".len());
        assert_eq!(end_records(1, 10, 10).len(), 22);
    }

    #[test]
    fn entry_names_keep_folders_and_refuse_escapes() {
        assert_eq!(
            entry_name(Path::new("Artist/Album/01 - T.flac")).as_deref(),
            Some("Artist/Album/01 - T.flac")
        );
        assert_eq!(entry_name(Path::new("../x.flac")), None);
        assert_eq!(entry_name(Path::new("/etc/passwd")), None);
        assert_eq!(entry_name(Path::new("")), None);
    }

    #[test]
    fn repeated_names_are_numbered() {
        let named = |name: &str| ArchiveFile {
            name: name.into(),
            path: PathBuf::new(),
            size: 0,
            modified: None,
        };
        let mut files = vec![
            named("A/T.flac"),
            named("A/t.flac"),
            named("A/T.flac"),
            named("A/README"),
        ];
        dedupe_names(&mut files);
        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["A/T.flac", "A/t (2).flac", "A/T (3).flac", "A/README"]
        );
    }

    #[test]
    fn prepared_archives_belong_to_their_user_and_are_capped() {
        let archives = PreparedArchives::default();
        let ids: Vec<String> = (0..TICKETS_PER_USER + 1)
            .map(|_| archives.insert(UserId(1), "a.zip".into(), Vec::new()))
            .collect();
        assert!(archives.get(&ids[0], UserId(1)).is_none(), "oldest dropped");
        assert!(archives.get(&ids[1], UserId(1)).is_some());
        assert!(archives.get(&ids[1], UserId(2)).is_none());
        assert!(archives.get("nope", UserId(1)).is_none());
    }

    #[test]
    fn dos_time_encodes_utc_and_clamps_old_dates() {
        // 2025-10-09 08:53:20 UTC
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000);
        let (time, date) = dos_time(Some(t));
        assert_eq!(date, ((2025 - 1980) << 9) | (10 << 5) | 9);
        assert_eq!(time, (8 << 11) | (53 << 5) | 10);
        assert_eq!(dos_time(Some(UNIX_EPOCH)), (0, 33));
        assert_eq!(dos_time(None), (0, 33));
    }
}
