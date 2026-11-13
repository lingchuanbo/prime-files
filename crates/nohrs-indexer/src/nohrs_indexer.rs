//! Instant whole-computer indexing for Prime Files, the way Everything does
//! it: by reading each NTFS volume's master file table (MFT) instead of
//! opening every folder.
//!
//! Reading a volume's MFT needs administrator rights, which Prime Files —
//! installed per user — does not have. So a small Windows service, installed
//! once with the person's consent (UAC), reads it and hands the names to the
//! app over a local pipe. It is read-only, keeps an in-memory snapshot and
//! follows the NTFS change journal in the background. The pipe answers only
//! the user who installed it
//! (and administrators), and other users' profile folders are named but
//! not listed, as the app's own walk can see them.
//!
//! - [`protocol`]: what goes over the pipe; both sides use it.
//! - [`mft`]: reading a volume's records into a tree of names.
//! - [`client`]: the app's side.
//! - `service` and `install`: the service's side (Windows only).

pub mod mft;
pub mod protocol;

#[cfg(windows)]
pub mod client;
#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod service;

/// The service's name in the Service Control Manager.
pub const SERVICE_NAME: &str = "PrimeFilesIndexer";
/// The pipe the service answers on.
pub const PIPE_NAME: &str = r"\\.\pipe\PrimeFilesIndexer";
/// The program file, beside PrimeFiles.exe and in Program Files.
pub const EXE_NAME: &str = "prime-files-indexer.exe";

/// Whether `text` is a string SID (`S-1-5-21-…`): it goes into a security
/// descriptor and a service's command line, so nothing else may pass.
pub fn is_sid(text: &str) -> bool {
    let mut parts = text.split('-');
    parts.next() == Some("S")
        && text.len() <= 184
        && text.split('-').skip(1).count() >= 2
        && parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_a_string_sid_passes() {
        assert!(is_sid("S-1-5-21-3623811015-3361044348-30300820-1013"));
        assert!(is_sid("S-1-5-18"));
        assert!(!is_sid("S-1-5-21-1;(A;;GA;;;WD)"));
        assert!(!is_sid("S-1"));
        assert!(!is_sid("S-1-5-21- --install"));
        assert!(!is_sid(""));
    }
}
