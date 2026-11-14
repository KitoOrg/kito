//! Su quale file system sta il repository, e se i suoi eventi sono affidabili.
//!
//! Su alcuni file system il sistema operativo non segnala (o segnala solo in
//! parte) le modifiche fatte da un'altra macchina o da un altro sistema: è il
//! caso di `/mnt/c` sotto WSL per i file modificati da programmi Windows, e
//! delle cartelle di rete. Lì il watcher non basta e serve la scansione.

use std::path::Path;

/// Tipi di file system su cui gli eventi non sono affidabili.
const UNRELIABLE: &[&str] =
    &["9p", "drvfs", "cifs", "smb3", "smbfs", "nfs", "nfs4", "vboxsf", "fuse.sshfs", "fuse.vmhgfs-fuse"];

pub fn events_are_unreliable(fs_type: &str) -> bool {
    UNRELIABLE.contains(&fs_type)
}

/// Tipo del file system che contiene `path` (per esempio `ext4`, `9p`).
/// `None` se non si riesce a determinarlo (per esempio fuori da Linux).
pub fn filesystem_type(path: &Path) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    type_from_mountinfo(&mountinfo, path)
}

/// Cerca in `/proc/self/mountinfo` il punto di mount più profondo che
/// contiene `path`. Formato di una riga:
/// `36 35 98:0 /radice /punto/di/mount opzioni … - tipo sorgente opzioni`.
fn type_from_mountinfo(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else { continue };
        let Some(mount_point) = before.split(' ').nth(4) else { continue };
        let Some(fs_type) = after.split(' ').next() else { continue };
        // Gli spazi nei percorsi sono scritti come \040.
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
99 72 0:30 / /mnt/disco\040esterno rw - 9p E:\134 rw
73 72 0:22 / /proc rw,nosuid - proc proc rw
riga malformata senza separatore
";

    #[test]
    fn picks_the_deepest_mount_that_contains_the_path() {
        let fs = |p: &str| type_from_mountinfo(WSL, Path::new(p));
        assert_eq!(fs("/home/fra/progetto").as_deref(), Some("ext4"));
        assert_eq!(fs("/mnt/c/Users/fra/progetto").as_deref(), Some("9p"));
        assert_eq!(fs("/mnt/disco esterno/x").as_deref(), Some("9p"));
        assert_eq!(fs("/mnt/cartella").as_deref(), Some("ext4"), "/mnt/c non è un prefisso di /mnt/cartella");
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
        // Su Linux il tipo si trova sempre; altrove la funzione dice "non so".
        let found = filesystem_type(&std::env::temp_dir());
        if cfg!(target_os = "linux") {
            assert!(found.is_some());
        }
    }
}
