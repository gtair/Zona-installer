//! SHA-256 hashing of downloaded assets.
//!
//! Files are hashed here independently of aria2c's checksum pass, and compared
//! case-insensitively (config values are mixed case). Files are streamed in
//! [`READ_CHUNK`]-sized chunks so they never need to fit in memory.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

/// Size of each streaming read, in bytes.
const READ_CHUNK: usize = 64 * 1024;

/// Streams `path` through SHA-256 and returns the digest as lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|source| AppError::Io {
        context: "open file for SHA-256".to_string(),
        path: Some(path.to_path_buf()),
        source,
    })?;

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        let read = file.read(&mut buffer).map_err(|source| AppError::Io {
            context: "read file for SHA-256".to_string(),
            path: Some(path.to_path_buf()),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let digest = hasher.finalize();
    Ok(to_lower_hex(digest.as_slice()))
}

/// Case-insensitive comparison of two hex strings.
pub fn hex_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn to_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Known ground-truth digests (fixed inputs whose exact hex can be asserted
    /// without re-hashing).
    const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const SHA256_HELLO_WORLD: &str =
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory name.
    fn unique_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "zona_installer_hashing_{}_{}_{}",
            std::process::id(),
            nanos,
            n
        ))
    }

    #[test]
    fn hashes_known_content_to_lowercase_hex() {
        let dir = unique_temp_dir();
        assert!(fs::create_dir_all(&dir).is_ok(), "create temp dir");
        let file = dir.join("payload.bin");
        assert!(
            fs::write(&file, b"hello world").is_ok(),
            "write temp payload"
        );

        let digest = sha256_file(&file).expect("hash temp payload");

        assert_eq!(digest, SHA256_HELLO_WORLD);
        assert_eq!(digest.len(), 64, "SHA-256 hex is 64 chars");
        assert!(
            !digest.contains(|c: char| c.is_ascii_uppercase()),
            "digest must be lowercase hex: {digest}"
        );

        assert!(fs::remove_dir_all(&dir).is_ok(), "clean temp dir");
    }

    #[test]
    fn hashes_empty_file() {
        let dir = unique_temp_dir();
        assert!(fs::create_dir_all(&dir).is_ok(), "create temp dir");
        let file = dir.join("empty.bin");
        assert!(fs::write(&file, b"").is_ok(), "write empty file");

        let digest = sha256_file(&file).expect("hash empty file");
        assert_eq!(digest, SHA256_EMPTY);

        assert!(fs::remove_dir_all(&dir).is_ok(), "clean temp dir");
    }

    #[test]
    fn streams_content_larger_than_one_read_chunk() {
        // Deterministic byte pattern larger than READ_CHUNK so the read loop must
        // iterate and correctly stitch chunks across the boundary.
        let content: Vec<u8> = (0..(READ_CHUNK * 3 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();
        let expected = to_lower_hex(Sha256::digest(&content).as_slice());

        let dir = unique_temp_dir();
        assert!(fs::create_dir_all(&dir).is_ok(), "create temp dir");
        let file = dir.join("big.bin");
        assert!(fs::write(&file, &content).is_ok(), "write big payload");

        let digest = sha256_file(&file).expect("hash big payload");
        assert_eq!(digest, expected);

        assert!(fs::remove_dir_all(&dir).is_ok(), "clean temp dir");
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = unique_temp_dir();
        let missing = dir.join("does_not_exist.bin");

        match sha256_file(&missing) {
            Err(AppError::Io { path, .. }) => assert_eq!(path.as_deref(), Some(missing.as_path())),
            other => panic!("expected AppError::Io, got {other:?}"),
        }
    }

    #[test]
    fn hex_eq_ignores_case() {
        // The acceptance case: same digest, different case.
        assert!(hex_eq(
            SHA256_HELLO_WORLD,
            &SHA256_HELLO_WORLD.to_uppercase()
        ));
        assert!(hex_eq(
            &SHA256_HELLO_WORLD.to_uppercase(),
            SHA256_HELLO_WORLD
        ));
        assert!(hex_eq("AbCdEf", "aBcDeF"));
        assert!(hex_eq("", ""));

        // Different digests stay unequal regardless of case.
        assert!(!hex_eq(SHA256_EMPTY, SHA256_HELLO_WORLD));
        assert!(!hex_eq("abcdef", "abcde0"));
        assert!(!hex_eq("abcde", "abcdef"));
    }
}
