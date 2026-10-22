//! OS lookups for peer processes. This is the crate's only module with `unsafe` code:
//! a few read-only libc calls.

use std::path::PathBuf;

/// The effective uid of this process.
pub(super) fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions, can't fail and touches no memory.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "macos")]
pub(super) fn executable(pid: i32) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let capacity = usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).ok()?;
    let mut buffer = vec![0u8; capacity];
    // SAFETY: `buffer` is valid for writes of `capacity` bytes for the duration of the
    // call, and `proc_pidpath` writes at most `buffersize` bytes into it.
    let len = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            u32::try_from(capacity).ok()?,
        )
    };
    let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
    buffer.truncate(len.min(capacity));
    Some(PathBuf::from(OsString::from_vec(buffer)))
}

#[cfg(target_os = "macos")]
pub(super) fn parent_pid(pid: i32) -> Option<i32> {
    use std::mem::MaybeUninit;

    let size = i32::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    // SAFETY: `info` points to writable memory of `size` bytes, which is exactly what
    // `PROC_PIDT_SHORTBSDINFO` fills in; the kernel writes nothing beyond `buffersize`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call filled in the whole struct (checked above), and the struct
    // consists of plain integers and byte arrays, valid for any bit pattern.
    let info = unsafe { info.assume_init() };
    i32::try_from(info.pbsi_ppid).ok()
}

#[cfg(target_os = "linux")]
pub(super) fn executable(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "linux")]
pub(super) fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_ppid(&stat)
}

/// The fourth field of `/proc/<pid>/stat`. The second field (the command name) is in
/// parentheses and may itself contain spaces and parentheses, so split after the last
/// `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_ppid(stat: &str) -> Option<i32> {
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_ascii_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn executable(_pid: i32) -> Option<PathBuf> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn parent_pid(_pid: i32) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parsing_handles_odd_command_names() {
        assert_eq!(parse_stat_ppid("42 (zsh) S 7 42 42 0"), Some(7));
        assert_eq!(parse_stat_ppid("42 (a) b) (c) R 99 1"), Some(99));
        assert_eq!(parse_stat_ppid("garbage"), None);
    }
}
