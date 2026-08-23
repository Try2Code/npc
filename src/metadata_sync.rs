//! Catching a RAW and its JPG rated or tagged differently.
//!
//! `scan` reconciles a stack's RAW-embedded and JPG-embedded rating and tags
//! only once, when the stack is first cataloged (`scan::combine_rating`,
//! `scan::union_embedded_tags`). After that the merged values live only in
//! the catalog, so rating or tagging the RAW in one tool and the JPG in
//! another -- at different times, in different applications -- drifts apart
//! with nothing to notice. This module re-reads both originals live and
//! reports or resolves the difference; `doctor` uses the report, `npc
//! rate-sync` uses the resolution.
//!
//! Ratings and tags are treated differently on purpose. A rating is a single
//! scalar: two different explicit values from two tools is a genuine
//! disagreement. Tags are a set that only ever grows by union: two tools
//! contributing *different* tags isn't a contradiction, so there is no
//! tags-disagree concept here -- differing tags just merge. The one thing
//! both share is "one side carries a value, the other has none at all" --
//! not a conflict (there's nothing to disagree about, and the catalog's
//! resolved value is already correct), but worth being visible rather than
//! looking identical to "everything already agrees."
//!
//! Nothing here writes. Resolution is handed to the caller, who routes it
//! through the ordinary `edit::apply`/`edit::flush` path.

use crate::db::Catalog;
use crate::meta;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub stack_id: i64,
    pub dir: String,
    pub base: String,
    /// The RAW's own embedded rating, read live off the file.
    pub raw_rating: Option<i32>,
    /// The JPG's own embedded rating, read live off the file.
    pub jpg_rating: Option<i32>,
    /// What `scan::combine_rating` resolves `raw_rating`/`jpg_rating` to.
    pub resolved_rating: Option<i32>,
    /// The catalog's current merged rating for this stack.
    pub catalog_rating: Option<i64>,
    /// The RAW's own embedded tags, read live off the file.
    pub raw_tags: Vec<String>,
    /// The JPG's own embedded tags, read live off the file.
    pub jpg_tags: Vec<String>,
    /// The union of catalog tags, `raw_tags` and `jpg_tags`.
    pub resolved_tags: Vec<String>,
    /// The catalog's current tags for this stack.
    pub catalog_tags: Vec<String>,
}

impl Comparison {
    /// The two originals currently disagree -- both carry a rating, and it's
    /// not the same one.
    pub fn rating_disagrees(&self) -> bool {
        matches!((self.raw_rating, self.jpg_rating), (Some(a), Some(b)) if a != b)
    }

    /// Exactly one side carries a rating at all. Not a conflict -- there's
    /// nothing to disagree about, and `combine_rating` already resolves this
    /// to the one value that exists -- just worth being visible.
    pub fn rating_one_sided(&self) -> bool {
        self.raw_rating.is_some() != self.jpg_rating.is_some()
    }

    /// Exactly one side carries any embedded tags at all.
    pub fn tags_one_sided(&self) -> bool {
        self.raw_tags.is_empty() != self.jpg_tags.is_empty()
    }

    pub fn one_sided(&self) -> bool {
        self.rating_one_sided() || self.tags_one_sided()
    }

    /// There is a resolved rating, and it differs from what the catalog
    /// holds today. `false` whenever neither original carries an embedded
    /// rating, even if the catalog has one -- a rating set directly through
    /// `npc rate` has no embedded counterpart to resync from, and must never
    /// be clobbered back to unset.
    pub fn needs_rating_write(&self) -> bool {
        self.resolved_rating
            .is_some_and(|r| Some(i64::from(r)) != self.catalog_rating)
    }

    /// Tags the resolved union carries that the catalog doesn't yet -- never
    /// tags the catalog has that the union doesn't, since a union can only
    /// grow.
    pub fn added_tags(&self) -> Vec<String> {
        self.resolved_tags
            .iter()
            .filter(|t| !self.catalog_tags.iter().any(|c| c.eq_ignore_ascii_case(t)))
            .cloned()
            .collect()
    }

    pub fn needs_tags_write(&self) -> bool {
        !self.added_tags().is_empty()
    }

    pub fn needs_write(&self) -> bool {
        self.needs_rating_write() || self.needs_tags_write()
    }
}

/// Read a role's own embedded rating and tags in one `read_meta` call --
/// deliberately not two separate reads, so this costs no more Exiv2 calls
/// than a rating-only check would.
fn read_role_meta(dir: &str, name: &str) -> (Option<i32>, Vec<String>) {
    let path = format!("{dir}/{name}");
    match meta::read_meta(&path) {
        Ok(m) => ((m.rating >= 0).then_some(m.rating), m.tags),
        Err(_) => (None, Vec::new()),
    }
}

/// Every stack's current tags, fetched in one bulk query rather than one
/// query per stack inside the parallel section below -- `rusqlite::Connection`
/// access has to stay sequential.
fn all_stack_tags(cat: &Catalog) -> Result<HashMap<i64, Vec<String>>> {
    let mut stmt = cat
        .conn
        .prepare("SELECT st.stack_id, t.name FROM stack_tag st JOIN tag t ON t.id = st.tag_id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let (sid, name) = row?;
        map.entry(sid).or_default().push(name);
    }
    Ok(map)
}

