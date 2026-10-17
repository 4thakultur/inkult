//! Soulseek wire format: little-endian integers, length-prefixed strings,
//! and length-prefixed frames.
//!
//! Everything read here comes from the network and is untrusted: every read
//! is bounds-checked, frames have a size cap chosen per message type, and
//! compressed payloads are inflated with an output limit so a small "zip
//! bomb" cannot exhaust memory.

use std::io::{self, Read, Write};

use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("message ended early")]
    Truncated,
    #[error("message of {len} bytes exceeds the limit of {max} bytes")]
    TooLarge { len: usize, max: usize },
    #[error("bad compressed data: {0}")]
    Decompress(String),
    #[error("{0}")]
    Invalid(String),
    #[error("connection error: {0}")]
    Io(#[from] io::Error),
}

/// Builds a message payload.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }

    /// A length-prefixed byte string. Lengths beyond `u32` cannot occur for
    /// anything Inkult sends (all inputs are bounded far below that).
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(u32::try_from(v.len()).unwrap_or(u32::MAX));
        self.buf.extend_from_slice(v);
        self
    }

    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// Wrap the payload as a frame with a 32-bit message code.
    pub fn frame_u32(code: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Writer::new();
        out.u32(u32::try_from(payload.len() + 4).unwrap_or(u32::MAX))
            .u32(code)
            .raw(payload);
        out.into_inner()
    }

    /// Wrap the payload as a frame with an 8-bit message code (peer init).
    pub fn frame_u8(code: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Writer::new();
        out.u32(u32::try_from(payload.len() + 1).unwrap_or(u32::MAX))
            .u8(code)
            .raw(payload);
        out.into_inner()
    }
}

/// Reads a message payload.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.remaining() < n {
            return Err(WireError::Truncated);
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, WireError> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(u64::from_le_bytes(arr))
    }

    pub fn bool(&mut self) -> Result<bool, WireError> {
        Ok(self.u8()? != 0)
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub fn string(&mut self) -> Result<String, WireError> {
        Ok(decode_text(self.bytes()?))
    }

    /// An item count, rejected if even `min_item_bytes` per item could not
    /// fit in what is left, so a forged count cannot make us loop (or
    /// pre-allocate) far beyond the actual message.
    pub fn count(&mut self, min_item_bytes: usize) -> Result<usize, WireError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item_bytes.max(1)) > self.remaining() {
            return Err(WireError::Invalid(format!(
                "list claims {n} entries but only {} bytes remain",
                self.remaining()
            )));
        }
        Ok(n)
    }
}

/// Soulseek has no declared text encoding. Modern clients send UTF-8; old
/// ones sent Latin-1, which is decoded byte-for-character as a fallback so
/// such names stay readable instead of turning into replacement characters.
pub fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

/// MD5 hex digest, as the login message requires. MD5 is the protocol's
/// choice, not a security measure.
pub fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    Md5::digest(input.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing into a Vec cannot fail.
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

/// Inflate at most `max` bytes; larger output is an error, not truncation.
pub fn zlib_decompress(data: &[u8], max: usize) -> Result<Vec<u8>, WireError> {
    let decoder = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| WireError::Decompress(e.to_string()))?;
    if out.len() > max {
        return Err(WireError::TooLarge {
            len: out.len(),
            max,
        });
    }
    Ok(out)
}

/// Read one frame: `u32` length, then that many bytes (returned).
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Vec<u8>, WireError> {
    let len = reader.read_u32_le().await? as usize;
    read_body(reader, len, max).await
}

/// Read a frame whose 32-bit code decides the size limit: returns the code
/// and the payload after it.
pub async fn read_coded_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit_for: impl Fn(u32) -> usize,
) -> Result<(u32, Vec<u8>), WireError> {
    let len = reader.read_u32_le().await? as usize;
    if len < 4 {
        return Err(WireError::Invalid(format!("frame of {len} bytes")));
    }
    let code = reader.read_u32_le().await?;
    let body = read_body(reader, len - 4, limit_for(code)).await?;
    Ok((code, body))
}

async fn read_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: usize,
    max: usize,
) -> Result<Vec<u8>, WireError> {
    if len > max {
        return Err(WireError::TooLarge { len, max });
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_primitives() {
        let mut w = Writer::new();
        w.u8(7)
            .u32(0xDEAD_BEEF)
            .u64(1 << 40)
            .bool(true)
            .str("héllo");
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u64().unwrap(), 1 << 40);
        assert!(r.bool().unwrap());
        assert_eq!(r.string().unwrap(), "héllo");
        assert_eq!(r.remaining(), 0);
        assert!(matches!(r.u8(), Err(WireError::Truncated)));
    }

    #[test]
    fn rejects_forged_lengths_and_counts() {
        let mut w = Writer::new();
        w.u32(1000).raw(b"abc");
        let bytes = w.into_inner();
        assert!(matches!(
            Reader::new(&bytes).string(),
            Err(WireError::Truncated)
        ));
        let mut w = Writer::new();
        w.u32(u32::MAX);
        let bytes = w.into_inner();
        assert!(Reader::new(&bytes).count(1).is_err());
    }

    #[test]
    fn decodes_latin1_fallback() {
        assert_eq!(decode_text(b"caf\xe9"), "café");
        assert_eq!(decode_text("café".as_bytes()), "café");
    }

    #[test]
    fn md5_matches_protocol_example() {
        // From the protocol documentation's login example.
        assert_eq!(
            md5_hex("usernamepassword"),
            "d51c9a7e9353746a6020f9602d452929"
        );
    }

    #[test]
    fn zlib_round_trip_and_bomb_limit() {
        let data = vec![b'x'; 100_000];
        let packed = zlib_compress(&data);
        assert!(packed.len() < 1000);
        assert_eq!(zlib_decompress(&packed, 100_000).unwrap(), data);
        assert!(matches!(
            zlib_decompress(&packed, 99_999),
            Err(WireError::TooLarge { .. })
        ));
        assert!(zlib_decompress(b"not zlib", 100).is_err());
    }

    #[tokio::test]
    async fn frames_enforce_limits() {
        let frame = Writer::frame_u32(5, b"payload");
        let (code, body) = read_coded_frame(&mut frame.as_slice(), |_| 100)
            .await
            .unwrap();
        assert_eq!((code, body.as_slice()), (5, &b"payload"[..]));
        assert!(matches!(
            read_coded_frame(&mut frame.as_slice(), |_| 3).await,
            Err(WireError::TooLarge { .. })
        ));
    }
}
