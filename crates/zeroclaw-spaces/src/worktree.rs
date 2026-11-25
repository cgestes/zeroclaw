//! Writing into a working tree that agents and shells edit at the same time.
//!
//! A file is never truncated in place. New content goes to a temporary file
//! that is synced, then atomically *exchanged* with the target. The bytes that
//! come back out are hashed: if they are not what the engine expected, someone
//! wrote the file in between, so the exchange is undone and the caller
//! commits that edit before trying again. Nothing anyone wrote is dropped.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::git::Git;

/// Free space a write must leave on the filesystem, on top of twice the
/// content size, so an agent or the OS still has room to work.
const MIN_FREE_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) enum Placed {
    Done,
    /// The file held something other than what was expected; it was put back.
    Displaced,
}

/// The content and mode of a working-tree file, as git would record it.
pub(crate) struct Disk {
    pub bytes: Vec<u8>,
    pub mode: &'static str,
}

/// Read `path` the way git sees it: symlinks as their target, executables
/// as mode 100755. `None` when nothing is there.
pub(crate) fn read(path: &Path) -> Result<Option<Disk>, Error> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(Some(Disk {
            bytes: target.as_os_str().as_encoded_bytes().to_vec(),
            mode: "120000",
        }));
    }
    if !meta.is_file() {
        return Ok(None);
    }
    Ok(Some(Disk {
        bytes: fs::read(path)?,
        mode: if executable(&meta) {
            "100755"
        } else {
            "100644"
        },
    }))
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_meta: &fs::Metadata) -> bool {
    false
}

/// Put `bytes` (with git `mode`) at `top/rel`, which must currently hold the
/// blob `expected` (`None`: must not exist).
pub(crate) fn write(
    git: &Git,
    top: &Path,
    rel: &str,
    bytes: &[u8],
    mode: &str,
    expected: Option<&str>,
) -> Result<Placed, Error> {
    let target = top.join(rel);
    let dir = target
        .parent()
        .ok_or_else(|| Error::InvalidPath(rel.to_string()))?;
    fs::create_dir_all(dir)?;
    ensure_space(dir, bytes.len() as u64)?;
    let tmp = temp_path(&target);
    create(&tmp, bytes, mode)?;

    if expected.is_none() {
        // A new file: only take the name if it is still free.
        return match rename_noreplace(&tmp, &target) {
            Ok(()) => {
                sync_dir(dir);
                Ok(Placed::Done)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                Ok(Placed::Displaced)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.into())
            }
        };
    }

    match exchange(&tmp, &target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The file vanished since the snapshot: someone deleted it.
            let _ = fs::remove_file(&tmp);
            return Ok(Placed::Displaced);
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    // `tmp` now holds whatever was at the target a moment ago.
    if matches_blob(git, &tmp, expected)? {
        let _ = fs::remove_file(&tmp);
        sync_dir(dir);
        return Ok(Placed::Done);
    }
    // Someone else's edit: swap it back in. Our bytes come back out, unless
    // yet another write landed in between, which is then kept aside too.
    exchange(&tmp, &target)?;
    match read(&tmp)? {
        Some(back) if back.bytes == bytes => {
            let _ = fs::remove_file(&tmp);
        }
        _ => {
            fs::rename(&tmp, recovery_path(&target))?;
        }
    }
    Ok(Placed::Displaced)
}

/// Remove `top/rel`, which must hold the blob `expected`.
pub(crate) fn remove(git: &Git, top: &Path, rel: &str, expected: &str) -> Result<Placed, Error> {
    let target = top.join(rel);
    let tmp = temp_path(&target);
    match fs::rename(&target, &tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Placed::Done),
        Err(e) => return Err(e.into()),
    }
    if matches_blob(git, &tmp, Some(expected))? {
        let _ = fs::remove_file(&tmp);
        if let Some(dir) = target.parent() {
            sync_dir(dir);
            prune_empty_dirs(top, dir);
        }
        return Ok(Placed::Done);
    }
    restore(&tmp, &target)?;
    Ok(Placed::Displaced)
}

/// Move `tmp` (moved aside from `target`) back without clobbering anything
/// that appeared there meanwhile. If the name was taken again, keep the displaced bytes
/// next to it under a recovery name rather than lose them.
fn restore(tmp: &Path, target: &Path) -> Result<(), Error> {
    match rename_noreplace(tmp, target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let keep = recovery_path(target);
            fs::rename(tmp, &keep)?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn matches_blob(git: &Git, path: &Path, expected: Option<&str>) -> Result<bool, Error> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    let Some(disk) = read(path)? else {
        return Ok(false);
    };
    Ok(git.hash_blob(&disk.bytes)? == expected)
}

fn temp_path(target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.zc-{}-{n}.tmp", std::process::id()))
}

fn recovery_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut i = 1;
    loop {
        let p = target.with_file_name(format!("{name}.recovered-{i}"));
        if !p.exists() {
            return p;
        }
        i += 1;
    }
}

fn create(path: &Path, bytes: &[u8], mode: &str) -> Result<(), Error> {
    if mode == "120000" {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let target = std::ffi::OsStr::from_bytes(bytes);
            std::os::unix::fs::symlink(target, path)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        return Err(Error::Invalid("symlinks need a unix filesystem".into()));
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = if mode == "100755" { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(perms))?;
    }
    Ok(())
}

