//! Zona Installer self-update helper (`update.exe`).
//!
//! Replaces the old PowerShell `update.ps1`. It is a plain console binary with
//! no GUI dependencies so it runs on Windows and under Wine.
//!
//! The installer copies this binary to a temp directory and spawns the copy, so
//! extracting the release zip over the install directory never tries to
//! overwrite this running image (Windows and Wine lock running executables).
//!
//! Invoked as:
//!   update.exe -InstallerPid <pid> -Url <zip-url> -SigUrl <sig-url> \
//!              -Dest <zona-dir> -Relaunch "<flags>"
//!
//! It waits for the old installer to exit (releasing its exe lock), downloads
//! the release zip and its detached OpenPGP signature, verifies the signature
//! against the embedded public key, extracts the zip over `-Dest`, verifies the
//! new installer exists, and relaunches it with the stored flags.

use std::env;
use std::ffi::OsString;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
};

/// The User-Agent GitHub requires on asset requests.
const USER_AGENT: &str = "zona-installer";

/// The armored public key embedded at compile time (`dependencies/public-key.asc`).
const PUBLIC_KEY: &[u8] = include_bytes!("../../dependencies/public-key.asc");

/// Cap downloads at 50MB (the release zip is ~5MB) to avoid unbounded reads.
const DOWNLOAD_LIMIT: u64 = 50 * 1024 * 1024;

/// The new installer's path relative to the Zona directory.
const NEW_EXE: &str = "installer\\zona-installer.exe";

fn main() {
    match parse_args(env::args_os().skip(1).collect()) {
        Err(message) => fail(&message),
        Ok(args) => {
            if let Err(message) = run(&args) {
                fail(&message);
            }
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("Update failed: {message}");
    std::process::exit(1);
}

#[derive(Debug)]
struct Args {
    installer_pid: u32,
    url: String,
    sig_url: String,
    dest: PathBuf,
    relaunch: String,
}

/// Parses the `-Name value` command line (pure).
fn parse_args(raw: Vec<OsString>) -> Result<Args, String> {
    let mut installer_pid: Option<u32> = None;
    let mut url: Option<String> = None;
    let mut sig_url: Option<String> = None;
    let mut dest: Option<PathBuf> = None;
    let mut relaunch = String::new();

    let mut args = raw.into_iter();
    while let Some(flag) = args.next() {
        let flag = flag.to_string_lossy().to_ascii_lowercase();
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("missing value for {flag}"))
                .map(|v| v.to_string_lossy().into_owned())
        };
        match flag.as_str() {
            "-installerpid" => {
                installer_pid = Some(
                    value()?
                        .parse::<u32>()
                        .map_err(|e| format!("invalid -InstallerPid: {e}"))?,
                );
            }
            "-url" => url = Some(value()?),
            "-sigurl" => sig_url = Some(value()?),
            "-dest" => dest = Some(PathBuf::from(value()?)),
            "-relaunch" => relaunch = value()?,
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        installer_pid: installer_pid.ok_or_else(|| "-InstallerPid is required".to_string())?,
        url: url.ok_or_else(|| "-Url is required".to_string())?,
        sig_url: sig_url.ok_or_else(|| "-SigUrl is required".to_string())?,
        dest: dest.ok_or_else(|| "-Dest is required".to_string())?,
        relaunch,
    })
}

fn run(args: &Args) -> Result<(), String> {
    println!("Updating Zona installer...");

    // 1. Wait for the old installer to fully exit, releasing its exe lock.
    wait_for_process(args.installer_pid);

    // 2. Download the release zip and its detached signature.
    println!("Downloading update...");
    let zip = download(&args.url)?;
    let signature = download(&args.sig_url)?;

    // 3. Verify the signature before touching anything on disk.
    println!("Verifying signature...");
    verify_signature(&signature, &zip)?;

    // 4. Extract over the Zona directory. The old exe is gone and this helper
    //    runs from temp, so nothing under the install dir is locked.
    println!("Extracting...");
    extract_zip(&zip, &args.dest)?;

    // 5. Verify the new installer exists before launching it.
    let new_exe = args.dest.join(NEW_EXE);
    if !new_exe.is_file() {
        return Err(format!("{} was not found after extraction", new_exe.display()));
    }

    // 6. Relaunch the new installer with the stored flags.
    println!("Launching updated installer...");
    relaunch(&new_exe, &args.relaunch)
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    response
        .body_mut()
        .with_config()
        .limit(DOWNLOAD_LIMIT)
        .read_to_vec()
        .map_err(|e| format!("download failed: {e}"))
}

/// Blocks until the installer process exits (impure, best effort).
///
/// The installer already exits immediately after spawning this helper, but the
/// running exe's file lock can outlive it briefly; waiting here ensures the old
/// `zona-installer.exe` can be overwritten during extraction.
fn wait_for_process(pid: u32) {
    // SAFETY: `pid` is the installer's own process id; `PROCESS_SYNCHRONIZE`
    // requests only the right to wait on it. A null handle means it is already
    // gone, in which case there is nothing to wait for.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return;
    }
    let _guard = HandleGuard(handle);
    // SAFETY: `handle` is a live process handle; `INFINITE` blocks until exit.
    unsafe {
        WaitForSingleObject(handle, INFINITE);
    }
}

