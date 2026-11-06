//! The shared jobserver itself: a named FIFO holding one byte per free
//! build slot, GNU make 4.4 style (`--jobserver-auth=fifo:<path>`).
//!
//! Clients (cargo, rustc, make ≥ 4.4, ninja ≥ 1.13 — anything on the
//! `jobserver` crate) read a byte before starting extra work and write it
//! back when done. Every top-level client also holds one implicit slot it
//! never reads, so a pool for N slots holds N − 1 bytes: one build alone
//! gets exactly N jobs, the same as `make -jN` or cargo's own default.
//!
//! The engine keeps the FIFO open read-write for its whole life. That matters:
//! a pipe's bytes vanish when its last descriptor closes, and it lets the
//! engine count and top up the bytes in it.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The byte GNU make writes for a slot.
const TOKEN: u8 = b'+';

pub(crate) struct SlotPool {
    path: PathBuf,
    file: File,
    /// Bytes the pool holds when nothing is building.
    tokens: usize,
    /// The FIFO's identity, to tell ours from a replacement at the path.
    dev: u64,
    ino: u64,
}

impl SlotPool {
    /// A fresh pool of `tokens` bytes at `dir/slots.fifo`. `dir` is created
    /// owner-only (or must already be: ours, a real directory, 0700); a FIFO
    /// or anything else already at the path — a stale pool from a crashed
    /// engine — is replaced.
    pub(crate) fn create(dir: &Path, tokens: usize) -> io::Result<Self> {
        ensure_private_dir(dir)?;
        let path = dir.join("slots.fifo");
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in pool path"))?;
        // SAFETY: a valid NUL-terminated path; mkfifo only reads it.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // The umask can only narrow 0600, but be explicit.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        // Read-write: opening never blocks waiting for the other end, and the
        // engine counts as both reader and writer for the pool's whole life.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        let meta = file.metadata()?;
        let pool = Self {
            path,
            file,
            tokens,
            dev: meta.dev(),
            ino: meta.ino(),
        };
        pool.add(tokens)?;
        Ok(pool)
    }

    /// `fifo:<path>`, the `--jobserver-auth` value.
    pub(crate) fn auth(&self) -> String {
        format!("fifo:{}", self.path.display())
    }

    pub(crate) fn tokens(&self) -> usize {
        self.tokens
    }

    /// The path still names this pool's FIFO (not deleted by a temp-dir
    /// sweep, not replaced).
    pub(crate) fn is_intact(&self) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok_and(|meta| {
            meta.file_type().is_fifo() && meta.dev() == self.dev && meta.ino() == self.ino
        })
    }

    /// Free slots: bytes sitting in the FIFO right now.
    pub(crate) fn available(&self) -> io::Result<usize> {
        bytes_available(&self.file)
    }

    /// Put `count` slots (back) into the pool.
    pub(crate) fn add(&self, count: usize) -> io::Result<()> {
        let mut left = count;
        let chunk = [TOKEN; 64];
        while left > 0 {
            let n = (&self.file).write(&chunk[..left.min(chunk.len())])?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            left -= n;
        }
        Ok(())
    }

    /// Take up to `count` free slots out of the pool; how many it got.
    pub(crate) fn take(&self, count: usize) -> io::Result<usize> {
        let mut buf = [0u8; 64];
        let mut taken = 0;
        while taken < count {
            let want = (count - taken).min(buf.len());
            match (&self.file).read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => taken += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(taken)
    }
}

impl Drop for SlotPool {
    fn drop(&mut self) {
        // Only unlink what is still ours; clients holding slots keep their
        // own descriptors and simply finish.
        if self.is_intact() {
            let _ = std::fs::remove_file(&self.path);
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
}

/// Create `dir` owner-only, or accept it if it already is one: a real
/// directory (not a symlink) owned by us with no group/other access. In a
/// shared `/tmp` this is what keeps other users out of the pool.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !meta.file_type().is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory", dir.display()),
        ));
    }
    Ok(())
}

/// Bytes waiting in a pipe/FIFO. macOS answers FIONREAD with 0 for FIFOs,
/// so it asks kqueue (EVFILT_READ's `data` is the byte count) instead.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn bytes_available(file: &File) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: plain kqueue/kevent calls on a descriptor we own for the
    // duration; the zeroed structs are valid inputs and outputs.
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut change: libc::kevent = std::mem::zeroed();
        change.ident = file.as_raw_fd() as libc::uintptr_t;
        change.filter = libc::EVFILT_READ;
        change.flags = libc::EV_ADD;
        let mut event: libc::kevent = std::mem::zeroed();
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let n = libc::kevent(kq, &change, 1, &mut event, 1, &timeout);
        let err = io::Error::last_os_error();
        libc::close(kq);
        if n < 0 {
            return Err(err);
        }
        if n == 0 {
            return Ok(0);
        }
        if event.flags & libc::EV_ERROR != 0 {
            return Err(io::Error::from_raw_os_error(event.data as i32));
        }
        Ok(event.data.max(0) as usize)
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn bytes_available(file: &File) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one c_int through the pointer.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FIONREAD, &mut n) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n.max(0) as usize)
}
