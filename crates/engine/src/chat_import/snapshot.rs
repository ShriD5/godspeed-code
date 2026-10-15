//! Read-only snapshots of another app's SQLite database.
//!
//! Source apps may hold their databases open (WAL mode) while we read, so a
//! live file is never opened: the database and its `-wal`/`-shm` siblings
//! are copied into a private temp dir (an APFS clone on macOS, so even a
//! 1 GB file costs nothing) and the copy is opened read-only.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::EngineError;

/// A read-only connection to a private copy; the copy lives as long as this.
pub(crate) struct SqliteCopy {
    pub conn: Connection,
    _dir: tempfile::TempDir,
}

impl SqliteCopy {
    pub fn open(db: &Path) -> Result<Self, EngineError> {
        let dir = tempfile::Builder::new()
            .prefix("godspeed-import-")
            .tempdir()?;
        let name = db
            .file_name()
            .ok_or_else(|| EngineError::Other(format!("{} has no file name", db.display())))?;
        let copy = dir.path().join(name);
        std::fs::copy(db, &copy)?;
        for suffix in ["-wal", "-shm"] {
            let mut sibling = db.as_os_str().to_owned();
            sibling.push(suffix);
            let sibling = PathBuf::from(sibling);
            if sibling.is_file() {
                let mut target = copy.as_os_str().to_owned();
                target.push(suffix);
                std::fs::copy(&sibling, PathBuf::from(target))?;
            }
        }
        let conn = Connection::open_with_flags(
            &copy,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|err| EngineError::Other(format!("{}: {err}", db.display())))?;
        Ok(Self { conn, _dir: dir })
    }
}
