//! Detecting mounted SD cards for interactive import.
//!
//! A card is any mounted filesystem with a top-level `DCIM` directory -- the
//! universal camera convention. This needs no removable-media or USB
//! heuristics, and won't misfire on the root filesystem or a network mount,
//! since those don't have one.

use crate::vol;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Card {
    pub mount: PathBuf,
    pub label: Option<String>,
}

/// Every mounted card found right now.
pub fn detect() -> Result<Vec<Card>> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(candidate_mounts(&text, has_dcim)
        .into_iter()
        .map(|mount| {
            let label = vol::identify(&mount).ok().and_then(|v| v.label);
            Card { mount, label }
        })
        .collect())
}

fn has_dcim(mount: &Path) -> bool {
    std::fs::read_dir(mount)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_type().is_ok_and(|t| t.is_dir())
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case("dcim"))
        })
}

/// Mount points with a top-level `DCIM` folder, in mountinfo order,
/// deduplicated. `has_dcim` is injected so this is testable without real
/// mounts.
fn candidate_mounts(text: &str, has_dcim: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    vol::parse_mountinfo(text)
        .into_iter()
        .map(|(mount, _, _, _)| mount)
        .filter(|m| seen.insert(m.clone()))
        .filter(|m| has_dcim(m))
        .collect()
}

/// Parse a card-selection line: comma/space separated 1-based numbers, or
/// `all`. Returns 0-based indices, in the order given, deduplicated. Empty
/// input (just pressing enter) means all cards -- importing every card in
/// the reader is the usual case. `q` or `none` cancels and returns an
/// empty, non-error selection.
pub fn parse_selection(input: &str, n: usize) -> Result<Vec<usize>> {
    let input = input.trim();
    if input.is_empty() || input.eq_ignore_ascii_case("all") {
        return Ok((0..n).collect());
    }
    if input.eq_ignore_ascii_case("q") || input.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for tok in input.split([',', ' ']).filter(|t| !t.is_empty()) {
        let Ok(i) = tok.parse::<usize>() else {
            bail!("not a number: {tok}");
        };
        if i == 0 || i > n {
            bail!("{i} is out of range (1-{n})");
        }
        let idx = i - 1;
        if !out.contains(&idx) {
            out.push(idx);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
25 30 8:17 / /media/ram/NIKON rw,nosuid,nodev,relatime shared:300 - vfat /dev/sdb1 rw,fmask=0022
26 30 8:33 / /media/ram/BACKUP rw,relatime shared:301 - ext4 /dev/sdc1 rw
27 30 0:23 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
";

    #[test]
    fn candidates_are_filtered_to_mounts_with_a_dcim_folder() {
        let found = candidate_mounts(MOUNTINFO, |m| m == Path::new("/media/ram/NIKON"));
        assert_eq!(found, vec![PathBuf::from("/media/ram/NIKON")]);
    }

    #[test]
    fn no_mount_has_a_dcim_folder_is_an_empty_list() {
        assert!(candidate_mounts(MOUNTINFO, |_| false).is_empty());
    }

    #[test]
    fn numbers_select_by_one_based_index() {
        assert_eq!(parse_selection("1,3", 3).unwrap(), vec![0, 2]);
        assert_eq!(parse_selection("2", 3).unwrap(), vec![1]);
        assert_eq!(parse_selection(" 1  2 ", 3).unwrap(), vec![0, 1]);
    }

    #[test]
    fn all_selects_everything() {
        assert_eq!(parse_selection("all", 4).unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_selection("ALL", 4).unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        assert!(parse_selection("0", 3).is_err());
        assert!(parse_selection("4", 3).is_err());
    }

    #[test]
    fn garbage_input_is_rejected_with_a_useful_message() {
        let err = parse_selection("banana", 3).unwrap_err();
        assert!(err.to_string().contains("banana"));
    }

    #[test]
    fn empty_input_selects_every_card() {
        assert_eq!(parse_selection("", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_selection("  \n", 2).unwrap(), vec![0, 1]);
    }

    #[test]
    fn q_or_none_cancels() {
        assert_eq!(parse_selection("q", 3).unwrap(), Vec::<usize>::new());
        assert_eq!(parse_selection(" None ", 3).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn duplicate_indices_are_deduped_preserving_order() {
        assert_eq!(parse_selection("2,2,1", 3).unwrap(), vec![1, 0]);
    }
}
