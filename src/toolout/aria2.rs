//! Pure parser for the stdout of the bundled `aria2c.exe`.
//!
//! aria2c writes progress in-place: each `--summary-interval` tick prints a
//! `[#<gid> <done>/<total>(<pct>%) CN:<n> DL:<rate> ETA:<eta>]` summary record,
//! followed by a `FILE: <path>` line. Results are mapped back to assets by
//! `path/URI`, never by GID (aria2c truncates it in its output).

use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// Number of hex characters in a derived aria2c GID.
const GID_LEN: usize = 16;

/// Lowercase hex alphabet used by [`to_lower_hex`].
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Cosmetic progress for one file, parsed from an aria2c summary line.
///
/// `file` is `None` until the caller pairs the summary with the following
/// `FILE:` line via [`parse_file_line`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aria2FileProgress {
    /// The GID token as aria2c printed it (truncated to 6 hex in its output).
    pub gid: String,
    /// Bytes downloaded so far, with aria2c's unit suffix (e.g. `16KiB`).
    pub done: String,
    /// Total size, with aria2c's unit suffix (e.g. `2.8MiB`).
    pub total: String,
    /// Progress percentage inside the parentheses, if present.
    pub pct: Option<u32>,
    /// Connection count (`CN:` field); `0` when absent.
    pub cn: u32,
    /// Current download rate (`DL:` field); empty when absent.
    pub dl: String,
    /// Estimated time remaining (`ETA:` field); empty when absent (e.g. done).
    pub eta: String,
    /// The file path from a following `FILE:` line, if associated by the caller.
    pub file: Option<PathBuf>,
}

/// One row of the `Download Results` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadResultRow {
    /// The GID column (aria2c truncates this to 6 hex; do NOT key on it).
    pub gid: String,
    /// The `stat` column, e.g. `OK` or `ERR`.
    pub status: String,
    /// The `path/URI` column — the key results are mapped back to assets by.
    pub path: String,
}

/// Parse a single aria2c progress-summary line, e.g.
/// `[#000000 16KiB/2.8MiB(0%) CN:1 DL:15KiB ETA:3m3s]`.
///
/// Returns `None` for any line that is not a `[#...]` summary record.
pub fn parse_summary_line(line: &[u8]) -> Option<Aria2FileProgress> {
    let line = trim_ascii(line);
    let inner = line.strip_prefix(b"[#")?.strip_suffix(b"]")?;
    let text = String::from_utf8_lossy(inner);

    let mut tokens = text.split_whitespace();
    let gid = tokens.next()?.to_string();

    // `<done>/<total>(<pct>%)` — pct is optional.
    let size = tokens.next()?;
    let (done, rest) = size.split_once('/')?;
    let (total, pct) = match rest.split_once('(') {
        Some((total, pct_part)) => {
            let digits = pct_part.strip_suffix("%)").unwrap_or(pct_part);
            (total.to_string(), digits.parse::<u32>().ok())
        }
        None => (rest.to_string(), None),
    };

    let mut cn = 0;
    let mut dl = String::new();
    let mut eta = String::new();
    for token in tokens {
        if let Some(value) = token.strip_prefix("CN:") {
            cn = value.parse::<u32>().unwrap_or(0);
        } else if let Some(value) = token.strip_prefix("DL:") {
            dl = value.to_string();
        } else if let Some(value) = token.strip_prefix("ETA:") {
            eta = value.to_string();
        }
    }

    Some(Aria2FileProgress {
        gid,
        done: done.to_string(),
        total,
        pct,
        cn,
        dl,
        eta,
        file: None,
    })
}

/// Parse a `FILE: <path>` line into the path it names.
pub fn parse_file_line(line: &[u8]) -> Option<PathBuf> {
    let line = trim_ascii(line);
    let rest = trim_ascii(line.strip_prefix(b"FILE:")?);
    if rest.is_empty() {
        return None;
    }
    Some(PathBuf::from(String::from_utf8_lossy(rest).into_owned()))
}

