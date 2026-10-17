//! Single byte-range requests (RFC 9110 §14), as browsers send them when
//! seeking in `<audio>`. Multi-range requests are not supported; they are
//! answered with the whole file, which the RFC allows.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// `bytes=N-`
    From(u64),
    /// `bytes=N-M` (inclusive)
    FromTo(u64, u64),
    /// `bytes=-N`: the last N bytes.
    Suffix(u64),
}

impl ByteRange {
    /// Parse a `Range` header value; `None` means "ignore it and send the
    /// whole representation" (malformed, multi-range or another unit).
    pub fn parse(header: &str) -> Option<ByteRange> {
        let spec = header.trim().strip_prefix("bytes=")?.trim();
        if spec.contains(',') {
            return None;
        }
        let (start, end) = spec.split_once('-')?;
        let number = |s: &str| s.trim().parse::<u64>().ok();
        match (start.trim().is_empty(), end.trim().is_empty()) {
            (true, false) => number(end).map(ByteRange::Suffix),
            (false, true) => number(start).map(ByteRange::From),
            (false, false) => {
                let (start, end) = (number(start)?, number(end)?);
                (start <= end).then_some(ByteRange::FromTo(start, end))
            }
            (true, true) => None,
        }
    }

    /// Inclusive byte positions within a representation of `total` bytes,
    /// or `None` if the range cannot be satisfied (answer 416).
    pub fn resolve(self, total: u64) -> Option<(u64, u64)> {
        let last = total.checked_sub(1)?;
        match self {
            ByteRange::From(start) => (start <= last).then_some((start, last)),
            ByteRange::FromTo(start, end) => (start <= last).then_some((start, end.min(last))),
            ByteRange::Suffix(0) => None,
            ByteRange::Suffix(len) => Some((total.saturating_sub(len), last)),
        }
    }

    /// Canonical header value, for forwarding to an upstream server.
    pub fn header_value(self) -> String {
        match self {
            ByteRange::From(start) => format!("bytes={start}-"),
            ByteRange::FromTo(start, end) => format!("bytes={start}-{end}"),
            ByteRange::Suffix(len) => format!("bytes=-{len}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_ranges() {
        assert_eq!(ByteRange::parse("bytes=0-"), Some(ByteRange::From(0)));
        assert_eq!(
            ByteRange::parse("bytes=10-20"),
            Some(ByteRange::FromTo(10, 20))
        );
        assert_eq!(ByteRange::parse("bytes=-500"), Some(ByteRange::Suffix(500)));
        assert_eq!(ByteRange::parse("bytes=20-10"), None);
        assert_eq!(ByteRange::parse("bytes=0-1,5-6"), None);
        assert_eq!(ByteRange::parse("items=0-1"), None);
        assert_eq!(ByteRange::parse("bytes=-"), None);
        assert_eq!(ByteRange::parse("bytes=x-1"), None);
    }

    #[test]
    fn resolves_against_length() {
        assert_eq!(ByteRange::From(0).resolve(100), Some((0, 99)));
        assert_eq!(ByteRange::From(100).resolve(100), None);
        assert_eq!(ByteRange::FromTo(90, 1000).resolve(100), Some((90, 99)));
        assert_eq!(ByteRange::Suffix(10).resolve(100), Some((90, 99)));
        assert_eq!(ByteRange::Suffix(1000).resolve(100), Some((0, 99)));
        assert_eq!(ByteRange::Suffix(0).resolve(100), None);
        assert_eq!(ByteRange::From(0).resolve(0), None);
        assert_eq!(ByteRange::FromTo(5, 9).header_value(), "bytes=5-9");
    }
}
