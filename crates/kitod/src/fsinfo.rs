//! Which file system the repository lives on, and whether its events are reliable.
//!
//! On some file systems the operating system does not report (or reports only
//! in part) the changes made by another machine or by another system: this is
//! the case of `/mnt/c` under WSL for files modified by Windows programs, and
//! of network directories. There the watcher is not enough and the scan is needed.

use std::path::Path;

/// File system types on which events are unreliable.
const UNRELIABLE: &[&str] =
    &["9p", "drvfs", "cifs", "smb3", "smbfs", "nfs", "nfs4", "vboxsf", "fuse.sshfs", "fuse.vmhgfs-fuse"];

pub fn events_are_unreliable(fs_type: &str) -> bool {
    UNRELIABLE.contains(&fs_type)
}

/// Type of the file system that contains `path` (for example `ext4`, `9p`).
/// `None` if it cannot be determined (for example outside Linux).
pub fn filesystem_type(path: &Path) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    type_from_mountinfo(&mountinfo, path)
}

/// Looks in `/proc/self/mountinfo` for the deepest mount point that contains
/// `path`. Format of a line:
/// `36 35 98:0 /root /mount/point options … - type source options`.
fn type_from_mountinfo(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else { continue };
        let Some(mount_point) = before.split(' ').nth(4) else { continue };
        let Some(fs_type) = after.split(' ').next() else { continue };
        // Spaces in paths are written as \040.
        let mount_point = mount_point.replace("\\040", " ");
        if path.starts_with(&mount_point) && best.is_none_or(|(len, _)| mount_point.len() >= len) {
            best = Some((mount_point.len(), fs_type));
        }
    }
    best.map(|(_, fs_type)| fs_type.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WSL: &str = r"72 61 8:32 / / rw,relatime - ext4 /dev/sdc rw,discard,errors=remount-ro
98 72 0:28 / /mnt/c rw,noatime - 9p C:\134 rw,dirsync,aname=drvfs;path=C:\;uid=1000
99 72 0:30 / /mnt/external\040disk rw - 9p E:\134 rw
73 72 0:22 / /proc rw,nosuid - proc proc rw
malformed line without a separator
";

    #[test]
    fn picks_the_deepest_mount_that_contains_the_path() {
        let fs = |p: &str| type_from_mountinfo(WSL, Path::new(p));
        assert_eq!(fs("/home/ada/project").as_deref(), Some("ext4"));
        assert_eq!(fs("/mnt/c/Users/ada/project").as_deref(), Some("9p"));
        assert_eq!(fs("/mnt/external disk/x").as_deref(), Some("9p"));
        assert_eq!(fs("/mnt/cache").as_deref(), Some("ext4"), "/mnt/c is not a prefix of /mnt/cache");
        assert_eq!(type_from_mountinfo("", Path::new("/x")), None);
    }

    #[test]
    fn knows_which_filesystems_drop_events() {
        assert!(
            events_are_unreliable("9p") && events_are_unreliable("nfs4") && events_are_unreliable("cifs")
        );
        assert!(
            !events_are_unreliable("ext4")
                && !events_are_unreliable("apfs")
                && !events_are_unreliable("btrfs")
        );
    }

    #[test]
    fn works_on_this_machine() {
        // On Linux the type is always found; elsewhere the function says "don't know".
        let found = filesystem_type(&std::env::temp_dir());
        if cfg!(target_os = "linux") {
            assert!(found.is_some());
        }
    }
}
