//! Reconciling a library file with an SD-card copy of the same shot.
//!
//! Importing from a card sometimes finds a file that is the same photograph
//! as one already cataloged but not byte-identical, because something else
//! (Shotwell, typically) embedded a rating or tags into it after it was first
//! imported. That's not a new photograph, and it must not be filed as one --
//! but the richer copy's metadata is worth keeping. This computes what the
//! merge would be; nothing here writes anything.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataMerge {
    pub stack_id: i64,
    /// The library path already cataloged for this shot.
    pub existing_path: String,
    pub rating_before: Option<i64>,
    pub rating_after: Option<i32>,
    pub tags_before: Vec<String>,
    pub tags_after: Vec<String>,
}

impl MetadataMerge {
    pub fn changes_rating(&self) -> bool {
        self.rating_after
            .is_some_and(|r| Some(i64::from(r)) != self.rating_before)
    }

    pub fn added_tags(&self) -> Vec<String> {
        self.tags_after
            .iter()
            .filter(|t| !self.tags_before.iter().any(|b| b.eq_ignore_ascii_case(t)))
            .cloned()
            .collect()
    }

    pub fn needs_write(&self) -> bool {
        self.changes_rating() || !self.added_tags().is_empty()
    }
}

/// Monotonic merge: never regress a rating, never drop a tag. Same policy
/// `scan::combine_rating`/`union_embedded_tags` already apply when a stack is
/// first seeded, just applied here to a different pairing -- the SD card's
/// embedded metadata against the library's current catalog state, rather
/// than a stack's RAW member against its JPG sibling.
pub fn resolve(
    catalog_rating: Option<i64>,
    catalog_tags: &[String],
    embedded_rating: Option<i32>,
    embedded_tags: &[String],
) -> (Option<i32>, Vec<String>) {
    let catalog_rating_i32 = catalog_rating.and_then(|r| i32::try_from(r).ok());
    let resolved_rating = crate::scan::combine_rating(catalog_rating_i32, embedded_rating);
    let resolved_tags = crate::scan::union_embedded_tags([catalog_tags, embedded_tags].into_iter());
    (resolved_rating, resolved_tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merge(
        rating_before: Option<i64>,
        rating_after: Option<i32>,
        tags_before: Vec<String>,
        tags_after: Vec<String>,
    ) -> MetadataMerge {
        MetadataMerge {
            stack_id: 1,
            existing_path: "/lib/750_0001.NEF".into(),
            rating_before,
            rating_after,
            tags_before,
            tags_after,
        }
    }

    #[test]
    fn resolve_never_regresses_a_rating() {
        let (rating, _) = resolve(Some(5), &[], Some(2), &[]);
        assert_eq!(rating, Some(5));
    }

    #[test]
    fn resolve_never_drops_a_tag() {
        let (_, tags) = resolve(None, &["existing".to_string()], None, &[]);
        assert_eq!(tags, vec!["existing".to_string()]);
    }

    #[test]
    fn resolve_takes_the_embedded_rating_when_the_catalog_has_none() {
        let (rating, _) = resolve(None, &[], Some(4), &[]);
        assert_eq!(rating, Some(4));
    }

    #[test]
    fn changes_rating_is_false_when_resolved_equals_current() {
        assert!(!merge(Some(4), Some(4), vec![], vec![]).changes_rating());
    }

    #[test]
    fn changes_rating_is_false_when_nothing_embedded() {
        assert!(!merge(Some(4), None, vec![], vec![]).changes_rating());
    }

    #[test]
    fn added_tags_ignores_case_when_comparing_to_existing() {
        let m = merge(
            None,
            None,
            vec!["Berlin".to_string()],
            vec!["berlin".to_string(), "india".to_string()],
        );
        assert_eq!(m.added_tags(), vec!["india".to_string()]);
    }

    #[test]
    fn needs_write_is_false_when_nothing_changes() {
        let m = merge(
            Some(4),
            Some(4),
            vec!["a".to_string()],
            vec!["a".to_string()],
        );
        assert!(!m.needs_write());
    }

    #[test]
    fn needs_write_is_true_when_only_a_tag_is_added() {
        let m = merge(Some(4), Some(4), vec![], vec!["new".to_string()]);
        assert!(m.needs_write());
    }
}
