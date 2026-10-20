//! Pure parser for the bundled `7z.exe` stdout stream.
//!
//! The real binary emits ordinary `\r\n`-terminated lines mixed with bare-`\r`
//! in-place progress records; the percent record arrives before its filename.
//! Everything here is cosmetic: success is decided by the exit code plus an
//! `Everything is Ok` line ([`is_success_stdout`]).

/// The literal marker `7z` prints to stdout once every operation succeeded.
const DONE_MARKER: &[u8] = b"Everything is Ok";

/// Cosmetic progress state for the 7z install step.
///
/// Purely informational; success/failure is decided elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SevenZipProgress {
    /// Latest percent value seen (0-100), if any.
    pub percent: Option<u32>,
    /// Latest file/directory being processed, if any.
    pub file: Option<String>,
}

/// One parsed record from the 7z stdout stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SevenZipEvent {
    /// A `<n>%` record (an optional trailing member index such as `0% 1` is ignored).
    Percent(u32),
    /// A `- <path>` record (may name a directory, e.g. `big\`).
    File(String),
    /// The `Everything is Ok` completion marker.
    Done,
    /// Any other line (headers, summary counters, ...).
    Other,
}

/// Split the raw `7z` stdout bytes into trimmed, non-empty lines.
///
/// Splits on every `\r` byte, treating both `\r\n` terminators and bare-`\r`
/// progress separators as boundaries; empty slices are discarded.
pub fn split_stdout(raw: &[u8]) -> Vec<&[u8]> {
    raw.split(|&b| b == b'\r')
        .map(trim_ascii)
        .filter(|line| !line.is_empty())
        .collect()
}

/// Parse a single stdout line into a [`SevenZipEvent`].
pub fn parse_line(line: &[u8]) -> SevenZipEvent {
    let line = trim_ascii(line);

    if line == DONE_MARKER {
        return SevenZipEvent::Done;
    }
    if let Some(path) = line.strip_prefix(b"- ") {
        return SevenZipEvent::File(String::from_utf8_lossy(path).into_owned());
    }
    if let Some(percent) = parse_percent(line) {
        return SevenZipEvent::Percent(percent);
    }
    SevenZipEvent::Other
}

/// `true` when the raw stdout contains the `Everything is Ok` completion marker.
pub fn is_success_stdout(raw: &[u8]) -> bool {
    raw.windows(DONE_MARKER.len()).any(|w| w == DONE_MARKER)
}

/// Parse a leading `<digits>%` value, ignoring any trailing member index.
fn parse_percent(line: &[u8]) -> Option<u32> {
    let end = line
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(line.len());
    if end == 0 || line.get(end) != Some(&b'%') {
        return None;
    }

    let mut value: u32 = 0;
    for &digit in &line[..end] {
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(digit - b'0'))?;
    }
    Some(value)
}

/// Trim leading and trailing ASCII whitespace from a byte slice.
fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event from the captured stream, in order (including `Other`).
    fn events(raw: &[u8]) -> Vec<SevenZipEvent> {
        split_stdout(raw).into_iter().map(parse_line).collect()
    }

    #[test]
    fn parses_captured_extract_stream_in_real_order() {
        let raw = include_bytes!("../../tests/fixtures/sevenzip_extract.stdout");

        // Drop the header/summary noise and check the meaningful records.
        let meaningful: Vec<SevenZipEvent> = events(raw)
            .into_iter()
            .filter(|event| !matches!(event, SevenZipEvent::Other))
            .collect();

        let expected = vec![
            SevenZipEvent::Percent(0),
            SevenZipEvent::File("big\\".to_string()),
            SevenZipEvent::Percent(0),
            SevenZipEvent::File("big\\sub\\".to_string()),
            SevenZipEvent::Percent(0),
            SevenZipEvent::File("big\\blob_1.bin".to_string()),
            SevenZipEvent::Percent(100),
            SevenZipEvent::File("big\\blob_2.bin".to_string()),
            SevenZipEvent::Percent(100),
            SevenZipEvent::File("big\\blob_3.bin".to_string()),
            SevenZipEvent::Percent(100),
            SevenZipEvent::File("big\\sub\\nested.bin".to_string()),
            SevenZipEvent::Percent(100),
            SevenZipEvent::File("readme.txt".to_string()),
            SevenZipEvent::Percent(100),
            SevenZipEvent::Done,
        ];
        assert_eq!(meaningful, expected);

        // The percent record genuinely precedes its filename, and the `- big\`
        // directory entry is present.
        let first_percent = meaningful
            .iter()
            .position(|e| matches!(e, SevenZipEvent::Percent(_)));
        let first_file = meaningful
            .iter()
            .position(|e| matches!(e, SevenZipEvent::File(_)));
        assert!(matches!((first_percent, first_file), (Some(p), Some(f)) if p < f));
        assert!(meaningful.contains(&SevenZipEvent::File("big\\".to_string())));
    }

    #[test]
    fn split_stdout_splits_on_cr_and_drops_empty_segments() {
        // `\r\n` terminators and bare `\r` separators both split; stray `\n` and
        // pure-space runs trim away to nothing.
        let parts = split_stdout(b"a\r\nb\rc\r\n\r\n   \rd\r");
        assert_eq!(parts, vec![&b"a"[..], &b"b"[..], &b"c"[..], &b"d"[..]]);
    }

    #[test]
    fn parse_line_recognizes_percent_file_and_done() {
        assert_eq!(parse_line(b"  0%"), SevenZipEvent::Percent(0));
        assert_eq!(parse_line(b"0% 1"), SevenZipEvent::Percent(0));
        assert_eq!(parse_line(b" 29% 1"), SevenZipEvent::Percent(29));
        assert_eq!(parse_line(b"100% 6"), SevenZipEvent::Percent(100));
        assert_eq!(
            parse_line(b"- big\\"),
            SevenZipEvent::File("big\\".to_string())
        );
        assert_eq!(
            parse_line(b"- big\\sub\\nested.bin"),
            SevenZipEvent::File("big\\sub\\nested.bin".to_string())
        );
        assert_eq!(parse_line(b"Everything is Ok"), SevenZipEvent::Done);
    }

    #[test]
    fn parse_line_leaves_other_lines_as_other() {
        assert_eq!(parse_line(b"0M Scan"), SevenZipEvent::Other);
        assert_eq!(parse_line(b"--"), SevenZipEvent::Other);
        assert_eq!(parse_line(b"Path = test.7z"), SevenZipEvent::Other);
        assert_eq!(parse_line(b"Folders: 2"), SevenZipEvent::Other);
        assert_eq!(parse_line(b""), SevenZipEvent::Other);
    }

    #[test]
    fn is_success_detects_captured_tail_and_rejects_truncated_capture() {
        let raw = include_bytes!("../../tests/fixtures/sevenzip_extract.stdout");
        assert!(is_success_stdout(raw));

        // A capture cut off before the marker is NOT a success.
        let marker = b"Everything is Ok";
        let pos = raw
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("captured fixture contains the success marker");
        assert!(!is_success_stdout(&raw[..pos]));

        // A summary-only tail without the marker is NOT a success.
        assert!(!is_success_stdout(b"Folders: 2\r\nFiles: 5\r\n"));
    }
}
