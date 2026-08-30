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
    /// Whether this stack currently has a present RAW/JPG member at all --
    /// distinct from that member's rating/tags being empty. `one_sided`
    /// means nothing for a stack that never had a sibling to disagree with
    /// (most of the library), so it's gated on both being present.
    pub raw_present: bool,
    pub jpg_present: bool,
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

    /// Both sides exist, but exactly one carries a rating at all. Not a
    /// conflict -- there's nothing to disagree about, and `combine_rating`
    /// already resolves this to the one value that exists -- just worth
    /// being visible. False whenever a sibling is simply absent (most of
    /// the library): there's no asymmetry between a file and a file that
    /// was never there.
    pub fn rating_one_sided(&self) -> bool {
        self.raw_present
            && self.jpg_present
            && self.raw_rating.is_some() != self.jpg_rating.is_some()
    }

    /// Both sides exist, but exactly one carries any embedded tags at all.
    pub fn tags_one_sided(&self) -> bool {
        self.raw_present && self.jpg_present && self.raw_tags.is_empty() != self.jpg_tags.is_empty()
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
fn read_role_meta(dir: &str, name: Option<&str>) -> (Option<i32>, Vec<String>) {
    let Some(name) = name else {
        return (None, Vec::new());
    };
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

type ScanRow = (
    i64,
    String,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
);

/// Every stack, with whichever of its RAW/JPG members are actually present
/// read live -- not from the catalog, which only ever reflects the one-shot
/// seed at first scan. A stack missing one role (most of the library: a JPG
/// with no surviving RAW, or vice versa) still gets its one present side
/// checked -- an inner join on both roles used to silently skip these
/// entirely, so a tag added after import to a RAW-less JPG (or the reverse)
/// was never picked up by `rate-sync` no matter how often it ran.
pub fn scan(cat: &Catalog) -> Result<Vec<Comparison>> {
    let mut stmt = cat.conn.prepare(
        "SELECT s.id, s.dir, s.base, s.rating, r.name, j.name
         FROM stack s
         LEFT JOIN file r ON r.stack_id = s.id AND r.role = 'raw'    AND r.present = 1
         LEFT JOIN file j ON j.stack_id = s.id AND j.role = 'oocjpg' AND j.present = 1",
    )?;
    let rows: Vec<ScanRow> = stmt
        .query_map([], row_to_scan_row)?
        .collect::<std::result::Result<_, _>>()?;
    build_comparisons(cat, rows)
}

/// Same as `scan`, but only for the given stack ids -- each of which costs a
/// live Exiv2 read of its present member(s). `rate-sync` targets one stack or
/// a handful via `-q`/explicit paths; running the full-library query first
/// and filtering after, as this used to, meant checking one file re-read
/// every original in the library first.
pub fn scan_ids(cat: &Catalog, ids: &[i64]) -> Result<Vec<Comparison>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT s.id, s.dir, s.base, s.rating, r.name, j.name
         FROM stack s
         LEFT JOIN file r ON r.stack_id = s.id AND r.role = 'raw'    AND r.present = 1
         LEFT JOIN file j ON j.stack_id = s.id AND j.role = 'oocjpg' AND j.present = 1
         WHERE s.id IN ({placeholders})"
    );
    let mut stmt = cat.conn.prepare(&sql)?;
    let rows: Vec<ScanRow> = stmt
        .query_map(rusqlite::params_from_iter(ids.iter()), row_to_scan_row)?
        .collect::<std::result::Result<_, _>>()?;
    build_comparisons(cat, rows)
}

fn row_to_scan_row(r: &rusqlite::Row) -> rusqlite::Result<ScanRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

fn build_comparisons(cat: &Catalog, rows: Vec<ScanRow>) -> Result<Vec<Comparison>> {
    let tags_by_stack = all_stack_tags(cat)?;

    Ok(rows
        .into_par_iter()
        .map(
            |(stack_id, dir, base, catalog_rating, raw_name, jpg_name)| {
                let (raw_present, jpg_present) = (raw_name.is_some(), jpg_name.is_some());
                let (raw_rating, raw_tags) = read_role_meta(&dir, raw_name.as_deref());
                let (jpg_rating, jpg_tags) = read_role_meta(&dir, jpg_name.as_deref());
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
                    raw_present,
                    jpg_present,
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
            raw_present: true,
            jpg_present: true,
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

    /// A stack with no RAW at all (the common case: most of the library is
    /// JPG-only) -- built directly rather than through `comparison()`, whose
    /// `raw_present: true` would defeat the point of this fixture.
    fn jpg_only(
        jpg_rating: Option<i32>,
        catalog_rating: Option<i64>,
        jpg_tags: Vec<String>,
        catalog_tags: Vec<String>,
    ) -> Comparison {
        let resolved_tags = crate::scan::union_embedded_tags(
            [catalog_tags.as_slice(), jpg_tags.as_slice()].into_iter(),
        );
        Comparison {
            stack_id: 1,
            dir: "/d".into(),
            base: "750_0001".into(),
            raw_present: false,
            jpg_present: true,
            raw_rating: None,
            jpg_rating,
            resolved_rating: crate::scan::combine_rating(None, jpg_rating),
            catalog_rating,
            raw_tags: vec![],
            jpg_tags,
            resolved_tags,
            catalog_tags,
        }
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

    /// A JPG-only stack's tags are, by definition, only ever on the JPG --
    /// that used to make `one_sided()` fire for nearly every tagged or rated
    /// stack in a library that's mostly JPG-only, which would drown doctor's
    /// actual signal (a genuine RAW/JPG asymmetry) in noise about stacks that
    /// never had a RAW to be asymmetric with.
    #[test]
    fn one_sided_is_false_for_a_role_that_was_never_present() {
        let c = jpg_only(Some(4), Some(4), vec!["a".into()], vec!["a".into()]);
        assert!(!c.rating_one_sided());
        assert!(!c.tags_one_sided());
        assert!(!c.one_sided());
    }

    /// The whole point of widening `scan` past "both roles present": a
    /// brand-new tag embedded in the only file a stack has must still be
    /// picked up, even with no sibling to compare it against.
    #[test]
    fn needs_write_still_fires_with_no_sibling_to_compare_against() {
        let c = jpg_only(None, None, vec!["Radisson".into()], vec![]);
        assert_eq!(c.added_tags(), vec!["Radisson".to_string()]);
        assert!(c.needs_tags_write());
        assert!(c.needs_write());
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