/// Closes a process handle on drop.
struct HandleGuard(HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `OpenProcess` and is not used after this.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Verifies `signature` is a valid detached OpenPGP signature over `data`,
/// made by the key in [`PUBLIC_KEY`].
fn verify_signature(signature: &[u8], data: &[u8]) -> Result<(), String> {
    verify_signature_with_key(PUBLIC_KEY, signature, data)
}

/// Verifies a detached OpenPGP signature with an explicit public key (pure
/// apart from the OpenPGP backend). Split from [`verify_signature`] so it can
/// be tested with an ephemeral key.
fn verify_signature_with_key(public_key: &[u8], signature: &[u8], data: &[u8]) -> Result<(), String> {
    use sequoia_openpgp as openpgp;
    use openpgp::parse::{Parse, stream::*};
    use openpgp::policy::StandardPolicy;
    use openpgp::Cert;

    let cert = Cert::from_bytes(public_key)
        .map_err(|e| format!("embedded signing key is invalid: {e}"))?;

    struct Helper {
        cert: Cert,
    }
    impl VerificationHelper for Helper {
        fn get_certs(&mut self, _ids: &[openpgp::KeyHandle]) -> openpgp::Result<Vec<Cert>> {
            Ok(vec![self.cert.clone()])
        }
        fn check(&mut self, structure: MessageStructure<'_>) -> openpgp::Result<()> {
            let good = structure.into_iter().any(|layer| {
                matches!(
                    layer,
                    MessageLayer::SignatureGroup { ref results }
                        if results.iter().any(|r| r.is_ok())
                )
            });
            if good {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "signature does not verify against the embedded public key",
                )
                .into())
            }
        }
    }

    let policy = StandardPolicy::new();
    let helper = Helper { cert };
    let mut verifier = DetachedVerifierBuilder::from_bytes(signature)
        .map_err(|e| format!("could not parse signature: {e}"))?
        .with_policy(&policy, None, helper)
        .map_err(|e| format!("could not set up signature verification: {e}"))?;
    verifier
        .verify_bytes(data)
        .map_err(|e| format!("signature verification failed: {e}"))
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("could not open update zip: {e}"))?;
    archive
        .extract(dest)
        .map_err(|e| format!("could not extract update zip to {}: {e}", dest.display()))
}

fn relaunch(exe: &Path, relaunch: &str) -> Result<(), String> {
    let mut command = Command::new(exe);
    if !relaunch.trim().is_empty() {
        // The relaunch flags are space-free (see `relaunch_args_string`), so
        // splitting on whitespace is safe.
        command.args(relaunch.split_whitespace());
    }
    command
        .spawn()
        .map_err(|e| format!("could not launch {}: {e}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn parse_args_reads_all_flags() {
        let args = parse_args(os(&[
            "-InstallerPid", "1234",
            "-Url", "https://example.com/zona-installer.zip",
            "-SigUrl", "https://example.com/zona-installer.zip.asc",
            "-Dest", r"C:\Zona",
            "-Relaunch", "--debug-console",
        ]))
        .expect("parse args");
        assert_eq!(args.installer_pid, 1234);
        assert_eq!(args.url, "https://example.com/zona-installer.zip");
        assert_eq!(args.sig_url, "https://example.com/zona-installer.zip.asc");
        assert_eq!(args.dest, PathBuf::from(r"C:\Zona"));
        assert_eq!(args.relaunch, "--debug-console");
    }

    #[test]
    fn parse_args_accepts_an_empty_relaunch() {
        let args = parse_args(os(&[
            "-InstallerPid", "1",
            "-Url", "https://example.com/z",
            "-SigUrl", "https://example.com/z.asc",
            "-Dest", r"C:\Zona",
        ]))
        .expect("parse args");
        assert_eq!(args.relaunch, "");
    }

    #[test]
    fn parse_args_rejects_missing_required_flags() {
        assert!(parse_args(os(&[
            "-Url", "https://example.com/z",
            "-SigUrl", "https://example.com/z.asc",
            "-Dest", r"C:\Zona",
        ]))
        .is_err());
    }

    #[test]
    fn verify_signature_roundtrip_and_tamper_detection() {
        use std::io::Write;
        use sequoia_openpgp as openpgp;
        use openpgp::cert::{CertBuilder, CipherSuite};
        use openpgp::policy::StandardPolicy;
        use openpgp::serialize::SerializeInto;
        use openpgp::serialize::stream::{Message, Signer};

        // Generate an ephemeral key, sign some bytes, and verify with the same
        // path the shipped binary uses (without gpg, so the whole flow is
        // exercised in CI and on any machine).
        let (cert, _revocation) = CertBuilder::new()
            .set_cipher_suite(CipherSuite::Cv25519)
            .add_userid("update-test@example.com")
            .add_signing_subkey()
            .generate()
            .expect("generate test cert");

        let public_key = cert
            .clone()
            .strip_secret_key_material()
            .armored()
            .to_vec()
            .expect("export public key");

        let policy = StandardPolicy::new();
        let keypair = cert
            .keys()
            .secret()
            .with_policy(&policy, None)
            .supported()
            .alive()
            .revoked(false)
            .for_signing()
            .next()
            .expect("signing key")
            .key()
            .clone()
            .into_keypair()
            .expect("keypair");

        let data = b"release zip bytes";
        let mut sink = Vec::new();
        {
            let message = Message::new(&mut sink);
            let mut signer = Signer::new(message, keypair)
                .expect("signer")
                .detached()
                .build()
                .expect("build signer");
            signer.write_all(data).expect("sign data");
            signer.finalize().expect("finalize signature");
        }

        verify_signature_with_key(&public_key, &sink, data).expect("valid signature");
        assert!(verify_signature_with_key(&public_key, &sink, b"tampered").is_err());
    }
}
