//! OS-specific process and descriptor queries: Linux reads `/proc`, macOS uses libproc.

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::path::PathBuf;

/// A process visible to this user: (pid, process group, session, uid).
#[derive(Clone, Copy, Debug)]
pub struct ProcessIds {
    pub pid: u32,
    pub pgid: u32,
    pub session: u32,
    pub uid: u32,
}

/// Path of an open descriptor, as the kernel resolved it.
#[cfg(target_os = "linux")]
pub fn fd_path(fd: RawFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
}

#[cfg(target_os = "macos")]
pub fn fd_path(fd: RawFd) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(c_path(&buf))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn fd_path(_fd: RawFd) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor paths are unsupported",
    ))
}

/// Current working directory of a process.
#[cfg(target_os = "linux")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let raw = info.pvi_cdir.vip_path.as_flattened();
    let bytes: Vec<u8> = raw.iter().map(|c| *c as u8).collect();
    let path = c_path(&bytes);
    (!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// Foreground process group of the terminal a process is attached to.
#[cfg(target_os = "linux")]
pub fn foreground_group(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command: state ppid pgrp session tty_nr tpgid.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(5)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .and_then(|v| u32::try_from(v).ok())
}

#[cfg(target_os = "macos")]
pub fn foreground_group(pid: u32) -> Option<u32> {
    bsd_info(pid).map(|info| info.e_tpgid).filter(|v| *v > 0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn foreground_group(_pid: u32) -> Option<u32> {
    None
}

/// NUL-separated command line of a process, as raw bytes.
#[cfg(target_os = "linux")]
pub fn process_cmdline(pid: u32) -> Vec<u8> {
    std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default()
}

#[cfg(target_os = "macos")]
pub fn process_cmdline(pid: u32) -> Vec<u8> {
    // KERN_PROCARGS2: argc, exec path, NUL padding, then argv strings.
    let Ok(pid) = i32::try_from(pid) else {
        return Vec::new();
    };
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let sysctl = |buf: *mut libc::c_void, size: &mut libc::size_t, mib: &mut [i32; 3]| unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buf, size, std::ptr::null_mut(), 0)
    };
    if sysctl(std::ptr::null_mut(), &mut size, &mut mib) != 0 || size < 4 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size];
    if sysctl(buf.as_mut_ptr().cast(), &mut size, &mut mib) != 0 || size < 4 {
        return Vec::new();
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
    let rest = &buf[4..];
    let Some(path_end) = rest.iter().position(|b| *b == 0) else {
        return Vec::new();
    };
    let Some(start) = rest[path_end..].iter().position(|b| *b != 0) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for arg in rest[path_end + start..].split(|b| *b == 0).take(argc) {
        out.extend_from_slice(arg);
        out.push(0);
    }
    out
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_cmdline(_pid: u32) -> Vec<u8> {
    Vec::new()
}

/// Every process the kernel lists, with its group, session, and owner.
#[cfg(target_os = "linux")]
pub fn process_ids() -> Option<Vec<ProcessIds>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = rest.split_whitespace().take(4).collect();
        if let [_, _, pgid, session] = fields[..]
            && let (Ok(pgid), Ok(session)) = (pgid.parse(), session.parse())
        {
            out.push(ProcessIds {
                pid,
                pgid,
                session,
                uid: metadata.uid(),
            });
        }
    }
    Some(out)
}

#[cfg(target_os = "macos")]
pub fn process_ids() -> Option<Vec<ProcessIds>> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return None;
    }
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if listed <= 0 {
        return None;
    }
    pids.truncate(listed as usize);
    Some(
        pids.into_iter()
            .filter(|pid| *pid > 0)
            .filter_map(|pid| {
                let info = bsd_info(pid as u32)?;
                let session = unsafe { libc::getsid(pid) };
                (session > 0).then_some(ProcessIds {
                    pid: pid as u32,
                    pgid: info.pbi_pgid,
                    session: session as u32,
                    uid: info.pbi_uid,
                })
            })
            .collect(),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_ids() -> Option<Vec<ProcessIds>> {
    None
}

/// Process count per session ID.
pub fn session_census() -> Option<HashMap<u32, usize>> {
    let mut counts = HashMap::new();
    for process in process_ids()? {
        *counts.entry(process.session).or_insert(0) += 1;
    }
    Some(counts)
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then_some(info)
}

#[cfg(target_os = "macos")]
fn c_path(buf: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_process() {
        let pid = std::process::id();
        assert_eq!(
            process_cwd(pid),
            std::env::current_dir()
                .ok()
                .and_then(|d| d.canonicalize().ok())
        );
        let cmdline = process_cmdline(pid);
        assert!(!cmdline.is_empty());
        let ids = process_ids().expect("process list");
        assert!(ids.iter().any(|p| p.pid == pid));
        assert!(session_census().is_some_and(|c| !c.is_empty()));
        let file = std::fs::File::open("Cargo.toml").unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(
            fd_path(file.as_raw_fd()).unwrap(),
            std::fs::canonicalize("Cargo.toml").unwrap()
        );
    }
}
