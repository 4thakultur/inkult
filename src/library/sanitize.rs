//! Turning untrusted metadata into safe path components.
//!
//! The rules are deliberately the intersection of what Linux, macOS and
//! Windows (incl. SMB/NAS shares) accept, so a library can be moved between
//! systems without renames.

/// Maximum bytes for one path component, leaving headroom below the common
/// 255-byte limit for extensions and " (2)" collision suffixes.
pub const MAX_COMPONENT_BYTES: usize = 180;

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Invisible characters that can disguise names (zero-width, bidi overrides).
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Make `raw` safe to use as a single file or directory name.
///
/// The result never contains path separators, never is `.`/`..`, never
/// starts with a dot, and is never empty (`fallback` is used instead).
pub fn sanitize_component(raw: &str, fallback: &str) -> String {
    let mapped: String = raw
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .map(|c| match c {
            '/' | '\\' | ':' | '|' => '-',
            '"' => '\'',
            '*' | '?' | '<' | '>' => '_',
            c => c,
        })
        .collect();
    let collapsed = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = truncate_to_bytes(&collapsed, MAX_COMPONENT_BYTES);
    // Leading dots hide files (and form `..`); trailing dots/spaces are
    // stripped silently by Windows, causing mismatched names.
    let trimmed = truncated
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);

    if trimmed.is_empty() {
        return if fallback.is_empty() {
            "_".to_owned()
        } else {
            sanitize_component(fallback, "_")
        };
    }
    let stem = trimmed.split('.').next().unwrap_or(trimmed).trim();
    if WINDOWS_RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
    {
        return format!("_{trimmed}");
    }
    trimmed.to_owned()
}

fn truncate_to_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutralizes_traversal_and_separators() {
        assert_eq!(sanitize_component("../../somewhere", "x"), "-..-somewhere");
        assert_eq!(sanitize_component("..", "fallback"), "fallback");
        assert_eq!(sanitize_component(".", "fallback"), "fallback");
        assert_eq!(sanitize_component("a/b\\c", "x"), "a-b-c");
        assert_eq!(sanitize_component("/etc/passwd", "x"), "-etc-passwd");
    }

    #[test]
    fn replaces_reserved_characters() {
        assert_eq!(
            sanitize_component("What? \"Yes\": <No> | *", "x"),
            "What_ 'Yes'- _No_ - _"
        );
    }

    #[test]
    fn strips_control_and_invisible_characters() {
        assert_eq!(
            sanitize_component("a\u{0000}b\nc\u{202E}d\u{200B}e", "x"),
            "abcde"
        );
    }

    #[test]
    fn handles_windows_quirks() {
        assert_eq!(sanitize_component("CON", "x"), "_CON");
        assert_eq!(sanitize_component("nul.txt", "x"), "_nul.txt");
        assert_eq!(sanitize_component("Console", "x"), "Console");
        assert_eq!(sanitize_component("Title. ", "x"), "Title");
        assert_eq!(sanitize_component(".hidden", "x"), "hidden");
    }

    #[test]
    fn falls_back_when_empty() {
        assert_eq!(
            sanitize_component("   ", "Unknown Artist"),
            "Unknown Artist"
        );
        assert_eq!(sanitize_component("\u{0007}", ""), "_");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        let long = "é".repeat(200);
        let out = sanitize_component(&long, "x");
        assert!(out.len() <= MAX_COMPONENT_BYTES);
        assert!(out.chars().all(|c| c == 'é'));
    }
}
