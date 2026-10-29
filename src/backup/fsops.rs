//! Thin libc wrappers for metadata operations std does not offer (xattrs, mknod, nanosecond
//! times without following symlinks, statvfs/statfs, syncfs) and a mount-safe recursive delete.

use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))
}

fn cname(n: &[u8]) -> io::Result<CString> {
    CString::new(n)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "xattr name contains NUL"))
}

fn is_unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::ENOSYS))
}

/// Lists extended attribute names of `path` (not following symlinks). A file system without
/// xattr support yields an empty list.
pub(crate) fn list_xattrs(path: &Path) -> io::Result<Vec<Vec<u8>>> {
    let c = cpath(path)?;
    loop {
        // SAFETY: c is a valid C string; a NULL buffer with size 0 queries the needed size.
        let size = unsafe { libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            let e = io::Error::last_os_error();
            return if is_unsupported(&e) {
                Ok(Vec::new())
            } else {
                Err(e)
            };
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf has the given length.
        let got = unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ERANGE) {
                continue; // list grew in between
            }
            return Err(e);
        }
        buf.truncate(got as usize);
        let mut names: Vec<Vec<u8>> = buf
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_vec())
            .collect();
        names.sort();
        return Ok(names);
    }
}

/// Reads one extended attribute (not following symlinks). `None` if it vanished.
pub(crate) fn get_xattr(path: &Path, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let c = cpath(path)?;
    let n = cname(name)?;
    loop {
        // SAFETY: valid C strings, NULL/0 queries the size.
        let size = unsafe { libc::lgetxattr(c.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::ENODATA) {
                Ok(None)
            } else {
                Err(e)
            };
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf has the given length.
        let got =
            unsafe { libc::lgetxattr(c.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ERANGE) => continue,
                Some(libc::ENODATA) => return Ok(None),
                _ => return Err(e),
            }
        }
        buf.truncate(got as usize);
        return Ok(Some(buf));
    }
}

/// Sets an extended attribute on `path` without following symlinks.
pub(crate) fn set_xattr(path: &Path, name: &[u8], value: &[u8]) -> io::Result<()> {
    let c = cpath(path)?;
    let n = cname(name)?;
    // SAFETY: valid pointers and lengths.
    let r = unsafe {
        libc::lsetxattr(
            c.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Sets an extended attribute on an open file.
pub(crate) fn fset_xattr(file: &fs::File, name: &[u8], value: &[u8]) -> io::Result<()> {
    let n = cname(name)?;
    // SAFETY: valid fd, pointers and lengths.
    let r = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `mknod(2)` for character/block devices and FIFOs. `mode` includes the file type bits.
pub(crate) fn mknod(path: &Path, mode: u32, major: u32, minor: u32) -> io::Result<()> {
    let c = cpath(path)?;
    let dev = libc::makedev(major, minor);
    // SAFETY: valid C string.
    let r = unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, dev) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `chmod` that never follows a symlink (`fchmodat` without AT_SYMLINK_NOFOLLOW would follow,
/// so callers only use it on paths they just created or verified with lstat).
pub(crate) fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn timespec(sec: i64, nsec: u32) -> libc::timespec {
    libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec as _,
    }
}

/// Sets atime and mtime of `path` (not following symlinks) to the given time.
pub(crate) fn set_mtime_nofollow(path: &Path, sec: i64, nsec: u32) -> io::Result<()> {
    let c = cpath(path)?;
    let ts = [timespec(sec, nsec), timespec(sec, nsec)];
    // SAFETY: valid C string, two timespecs.
    let r = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            ts.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Sets atime and mtime of an open file.
pub(crate) fn fset_mtime(file: &fs::File, sec: i64, nsec: u32) -> io::Result<()> {
    let ts = [timespec(sec, nsec), timespec(sec, nsec)];
    // SAFETY: valid fd, two timespecs.
    let r = unsafe { libc::futimens(file.as_raw_fd(), ts.as_ptr()) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Free and used space of the file system containing `path`, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsSpace {
    pub total: u64,
    pub used: u64,
    /// Available to unprivileged users (`f_bavail`).
    pub available: u64,
}

pub fn fs_space(path: &Path) -> io::Result<FsSpace> {
    let c = cpath(path)?;
    // SAFETY: zeroed statvfs is a valid out parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and out pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let frsize = st.f_frsize as u64;
    Ok(FsSpace {
        total: st.f_blocks as u64 * frsize,
        used: (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * frsize,
        available: st.f_bavail as u64 * frsize,
    })
}

/// `statfs(2)` `f_type` of the file system containing `path`.
pub(crate) fn fs_type(path: &Path) -> io::Result<i64> {
    let c = cpath(path)?;
    // SAFETY: zeroed statfs is a valid out parameter.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and out pointer.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    Ok(st.f_type as i64)
}

/// `syncfs(2)` on the file system containing `path`.
pub(crate) fn syncfs(path: &Path) -> io::Result<()> {
    let f = fs::File::open(path)?;
    // SAFETY: valid fd.
    if unsafe { libc::syncfs(f.as_raw_fd()) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Recursively deletes `path` (file, symlink or directory) without following symlinks and
/// without descending into other file systems (`root_dev`). Errors are pushed to `errors` as
/// `(path, message)` and do not stop the deletion of other entries. Returns the number of
/// removed entries.
pub(crate) fn remove_tree(
    path: &Path,
    root_dev: u64,
    errors: &mut Vec<(String, String)>,
    cancel: &super::CancelToken,
) -> u64 {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return 0,
        Err(e) => {
            errors.push((path.display().to_string(), e.to_string()));
            return 0;
        }
    };
    if !md.is_dir() {
        return match fs::remove_file(path) {
            Ok(()) => 1,
            Err(e) => {
                errors.push((path.display().to_string(), e.to_string()));
                0
            }
        };
    }
    if md.dev() != root_dev {
        errors.push((
            path.display().to_string(),
            "is a mount point, not deleted".into(),
        ));
        return 0;
    }
    let mut removed = 0;
    match fs::read_dir(path) {
        Ok(rd) => {
            for ent in rd {
                if cancel.is_cancelled() {
                    return removed;
                }
                match ent {
                    Ok(ent) => removed += remove_tree(&ent.path(), root_dev, errors, cancel),
                    Err(e) => errors.push((path.display().to_string(), e.to_string())),
                }
            }
        }
        Err(e) => {
            errors.push((path.display().to_string(), e.to_string()));
            return removed;
        }
    }
    match fs::remove_dir(path) {
        Ok(()) => removed + 1,
        Err(e) => {
            errors.push((path.display().to_string(), e.to_string()));
            removed
        }
    }
}

/// Deletes every top-level entry of `root` except the names in `keep`. See [`remove_tree`].
pub(crate) fn clear_dir(
    root: &Path,
    keep: &[&str],
    errors: &mut Vec<(String, String)>,
    cancel: &super::CancelToken,
) -> io::Result<u64> {
    let root_dev = fs::symlink_metadata(root)?.dev();
    let mut names: Vec<_> = fs::read_dir(root)?.collect::<io::Result<Vec<_>>>()?;
    names.sort_by_key(|e| e.file_name());
    let mut removed = 0;
    for ent in names {
        if cancel.is_cancelled() {
            break;
        }
        let name = ent.file_name();
        if keep.iter().any(|k| OsStr::new(k) == name) {
            continue;
        }
        removed += remove_tree(&ent.path(), root_dev, errors, cancel);
    }
    Ok(removed)
}
