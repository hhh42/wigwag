//! Process-level hardening, and checks on the secret files wigwag reads.

use std::path::Path;

use anyhow::{Context as _, bail};

/// Called first thing in `main`, before any file, socket or thread exists.
///
/// - umask 077: every file and socket wigwag creates (database, attachments, the
///   Unix socket) is private to this user.
/// - No core dumps: a crash must not write identity keys and the database key to disk.
/// - Linux: not dumpable, so other processes running as the same user can't ptrace
///   wigwag or read its memory through /proc. Reset by execve, so the agent command
///   it runs is unaffected.
pub fn process() {
    #[cfg(unix)]
    unsafe {
        libc::umask(0o077);
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &none) != 0 {
            eprintln!("warning: could not turn off core dumps");
        }
    }
    #[cfg(target_os = "linux")]
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            eprintln!("warning: could not mark the process non-dumpable");
        }
    }
}

/// Reads a secret from a file that only its owner can read, the way ssh treats keys.
pub fn read_secret(path: &Path, what: &str) -> anyhow::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(path)
            .with_context(|| format!("cannot read {what} file {}", path.display()))?;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{what} file {} is readable by others (mode {:o}); run: chmod 600 {}",
                path.display(),
                mode & 0o777,
                path.display()
            );
        }
        // SAFETY: getuid has no preconditions.
        let me = unsafe { libc::getuid() };
        if meta.uid() != me {
            bail!(
                "{what} file {} is owned by uid {}, not by you ({me})",
                path.display(),
                meta.uid()
            );
        }
    }
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {what} file {}", path.display()))?;
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn refuses_secrets_others_can_read() {
        let dir = std::env::temp_dir().join(format!("wigwag-test-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token");
        std::fs::write(&p, "abc\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_secret(&p, "token").unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_secret(&p, "token").unwrap(), "abc");
    }
}
