//! Disk information for the header line and the free-space safety check.
//!
//! Identifies the volume hosting the target directory the way `df` would:
//! mount source (device node or network share), volume name where the
//! platform has one, filesystem type, and capacity. Each platform queries
//! the OS directly — `statfs` on macOS, `/proc/self/mountinfo` on Linux,
//! the volume APIs on Windows — so network mounts (SMB/NFS/UNC) report
//! their real source instead of being skipped.

use std::path::{Path, PathBuf};

/// The disk (volume) hosting the benchmark target directory.
#[derive(Debug, Clone)]
pub struct DiskInfo {
    /// Mount source: a device node (`/dev/disk3s5`), a network share
    /// (`//user@host/share`, `host:/export`, `\\server\share`), or a
    /// Windows drive root (`C:`).
    pub source: String,
    /// Volume name or label, on platforms that have one.
    pub volume: Option<String>,
    /// Where the volume is mounted.
    pub mount: PathBuf,
    pub file_system: String,
    pub total: u64,
    pub available: u64,
}

impl DiskInfo {
    /// Identity string for the UIs: `volume (source, fstype)`, or
    /// `source (fstype)` when the volume has no name.
    pub fn label(&self) -> String {
        match &self.volume {
            Some(volume) => format!("{volume} ({}, {})", self.source, self.file_system),
            None => format!("{} ({})", self.source, self.file_system),
        }
    }
}