/// Remove ANSI escape sequences (the `\x1b[...m` SGR bytes aria2c emits even when
/// stdout is piped) from captured output.
pub fn strip_ansi(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == 0x1b && raw.get(i + 1) == Some(&b'[') {
            // CSI: parameter bytes (0x30..=0x3f) / intermediate bytes
            // (0x20..=0x2f), then a final byte (0x40..=0x7e).
            let mut j = i + 2;
            while j < raw.len() {
                let b = raw[j];
                if (0x40..=0x7e).contains(&b) {
                    j += 1;
                    break;
                }
                if (0x20..=0x3f).contains(&b) {
                    j += 1;
                    continue;
                }
                break;
            }
            i = j;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    out
}

/// Parse every row of the `Download Results` table in captured aria2c output.
///
/// Parsing starts at the `Download Results:` marker and stops at `Status
/// Legend:`; rows are keyed by `path/URI`, never by GID. Returns an empty vector
/// when there is no results table.
pub fn parse_results_table(raw: &[u8]) -> Vec<DownloadResultRow> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in raw.split(|&b| b == b'\n' || b == b'\r') {
        let line = trim_ascii(line);
        if line.is_empty() {
            continue;
        }
        if !in_table {
            if line == b"Download Results:" {
                in_table = true;
            }
            continue;
        }
        if line.starts_with(b"Status Legend") {
            break;
        }
        if line.starts_with(b"gid") {
            continue;
        }
        if line.iter().all(|b| matches!(b, b'=' | b'+' | b'-')) {
            continue;
        }
        if let Some(row) = parse_result_row(line) {
            rows.push(row);
        }
    }
    rows
}

/// Derive a stable aria2c GID from an asset id: the first 16 lowercase hex
/// characters of `SHA-256(asset_id)`.
pub fn gid_for(asset_id: &str) -> String {
    let digest = Sha256::digest(asset_id.as_bytes());
    let mut hex = to_lower_hex(&digest);
    hex.truncate(GID_LEN);
    hex
}

/// Parse one `gid|stat|avg speed|path/URI` results row.
fn parse_result_row(line: &[u8]) -> Option<DownloadResultRow> {
    let text = String::from_utf8_lossy(line);
    let mut parts = text.splitn(4, '|');
    let gid = parts.next()?.trim().to_string();
    let status = parts.next()?.trim().to_string();
    let _avg_speed = parts.next()?;
    let path = parts.next()?.trim().to_string();
    if gid.is_empty() || path.is_empty() {
        return None;
    }
    Some(DownloadResultRow { gid, status, path })
}

