//! The USB drive of the UF2 bootloader, to which a UF2 file is copied to
//! flash a board: found under /Volumes on macOS and in /proc/mounts on
//! Linux, where udisksctl mounts it when nothing did.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::uf2::INFO_FILE;

/// Returns the mount point of the first drive named one of names that holds
/// INFO_UF2.TXT.
pub fn find(names: &[&str]) -> Option<PathBuf> {
    let mounts: Vec<PathBuf> = if cfg!(target_os = "macos") {
        names.iter().map(|n| Path::new("/Volumes").join(n)).collect()
    } else {
        fs::read_to_string("/proc/mounts")
            .map(|m| fat_mounts(&m, names))
            .unwrap_or_default()
    };
    mounts.into_iter().find(|m| m.join(INFO_FILE).exists())
}

/// Mounts a drive named one of names with udisksctl, on Linux, and returns
/// where. It returns None when there is no such drive, or on macOS, which
/// mounts drives itself.
pub fn mount(names: &[&str]) -> Result<Option<PathBuf>, String> {
    if !cfg!(target_os = "linux") {
        return Ok(None);
    }
    for name in names {
        let dev = Path::new("/dev/disk/by-label").join(name);
        if !dev.exists() {
            continue;
        }
        let out = Command::new("udisksctl")
            .args(["mount", "--no-user-interaction", "-b"])
            .arg(&dev)
            .output()
            .map_err(|e| format!("udisksctl: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("AlreadyMounted") {
            return Ok(find(names));
        }
        if !out.status.success() {
            return Err(format!("udisksctl mount {}: {}", dev.display(), err.trim()));
        }
        // "Mounted /dev/sdb at /media/user/FTHR840BOOT"
        if let Some((_, at)) = text.trim().split_once(" at ") {
            return Ok(Some(PathBuf::from(at.trim_end_matches('.'))));
        }
        return Ok(find(names));
    }
    Ok(None)
}

/// Returns the mount points of the FAT file systems in a /proc/mounts text
/// whose directory is named one of names.
pub fn fat_mounts(mounts: &str, names: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in mounts.lines() {
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        if f.len() < 3 || (f[2] != "vfat" && f[2] != "msdos") {
            continue;
        }
        let dir = PathBuf::from(unescape(f[1]));
        if dir.file_name().is_some_and(|b| names.iter().any(|n| b == *n)) {
            out.push(dir);
        }
    }
    out
}

/// Undoes the octal escapes of /proc/mounts, such as \040 for a space.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..=i + 3].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') << 6 | (b[i + 2] - b'0') << 3 | (b[i + 3] - b'0'));
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Returns the Board-ID line of a mounted bootloader drive.
pub fn board_id(mount: &Path) -> Option<String> {
    let info = fs::read_to_string(mount.join(INFO_FILE)).ok()?;
    info.lines()
        .find_map(|l| l.trim().strip_prefix("Board-ID:").map(|id| id.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat_mounts_by_name() {
        let mounts = "\
/dev/sda1 / ext4 rw 0 0
/dev/sdb /media/me/FTHR840BOOT vfat rw,nosuid 0 0
/dev/sdc /media/me/My\\040Drive vfat rw 0 0
/dev/sdd /mnt/FTHR840BOOT ext4 rw 0 0
";
        assert_eq!(
            fat_mounts(mounts, &["FTHR840BOOT"]),
            vec![PathBuf::from("/media/me/FTHR840BOOT")]
        );
        assert_eq!(
            fat_mounts(mounts, &["My Drive"]),
            vec![PathBuf::from("/media/me/My Drive")]
        );
        assert!(fat_mounts(mounts, &["NONE"]).is_empty());
    }

    #[test]
    fn board_id_line() {
        let dir = std::env::temp_dir().join(format!("knixx-drive-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(INFO_FILE),
            "UF2 Bootloader 0.6.0 lib/nrfx (v2.0.0)\r\nModel: Adafruit Feather nRF52840 Express\r\nBoard-ID: nRF52840-Feather-revD\r\n",
        )
        .unwrap();
        assert_eq!(board_id(&dir).as_deref(), Some("nRF52840-Feather-revD"));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(board_id(&dir), None);
    }
}
