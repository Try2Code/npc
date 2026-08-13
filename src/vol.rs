//! Identifying which physical volume a path lives on.
//!
//! The catalog is keyed by filesystem UUID and kept on the laptop, so the
//! library stays queryable when the drive is unplugged. That only works if a
//! volume can be recognised as the same volume across mounts, which is what
//! this resolves.

use anyhow::{Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Volume {
    /// Filesystem UUID where available, else `dev:MAJOR:MINOR`. Names the
    /// catalog file.
    pub uuid: String,
    pub label: Option<String>,
    pub mount: PathBuf,
    pub device: Option<String>,
    /// False when we fell back to a device number, which is not stable across
    /// reboots or replugs. Worth telling the user about.
    pub stable: bool,
}

/// Resolve the volume containing `path`.
pub fn identify(path: &Path) -> Result<Volume> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    let dev = std::fs::metadata(&canonical)
        .with_context(|| format!("stat {}", canonical.display()))?
        .dev();

    let major = rustix::fs::major(dev);
    let minor = rustix::fs::minor(dev);

    let (mount, device) = mount_for_dev(major, minor).unwrap_or_else(|| {
        // Fall back to walking up until the device number changes.
        (mount_point_by_walking(&canonical), None)
    });

    let by_uuid = device.as_deref().and_then(lookup_uuid);
    let label = device.as_deref().and_then(lookup_label);

    let (uuid, stable) = match by_uuid {
        Some(u) => (u, true),
        None => (format!("dev:{major}:{minor}"), false),
    };

    Ok(Volume {
        uuid,
        label,
        mount,
        device,
        stable,
    })
}

/// Find the mount point and backing device for a device number, from
/// /proc/self/mountinfo. Fields: id parent major:minor root mountpoint ... - fstype source ...
fn mount_for_dev(major: u32, minor: u32) -> Option<(PathBuf, Option<String>)> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let want = format!("{major}:{minor}");

    let mut best: Option<(PathBuf, Option<String>)> = None;
    for line in text.lines() {
        let mut parts = line.split(' ');
        let _id = parts.next()?;
        let _parent = parts.next()?;
        let devno = parts.next()?;
        let _root = parts.next()?;
        let mountpoint = parts.next()?;
        if devno != want {
            continue;
        }
        let source = line
            .split(" - ")
            .nth(1)
            .and_then(|tail| tail.split(' ').nth(1))
            .map(unescape);
        // Later entries shadow earlier ones for the same device; keep the last.
        best = Some((PathBuf::from(unescape(mountpoint)), source));
    }
    best
}

fn mount_point_by_walking(path: &Path) -> PathBuf {
    let dev_of = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
    let Some(start) = dev_of(path) else {
        return path.to_path_buf();
    };
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent() {
        match dev_of(parent) {
            Some(d) if d == start => current = parent.to_path_buf(),
            _ => break,
        }
    }
    current
}

fn lookup_uuid(device: &str) -> Option<String> {
    lookup_in("/dev/disk/by-uuid", device)
}

fn lookup_label(device: &str) -> Option<String> {
    lookup_in("/dev/disk/by-label", device)
}

fn lookup_in(dir: &str, device: &str) -> Option<String> {
    let target = Path::new(device).canonicalize().ok()?;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if entry.path().canonicalize().ok().as_deref() == Some(target.as_path()) {
            return entry
                .file_name()
                .to_str()
                .map(std::string::ToString::to_string);
        }
    }
    None
}

/// mountinfo escapes space, tab, newline and backslash as octal.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_mountinfo_octal() {
        assert_eq!(unescape(r"/media/my\040drive"), "/media/my drive");
        assert_eq!(unescape("/plain/path"), "/plain/path");
        assert_eq!(unescape(r"/tab\011here"), "/tab\there");
    }

    #[test]
    fn identifies_the_volume_holding_a_real_path() {
        let v = identify(Path::new(".")).expect("current dir must resolve");
        assert!(!v.uuid.is_empty());
        assert!(v.mount.is_absolute());
    }

    /// Two paths on the same filesystem must resolve to the same volume, or
    /// the catalog would fragment across directories.
    #[test]
    fn same_filesystem_gives_same_uuid() {
        let a = identify(Path::new("/")).unwrap();
        let b = identify(Path::new("/etc")).unwrap();
        assert_eq!(a.uuid, b.uuid);
    }
}