fn prune_empty_dirs(top: &Path, mut dir: &Path) {
    while dir != top && dir.starts_with(top) {
        if fs::remove_dir(dir).is_err() {
            break;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
}

fn sync_dir(dir: &Path) {
    // Best effort: the rename is already atomic; this makes it durable.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(unix)]
fn ensure_space(dir: &Path, size: u64) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidPath(dir.display().to_string()))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    #[allow(clippy::unnecessary_cast)]
    let available = (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
    let needed = size.saturating_mul(2).saturating_add(MIN_FREE_BYTES);
    if available < needed {
        return Err(Error::DiskFull { available, needed });
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_space(_dir: &Path, _size: u64) -> Result<(), Error> {
    Ok(())
}

#[cfg(unix)]
fn cpath(p: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))
}

#[cfg(target_os = "linux")]
fn renameat2(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    let (a, b) = (cpath(from)?, cpath(to)?);
    // SAFETY: both are valid NUL-terminated paths relative to AT_FDCWD.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            flags,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn renamex(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    let (a, b) = (cpath(from)?, cpath(to)?);
    // SAFETY: both are valid NUL-terminated paths.
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Atomically swap two existing paths.
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match renameat2(a, b, libc::RENAME_EXCHANGE) {
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => exchange_fallback(a, b),
            r => r,
        }
    }
    #[cfg(target_os = "macos")]
    {
        match renamex(a, b, libc::RENAME_SWAP) {
            Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => exchange_fallback(a, b),
            r => r,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        exchange_fallback(a, b)
    }
}

/// For filesystems without an atomic exchange: move the target aside, then
/// move the new file in. A write landing between the two steps is caught by
/// the caller's hash check of what was moved aside, or by the no-replace
/// rename failing.
fn exchange_fallback(new: &Path, target: &Path) -> std::io::Result<()> {
    let aside = new.with_extension("aside");
    fs::rename(target, &aside)?;
    if let Err(e) = rename_noreplace(new, target) {
        let _ = fs::rename(&aside, target);
        return Err(e);
    }
    fs::rename(&aside, new)
}

/// Rename `from` to `to`, failing with `AlreadyExists` if `to` exists.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match renameat2(from, to, libc::RENAME_NOREPLACE) {
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => link_noreplace(from, to),
            r => r,
        }
    }
    #[cfg(target_os = "macos")]
    {
        match renamex(from, to, libc::RENAME_EXCL) {
            Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => link_noreplace(from, to),
            r => r,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        link_noreplace(from, to)
    }
}

/// `link` fails if the name exists, which makes it a portable no-replace
/// rename for regular files.
fn link_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(from)?.file_type().is_symlink() {
        if to.exists() || fs::symlink_metadata(to).is_ok() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        return fs::rename(from, to);
    }
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, Git) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["init", "-q"])
            .status()
            .expect("git runs")
            .success();
        assert!(ok);
        let git = Git::new(tmp.path());
        (tmp, git)
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".zc-") || n.contains(".recovered-"))
            .collect()
    }

    #[test]
    fn a_write_replaces_the_expected_bytes() {
        let (tmp, git) = repo();
        fs::write(tmp.path().join("a.md"), "old\n").expect("write");
        let expected = git.hash_blob(b"old\n").expect("hash");
        let placed = write(
            &git,
            tmp.path(),
            "a.md",
            b"new\n",
            "100644",
            Some(&expected),
        )
        .expect("write");
        assert!(matches!(placed, Placed::Done));
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.md")).expect("read"),
            "new\n"
        );
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn a_write_that_finds_someone_elses_edit_puts_it_back() {
        let (tmp, git) = repo();
        // The engine expects "old", but an agent already wrote "theirs".
        fs::write(tmp.path().join("a.md"), "theirs\n").expect("write");
        let expected = git.hash_blob(b"old\n").expect("hash");
        let placed = write(
            &git,
            tmp.path(),
            "a.md",
            b"new\n",
            "100644",
            Some(&expected),
        )
        .expect("write");
        assert!(matches!(placed, Placed::Displaced));
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.md")).expect("read"),
            "theirs\n"
        );
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn a_new_file_never_replaces_one_that_appeared() {
        let (tmp, git) = repo();
        fs::write(tmp.path().join("b.md"), "someone\n").expect("write");
        let placed = write(&git, tmp.path(), "b.md", b"mine\n", "100644", None).expect("write");
        assert!(matches!(placed, Placed::Displaced));
        assert_eq!(
            fs::read_to_string(tmp.path().join("b.md")).expect("read"),
            "someone\n"
        );
    }

    #[test]
    fn a_remove_keeps_an_unexpected_edit() {
        let (tmp, git) = repo();
        fs::create_dir_all(tmp.path().join("d")).expect("mkdir");
        fs::write(tmp.path().join("d/c.md"), "edited\n").expect("write");
        let expected = git.hash_blob(b"original\n").expect("hash");
        let placed = remove(&git, tmp.path(), "d/c.md", &expected).expect("remove");
        assert!(matches!(placed, Placed::Displaced));
        assert_eq!(
            fs::read_to_string(tmp.path().join("d/c.md")).expect("read"),
            "edited\n"
        );

        let expected = git.hash_blob(b"edited\n").expect("hash");
        let placed = remove(&git, tmp.path(), "d/c.md", &expected).expect("remove");
        assert!(matches!(placed, Placed::Done));
        assert!(!tmp.path().join("d").exists(), "empty folders are pruned");
    }

    #[test]
    fn executables_keep_their_mode() {
        let (tmp, git) = repo();
        write(&git, tmp.path(), "run.sh", b"#!/bin/sh\n", "100755", None).expect("write");
        let disk = read(&tmp.path().join("run.sh"))
            .expect("read")
            .expect("exists");
        assert_eq!(disk.mode, "100755");
    }
}
