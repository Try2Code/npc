//! Catching a RAW and its JPG rated differently.
//!
//! `scan` reconciles a stack's RAW-embedded and JPG-embedded ratings only
//! once, when the stack is first cataloged (`scan::combine_rating`, the
//! higher of the two). After that the merged value lives only in
//! `stack.rating`, so rating the RAW in one tool and the JPG in another --
//! at different times, in different applications -- drifts apart with
//! nothing to notice. This module re-reads both originals live and reports
//! or resolves the disagreement; `doctor` uses the report, `npc rate-sync`
//! uses the resolution.
//!
//! Nothing here writes. Resolution is handed to the caller, who routes it
//! through the ordinary `edit::apply`/`edit::flush` path.

use crate::db::Catalog;
use crate::meta;
use anyhow::Result;
use rayon::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub stack_id: i64,
    pub dir: String,
    pub base: String,
    /// The RAW's own embedded rating, read live off the file.
    pub raw: Option<i32>,
    /// The JPG's own embedded rating, read live off the file.
    pub jpg: Option<i32>,
    /// What `scan::combine_rating` resolves `raw`/`jpg` to.
    pub resolved: Option<i32>,
    /// The catalog's current merged rating for this stack.
    pub catalog: Option<i64>,
}

impl Conflict {
    /// The two originals currently disagree -- both carry a rating, and it's
    /// not the same one.
    pub fn disagrees(&self) -> bool {
        matches!((self.raw, self.jpg), (Some(a), Some(b)) if a != b)
    }

    /// There is a resolved value, and it differs from what the catalog holds
    /// today. `false` whenever neither original carries an embedded rating,
    /// even if the catalog has one -- a rating set directly through `npc
    /// rate` has no embedded counterpart to resync from, and must never be
    /// clobbered back to unset.
    pub fn needs_write(&self) -> bool {
        self.resolved
            .is_some_and(|r| Some(i64::from(r)) != self.catalog)
    }
}

fn read_role_rating(dir: &str, name: &str) -> Option<i32> {
    let path = format!("{dir}/{name}");
    meta::read_meta(&path)
        .ok()
        .and_then(|m| (m.rating >= 0).then_some(m.rating))
}

/// Every stack that currently has both a present RAW and a present JPG,
/// with each role's rating read live -- not from the catalog, which only
/// ever reflects the one-shot seed at first scan.
pub fn scan(cat: &Catalog) -> Result<Vec<Conflict>> {
    let mut stmt = cat.conn.prepare(
        "SELECT s.id, s.dir, s.base, s.rating, r.name, j.name
         FROM stack s
         JOIN file r ON r.stack_id = s.id AND r.role = 'raw'    AND r.present = 1
         JOIN file j ON j.stack_id = s.id AND j.role = 'oocjpg' AND j.present = 1",
    )?;
    let rows: Vec<(i64, String, String, Option<i64>, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;

    Ok(rows
        .into_par_iter()
        .map(|(stack_id, dir, base, catalog, raw_name, jpg_name)| {
            let raw = read_role_rating(&dir, &raw_name);
            let jpg = read_role_rating(&dir, &jpg_name);
            let resolved = crate::scan::combine_rating(raw, jpg);
            Conflict {
                stack_id,
                dir,
                base,
                raw,
                jpg,
                resolved,
                catalog,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(raw: Option<i32>, jpg: Option<i32>, catalog: Option<i64>) -> Conflict {
        Conflict {
            stack_id: 1,
            dir: "/d".into(),
            base: "750_0001".into(),
            raw,
            jpg,
            resolved: crate::scan::combine_rating(raw, jpg),
            catalog,
        }
    }

    #[test]
    fn disagrees_only_when_both_present_and_unequal() {
        assert!(conflict(Some(3), Some(5), None).disagrees());
        assert!(!conflict(Some(3), Some(3), None).disagrees());
        assert!(!conflict(Some(3), None, None).disagrees());
        assert!(!conflict(None, None, None).disagrees());
    }

    #[test]
    fn needs_write_when_resolved_differs_from_catalog() {
        let c = conflict(Some(3), Some(5), Some(3));
        assert!(c.needs_write());
        assert_eq!(c.resolved, Some(5));
    }

    #[test]
    fn needs_write_is_false_when_already_resolved_in_catalog() {
        assert!(!conflict(Some(3), Some(5), Some(5)).needs_write());
    }

    /// Neither original carries an embedded rating: a catalog rating set
    /// directly through `npc rate` has nothing to resync from and must
    /// survive untouched, not be clobbered back to unset.
    #[test]
    fn needs_write_is_false_when_resolved_is_none_even_with_a_catalog_rating() {
        assert!(!conflict(None, None, Some(4)).needs_write());
    }
}