/// Lowercase hex-encode a byte slice.
fn to_lower_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        out.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
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

    /// Split a raw capture into trimmed lines on both `\n` and the bare `\r`
    /// in-place separators.
    fn lines(raw: &[u8]) -> Vec<&[u8]> {
        raw.split(|&b| b == b'\n' || b == b'\r')
            .map(trim_ascii)
            .filter(|line| !line.is_empty())
            .collect()
    }

    #[test]
    fn parses_summary_and_file_lines_from_capture() {
        let raw = include_bytes!("../../tests/fixtures/aria2_summary.stdout");

        let summaries: Vec<Aria2FileProgress> = lines(raw)
            .into_iter()
            .filter_map(parse_summary_line)
            .collect();
        assert!(summaries.len() >= 4, "expected several summary records");

        // Every record shares the truncated GID from the capture.
        assert!(summaries.iter().all(|s| s.gid == "000000"));

        // The first tick: 16KiB of 2.8MiB, 0%, one connection, 15KiB/s, ETA.
        let first = &summaries[0];
        assert_eq!(first.done, "16KiB");
        assert_eq!(first.total, "2.8MiB");
        assert_eq!(first.pct, Some(0));
        assert_eq!(first.cn, 1);
        assert_eq!(first.dl, "15KiB");
        assert_eq!(first.eta, "3m3s");

        // The final tick has no `ETA:` field (the download finished).
        let last = summaries
            .iter()
            .find(|s| s.pct == Some(83))
            .expect("the 83% tick is present in the capture");
        assert_eq!(last.done, "2.3MiB");
        assert_eq!(last.total, "2.8MiB");
        assert_eq!(last.dl, "602KiB");
        assert_eq!(last.eta, "");

        // The `FILE:` records name the downloaded file.
        let files: Vec<PathBuf> = lines(raw).into_iter().filter_map(parse_file_line).collect();
        assert_eq!(files.len(), 4);
        assert!(files.iter().all(|p| p == &PathBuf::from("dl_sum/blob.bin")));
    }

    #[test]
    fn parse_summary_line_parses_fields_and_rejects_others() {
        let parsed = parse_summary_line(b"[#deadbe 1.5MiB/2.8MiB(55%) CN:2 DL:533KiB ETA:2s]")
            .expect("a well-formed summary line parses");
        assert_eq!(parsed.gid, "deadbe");
        assert_eq!(parsed.done, "1.5MiB");
        assert_eq!(parsed.total, "2.8MiB");
        assert_eq!(parsed.pct, Some(55));
        assert_eq!(parsed.cn, 2);
        assert_eq!(parsed.dl, "533KiB");
        assert_eq!(parsed.eta, "2s");
        assert_eq!(parsed.file, None);

        // Missing ETA -> empty string, still Some.
        let no_eta = parse_summary_line(b"[#000000 2.3MiB/2.8MiB(83%) CN:1 DL:602KiB]")
            .expect("a summary line without ETA still parses");
        assert_eq!(no_eta.pct, Some(83));
        assert_eq!(no_eta.eta, "");

        // Leading/trailing whitespace is tolerated.
        assert!(parse_summary_line(b"  [#000000 0B/0B(0%) CN:0 DL:0B]  ").is_some());

        // Non-summary lines are rejected.
        assert_eq!(parse_summary_line(b"FILE: x"), None);
        assert_eq!(parse_summary_line(b"Download Results:"), None);
        assert_eq!(parse_summary_line(b""), None);
    }

    #[test]
    fn parse_file_line_recognizes_file_lines() {
        assert_eq!(
            parse_file_line(b"FILE: dl_sum/blob.bin"),
            Some(PathBuf::from("dl_sum/blob.bin"))
        );
        assert_eq!(
            parse_file_line(b"  FILE:  a/b/c.7z.001 "),
            Some(PathBuf::from("a/b/c.7z.001"))
        );
        assert_eq!(parse_file_line(b"FILE:"), None);
        assert_eq!(parse_file_line(b"FILE:    "), None);
        assert_eq!(parse_file_line(b"not a file line"), None);
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        // Direct: the acceptance-shaped `\x1b[1;31m` / `\x1b[0m` bytes go away.
        let out = strip_ansi(b"\x1b[1;31mERROR\x1b[0m");
        assert_eq!(out, b"ERROR".to_vec());

        // Text without ANSI passes through unchanged.
        assert_eq!(strip_ansi(b"plain text"), b"plain text".to_vec());

        // The captured error line loses its escapes but keeps the message.
        let raw = include_bytes!("../../tests/fixtures/aria2_error.stdout");
        assert!(raw.contains(&0x1b));
        let stripped = strip_ansi(raw);
        assert!(!stripped.contains(&0x1b));
        assert!(stripped.windows(5).any(|w| w == b"ERROR"));
        assert!(stripped.windows(18).any(|w| w == b"Resource not found"));
    }

    #[test]
    fn parse_results_table_reads_captured_rows() {
        // Success capture.
        let ok = parse_results_table(include_bytes!("../../tests/fixtures/aria2_results.stdout"));
        assert_eq!(
            ok,
            vec![DownloadResultRow {
                gid: "000000".to_string(),
                status: "OK".to_string(),
                path: "dl_ok/blob.bin".to_string(),
            }]
        );

        // Error capture (ANSI-stripped first).
        let raw = include_bytes!("../../tests/fixtures/aria2_error.stdout");
        let err = parse_results_table(&strip_ansi(raw));
        assert_eq!(
            err,
            vec![DownloadResultRow {
                gid: "000000".to_string(),
                status: "ERR".to_string(),
                path: "dl_err/missing.bin".to_string(),
            }]
        );

        // The summary capture ends with the same success table.
        let summary =
            parse_results_table(include_bytes!("../../tests/fixtures/aria2_summary.stdout"));
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].status, "OK");
        assert_eq!(summary[0].path, "dl_sum/blob.bin");
    }

    #[test]
    fn results_rows_are_distinguished_by_path_not_gid() {
        // Two rows with the SAME (truncated) GID but different paths must both be
        // returned and told apart by `path`.
        let raw = b"Download Results:\n\
                    gid   |stat|avg speed  |path/URI\n\
                    ======+====+===========+=======\n\
                    000000|OK  |    1MiB/s|a/one.bin\n\
                    000000|OK  |    1MiB/s|b/two.bin\n\
                    \n\
                    Status Legend:\n\
                    (OK):download completed.\n";
        let rows = parse_results_table(raw);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].gid, "000000");
        assert_eq!(rows[1].gid, "000000");
        assert_eq!(rows[0].path, "a/one.bin");
        assert_eq!(rows[1].path, "b/two.bin");

        // No results table -> empty.
        assert!(parse_results_table(b"no table here\n").is_empty());
    }

    #[test]
    fn gid_for_is_stable_and_sixteen_lowercase_hex() {
        let gid = gid_for("abc");
        // Ground truth: first 16 hex of SHA-256("abc").
        assert_eq!(gid, "ba7816bf8f01cfea");
        assert_eq!(gid.len(), 16);
        assert!(gid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert!(!gid.bytes().any(|b| b.is_ascii_uppercase()));

        // Stable, and distinct ids differ.
        assert_eq!(gid_for("abc"), gid);
        assert_ne!(
            gid_for("zona_multipart.7z.001"),
            gid_for("zona_multipart.7z.002")
        );
        assert_eq!(gid_for("zona_multipart.7z.001").len(), 16);
    }
}