/// Every stack that currently has both a present RAW and a present JPG,
/// with each role's rating and tags read live -- not from the catalog,
/// which only ever reflects the one-shot seed at first scan.
pub fn scan(cat: &Catalog) -> Result<Vec<Comparison>> {
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

    let tags_by_stack = all_stack_tags(cat)?;

    Ok(rows
        .into_par_iter()
        .map(
            |(stack_id, dir, base, catalog_rating, raw_name, jpg_name)| {
                let (raw_rating, raw_tags) = read_role_meta(&dir, &raw_name);
                let (jpg_rating, jpg_tags) = read_role_meta(&dir, &jpg_name);
                let resolved_rating = crate::scan::combine_rating(raw_rating, jpg_rating);
                let catalog_tags = tags_by_stack.get(&stack_id).cloned().unwrap_or_default();
                let resolved_tags = crate::scan::union_embedded_tags(
                    [
                        catalog_tags.as_slice(),
                        raw_tags.as_slice(),
                        jpg_tags.as_slice(),
                    ]
                    .into_iter(),
                );
                Comparison {
                    stack_id,
                    dir,
                    base,
                    raw_rating,
                    jpg_rating,
                    resolved_rating,
                    catalog_rating,
                    raw_tags,
                    jpg_tags,
                    resolved_tags,
                    catalog_tags,
                }
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comparison(
        raw_rating: Option<i32>,
        jpg_rating: Option<i32>,
        catalog_rating: Option<i64>,
        raw_tags: Vec<String>,
        jpg_tags: Vec<String>,
        catalog_tags: Vec<String>,
    ) -> Comparison {
        let resolved_tags = crate::scan::union_embedded_tags(
            [
                catalog_tags.as_slice(),
                raw_tags.as_slice(),
                jpg_tags.as_slice(),
            ]
            .into_iter(),
        );
        Comparison {
            stack_id: 1,
            dir: "/d".into(),
            base: "750_0001".into(),
            raw_rating,
            jpg_rating,
            resolved_rating: crate::scan::combine_rating(raw_rating, jpg_rating),
            catalog_rating,
            raw_tags,
            jpg_tags,
            resolved_tags,
            catalog_tags,
        }
    }

    fn rating_only(raw: Option<i32>, jpg: Option<i32>, catalog: Option<i64>) -> Comparison {
        comparison(raw, jpg, catalog, vec![], vec![], vec![])
    }

    #[test]
    fn rating_disagrees_only_when_both_present_and_unequal() {
        assert!(rating_only(Some(3), Some(5), None).rating_disagrees());
        assert!(!rating_only(Some(3), Some(3), None).rating_disagrees());
        assert!(!rating_only(Some(3), None, None).rating_disagrees());
        assert!(!rating_only(None, None, None).rating_disagrees());
    }

    #[test]
    fn needs_rating_write_when_resolved_differs_from_catalog() {
        let c = rating_only(Some(3), Some(5), Some(3));
        assert!(c.needs_rating_write());
        assert_eq!(c.resolved_rating, Some(5));
    }

    #[test]
    fn needs_rating_write_is_false_when_already_resolved_in_catalog() {
        assert!(!rating_only(Some(3), Some(5), Some(5)).needs_rating_write());
    }

    /// Neither original carries an embedded rating: a catalog rating set
    /// directly through `npc rate` has nothing to resync from and must
    /// survive untouched, not be clobbered back to unset.
    #[test]
    fn needs_rating_write_is_false_when_resolved_is_none_even_with_a_catalog_rating() {
        assert!(!rating_only(None, None, Some(4)).needs_rating_write());
    }

    #[test]
    fn rating_one_sided_when_exactly_one_side_has_a_rating() {
        assert!(rating_only(Some(3), None, None).rating_one_sided());
        assert!(rating_only(None, Some(3), None).rating_one_sided());
        assert!(!rating_only(Some(3), Some(5), None).rating_one_sided());
        assert!(!rating_only(None, None, None).rating_one_sided());
    }

    #[test]
    fn tags_one_sided_when_exactly_one_side_has_tags() {
        let c = comparison(None, None, None, vec!["a".into()], vec![], vec![]);
        assert!(c.tags_one_sided());
        let c = comparison(None, None, None, vec![], vec!["a".into()], vec![]);
        assert!(c.tags_one_sided());
    }

    /// Both sides carrying tags -- even different ones -- is not one-sided:
    /// there's no asymmetry, just two contributions to union.
    #[test]
    fn tags_are_not_one_sided_when_both_sides_have_any() {
        let c = comparison(
            None,
            None,
            None,
            vec!["sailing".into()],
            vec!["family".into()],
            vec![],
        );
        assert!(!c.tags_one_sided());
    }

    #[test]
    fn one_sided_is_false_when_everything_is_symmetric() {
        let c = comparison(
            Some(3),
            Some(3),
            Some(3),
            vec!["a".into()],
            vec!["a".into()],
            vec!["a".into()],
        );
        assert!(!c.one_sided());
        assert!(!comparison(None, None, None, vec![], vec![], vec![]).one_sided());
    }

    #[test]
    fn added_tags_is_case_insensitive_against_the_catalog() {
        let c = comparison(
            None,
            None,
            None,
            vec!["Berlin".into()],
            vec!["berlin".into(), "india".into()],
            vec![],
        );
        let mut added = c.added_tags();
        added.sort();
        assert_eq!(added, vec!["Berlin".to_string(), "india".to_string()]);
    }

    #[test]
    fn needs_tags_write_is_false_when_catalog_already_has_everything() {
        let c = comparison(
            None,
            None,
            None,
            vec!["a".into()],
            vec!["b".into()],
            vec!["a".into(), "b".into()],
        );
        assert!(!c.needs_tags_write());
    }

    #[test]
    fn needs_write_is_true_when_only_tags_changed() {
        let c = comparison(
            Some(4),
            Some(4),
            Some(4),
            vec![],
            vec!["new".into()],
            vec![],
        );
        assert!(!c.needs_rating_write());
        assert!(c.needs_tags_write());
        assert!(c.needs_write());
    }
}