/// Describes the volume hosting `path` (which must be canonicalized).
pub fn lookup(path: &Path) -> Option<DiskInfo> {
    imp::lookup(path)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::DiskInfo;

    pub fn lookup(path: &Path) -> Option<DiskInfo> {
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: all-zero bytes are a valid statfs value for the kernel to
        // overwrite.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is NUL-terminated and `fs` is a statfs-sized
        // buffer.
        if unsafe { libc::statfs(c_path.as_ptr(), &mut fs) } != 0 {
            return None;
        }
        // SAFETY: the kernel NUL-terminates the fixed-size name fields.
        let (source, mount, file_system) = unsafe {
            (
                text(fs.f_mntfromname.as_ptr()),
                text(fs.f_mntonname.as_ptr()),
                text(fs.f_fstypename.as_ptr()),
            )
        };
        Some(DiskInfo {
            source,
            volume: volume_name(&mount),
            mount: PathBuf::from(mount),
            file_system,
            total: fs.f_blocks.saturating_mul(u64::from(fs.f_bsize)),
            available: fs.f_bavail.saturating_mul(u64::from(fs.f_bsize)),
        })
    }

    /// # Safety
    /// `field` must point at a NUL-terminated C string.
    unsafe fn text(field: *const libc::c_char) -> String {
        // SAFETY: forwarded from the caller.
        unsafe { CStr::from_ptr(field) }
            .to_string_lossy()
            .into_owned()
    }

    /// The user-visible volume name ("Macintosh HD - Data", an SMB share
    /// name, …) via getattrlist's ATTR_VOL_NAME on the mount point. Network
    /// filesystems that cannot answer simply yield None.
    fn volume_name(mount: &str) -> Option<String> {
        #[repr(C)]
        struct VolName {
            length: u32,
            name: libc::attrreference_t,
            data: [u8; 512],
        }

        let c_mount = CString::new(mount).ok()?;
        let mut request = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: 0,
            volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_NAME,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        let mut reply = VolName {
            length: 0,
            name: libc::attrreference_t {
                attr_dataoffset: 0,
                attr_length: 0,
            },
            data: [0; 512],
        };
        // SAFETY: `c_mount` is NUL-terminated and `reply`'s size is passed
        // along, so the kernel cannot write past it.
        let rc = unsafe {
            libc::getattrlist(
                c_mount.as_ptr(),
                (&raw mut request).cast(),
                (&raw mut reply).cast(),
                std::mem::size_of::<VolName>(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        // attr_dataoffset is relative to the attrreference_t itself.
        let start = usize::try_from(reply.name.attr_dataoffset)
            .ok()?
            .checked_add(std::mem::offset_of!(VolName, name))?
            .checked_sub(std::mem::offset_of!(VolName, data))?;
        let rest = reply.data.get(start..)?;
        let len = (reply.name.attr_length as usize).saturating_sub(1); // drop the NUL
        let name = String::from_utf8_lossy(rest.get(..len.min(rest.len()))?).into_owned();
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::DiskInfo;

    pub fn lookup(path: &Path) -> Option<DiskInfo> {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let (source, mount, file_system) = super::deepest_mount(&mountinfo, path)?;
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: all-zero bytes are a valid statvfs value for the kernel to
        // overwrite.
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is NUL-terminated and `vfs` is a statvfs-sized
        // buffer.
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } != 0 {
            return None;
        }
        let frag = if vfs.f_frsize > 0 {
            vfs.f_frsize
        } else {
            vfs.f_bsize
        };
        Some(DiskInfo {
            source,
            volume: None,
            mount,
            file_system,
            total: vfs.f_blocks.saturating_mul(frag),
            available: vfs.f_bavail.saturating_mul(frag),
        })
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows_sys::Win32::Storage::FileSystem::{
        DRIVE_REMOTE, GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
    };

    use super::DiskInfo;

    pub fn lookup(path: &Path) -> Option<DiskInfo> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut root = [0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `root`'s length is passed
        // along.
        if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return None;
        }
        let mount = PathBuf::from(OsString::from_wide(
            &root[..root.iter().position(|&c| c == 0)?],
        ));

        let (mut available, mut total, mut free_total) = (0u64, 0u64, 0u64);
        // SAFETY: `root` is NUL-terminated and the out-pointers are valid
        // u64s.
        if unsafe {
            GetDiskFreeSpaceExW(root.as_ptr(), &mut available, &mut total, &mut free_total)
        } == 0
        {
            return None;
        }

        let mut label = [0u16; MAX_PATH as usize + 1];
        let mut fs_name = [0u16; MAX_PATH as usize + 1];
        // SAFETY: buffers and their lengths match the API contract; the
        // serial/length/flags out-params are allowed to be null.
        let have_info = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        } != 0;

        // "C:\" → "C:"; a UNC root keeps its "\\server\share" form. Mapped
        // drive letters resolve to the share behind them.
        let trimmed = mount.to_string_lossy().trim_end_matches('\\').to_owned();
        // SAFETY: `root` is NUL-terminated.
        let remote = unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE;
        let source = if remote && !trimmed.starts_with(r"\\") {
            unc_of(&trimmed).unwrap_or(trimmed)
        } else {
            trimmed
        };

        Some(DiskInfo {
            source,
            volume: have_info
                .then(|| wide_str(&label))
                .filter(|s| !s.is_empty()),
            mount,
            file_system: if have_info {
                wide_str(&fs_name)
            } else {
                "unknown".to_owned()
            },
            total,
            available,
        })
    }

    /// The `\\server\share` behind a mapped drive letter like `Z:`.
    fn unc_of(drive: &str) -> Option<String> {
        let local: Vec<u16> = drive.encode_utf16().chain([0]).collect();
        let mut remote = [0u16; 1024];
        let mut len = remote.len() as u32;
        // SAFETY: `local` is NUL-terminated; `remote`'s capacity is passed
        // via `len`.
        let rc = unsafe { WNetGetConnectionW(local.as_ptr(), remote.as_mut_ptr(), &mut len) };
        (rc == 0)
            .then(|| wide_str(&remote))
            .filter(|s| !s.is_empty())
    }

    fn wide_str(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod imp {
    use std::path::Path;

    use super::DiskInfo;

    pub fn lookup(_path: &Path) -> Option<DiskInfo> {
        None
    }
}

/// Picks the mount hosting `path` from `/proc/self/mountinfo` content: the
/// entry whose mount point is the deepest path-prefix of `path`. Returns
/// (source, mount point, fstype). Later entries win ties, since a later
/// mount on the same path shadows the earlier one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn deepest_mount(mountinfo: &str, path: &Path) -> Option<(String, PathBuf, String)> {
    let mut best: Option<(String, PathBuf, String)> = None;
    let mut best_depth = 0;
    for line in mountinfo.lines() {
        // Format: id parent major:minor root mount-point options [optional…]
        // - fstype source super-options. Spaces inside fields are
        // octal-escaped and a lone "-" terminates the optional list.
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        let (Some(mount), Some(fstype), Some(source)) =
            (fields.get(4), fields.get(dash + 1), fields.get(dash + 2))
        else {
            continue;
        };
        let mount = PathBuf::from(unescape(mount));
        let depth = mount.components().count();
        if path.starts_with(&mount) && depth >= best_depth {
            best_depth = depth;
            best = Some((unescape(source), mount, (*fstype).to_owned()));
        }
    }
    best
}

/// Reverses mountinfo's octal escapes (`\040` space, `\011` tab, …).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(char::from(byte));
                chars.nth(2);
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// "1.0 GiB"-style humanization (binary units, one decimal).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes_binary_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1 << 30), "1.0 GiB");
        assert_eq!(human_bytes(1536 << 20), "1.5 GiB");
    }

    #[test]
    fn label_shows_volume_source_and_fstype() {
        let mut info = DiskInfo {
            source: "/dev/disk3s5".into(),
            volume: Some("Macintosh HD - Data".into()),
            mount: PathBuf::from("/System/Volumes/Data"),
            file_system: "apfs".into(),
            total: 0,
            available: 0,
        };
        assert_eq!(info.label(), "Macintosh HD - Data (/dev/disk3s5, apfs)");
        info.volume = None;
        assert_eq!(info.label(), "/dev/disk3s5 (apfs)");
    }

    #[test]
    fn mountinfo_picks_deepest_mount() {
        let text = "\
36 25 8:1 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
99 36 0:52 / /mnt/data rw,relatime - nfs4 10.0.0.5:/export rw,addr=10.0.0.5
120 36 8:17 / /mnt/data/fast rw shared:5 master:1 - xfs /dev/sdb1 rw";
        let (source, mount, fstype) = deepest_mount(text, Path::new("/mnt/data/dir")).unwrap();
        assert_eq!(source, "10.0.0.5:/export");
        assert_eq!(mount, Path::new("/mnt/data"));
        assert_eq!(fstype, "nfs4");
        let (source, ..) = deepest_mount(text, Path::new("/mnt/data/fast/f")).unwrap();
        assert_eq!(source, "/dev/sdb1");
        let (source, ..) = deepest_mount(text, Path::new("/home/user")).unwrap();
        assert_eq!(source, "/dev/nvme0n1p2");
    }

    #[test]
    fn mountinfo_unescapes_spaces() {
        let text = "40 25 0:40 / /mnt/big\\040disk rw - cifs //nas/big\\040share rw";
        let (source, mount, fstype) = deepest_mount(text, Path::new("/mnt/big disk/x")).unwrap();
        assert_eq!(source, "//nas/big share");
        assert_eq!(mount, Path::new("/mnt/big disk"));
        assert_eq!(fstype, "cifs");
    }

    #[test]
    fn mountinfo_later_mount_shadows_same_path() {
        let text = "\
36 25 8:1 / /mnt rw - ext4 /dev/sda1 rw
50 36 8:2 / /mnt rw - ext4 /dev/sdb1 rw";
        let (source, ..) = deepest_mount(text, Path::new("/mnt/x")).unwrap();
        assert_eq!(source, "/dev/sdb1");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lookup_reports_a_device_backed_volume() {
        let info = lookup(Path::new("/")).expect("root volume must resolve");
        assert!(info.source.starts_with("/dev/"), "source: {}", info.source);
        assert_eq!(info.file_system, "apfs");
        assert!(info.total > 0 && info.available > 0);
    }
}
