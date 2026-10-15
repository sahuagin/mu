//! mu-09z5y part 3, increment 1: explicit permissions for the durable logs
//! (session event logs and the command journal).
//!
//! These files hold whole conversations, and until now they were created
//! with whatever the process umask allowed. Under a group-sharing
//! `umask 002` that was `rw-rw-r--`: readable by every user on the host.
//! They are now created owner read-write, group read, nothing for others
//! (`0640`), in directories `0750`. The operator chose group read so the
//! shared group can still read them.
//!
//! The umask still applies on top, so a stricter umask (such as `077`)
//! makes them tighter, never looser. Files that already exist keep their
//! mode: this governs what mu creates.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// Mode for a log file mu creates: owner rw, group r, other none.
pub const LOG_FILE_MODE: u32 = 0o640;
/// Mode for a directory mu creates to hold logs: owner rwx, group rx, other none.
pub const LOG_DIR_MODE: u32 = 0o750;

/// `create_dir_all`, but every directory it creates gets [`LOG_DIR_MODE`].
/// Directories that already exist are left as they are.
pub fn create_log_dirs(path: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(LOG_DIR_MODE)
        .create(path)
}

/// Open `path` for appending, creating it with [`LOG_FILE_MODE`] if it
/// doesn't exist. With `create_new`, fail if it already exists.
pub fn open_log_append(path: &Path, create_new: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).mode(LOG_FILE_MODE);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777
    }

    /// Never looser than 0640/0750, whatever the umask; a test process's
    /// umask may make them tighter, which is allowed.
    #[test]
    fn creates_dirs_and_files_without_world_access() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("events").join("daemon-1");
        create_log_dirs(&dir).expect("dirs");
        let file = dir.join("session-1.jsonl");
        open_log_append(&file, false).expect("create");
        for d in [root.path().join("events"), dir.clone()] {
            let m = mode(&d);
            assert_eq!(m & !LOG_DIR_MODE, 0, "{} is {m:o}", d.display());
        }
        let m = mode(&file);
        assert_eq!(m & !LOG_FILE_MODE, 0, "file is {m:o}");
        assert_eq!(m & 0o007, 0, "no access for others: {m:o}");
    }

    #[test]
    fn create_new_refuses_an_existing_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let file = root.path().join("s.jsonl");
        open_log_append(&file, true).expect("first create");
        let err = open_log_append(&file, true).expect_err("second must fail");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // plain create reopens it
        open_log_append(&file, false).expect("reopen");
    }
}
