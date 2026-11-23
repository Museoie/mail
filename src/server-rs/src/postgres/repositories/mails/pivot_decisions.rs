//! Which mapped-utility pivot rows a save keeps in step with a mail's flags
//! (repositories/mails/pivot-decisions.ts).

/// The two mapped-utility mailbox names, in the spelling the wire boundary
/// canonicalizes to before any pivot decision runs.
pub const STARRED_MAILBOX: &str = "Starred";
pub const TRASH_MAILBOX: &str = "Trash";

/// One pivot write the caller should issue for a saved row: `mailbox` is the
/// mapped-utility name (Starred / Trash), `present` is the target flag value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedPivotDecision {
    pub mailbox: &'static str,
    pub present: bool,
}

/// The gating logic behind `core::sync_mapped_pivots_for_row`. Returns the
/// pivot writes the caller should issue, in order.
///
/// - `saved` / `deleted` of `None` skips the corresponding pivot. The INSERT
///   branch of a save uses this for a false-flag fresh row that never had a
///   pivot to remove; the merge branch uses it for flags the incoming
///   placement didn't touch.
/// - A destination that is itself `Starred` / `Trash` skips that pivot: the
///   caller's own mapping write for the reserved UID already handled it, so
///   syncing on top would waste a counter tick.
pub fn decide_mapped_pivots(
    saved: Option<bool>,
    deleted: Option<bool>,
    dest_mailbox: Option<&str>,
) -> Vec<MappedPivotDecision> {
    let mut decisions = Vec::new();
    if let Some(present) = saved.filter(|_| dest_mailbox != Some(STARRED_MAILBOX)) {
        decisions.push(MappedPivotDecision { mailbox: STARRED_MAILBOX, present });
    }
    if let Some(present) = deleted.filter(|_| dest_mailbox != Some(TRASH_MAILBOX)) {
        decisions.push(MappedPivotDecision { mailbox: TRASH_MAILBOX, present });
    }
    decisions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pivot(mailbox: &'static str, present: bool) -> MappedPivotDecision {
        MappedPivotDecision { mailbox, present }
    }

    // core-decisions.test.ts — "decideMappedPivots"

    #[test]
    fn an_unset_flag_skips_its_pivot() {
        assert_eq!(decide_mapped_pivots(None, None, None), vec![]);
        assert_eq!(decide_mapped_pivots(None, None, Some("Archive")), vec![]);
    }

    #[test]
    fn a_true_flag_makes_its_pivot_present() {
        assert_eq!(
            decide_mapped_pivots(Some(true), Some(true), Some("Archive")),
            vec![pivot("Starred", true), pivot("Trash", true)]
        );
    }

    #[test]
    fn a_false_flag_clears_its_pivot() {
        assert_eq!(
            decide_mapped_pivots(Some(false), Some(false), None),
            vec![pivot("Starred", false), pivot("Trash", false)]
        );
    }

    #[test]
    fn skips_starred_when_the_destination_is_starred() {
        assert_eq!(decide_mapped_pivots(Some(true), None, Some("Starred")), vec![]);
        assert_eq!(decide_mapped_pivots(Some(true), Some(true), Some("Starred")), vec![pivot("Trash", true)]);
    }

    #[test]
    fn skips_trash_when_the_destination_is_trash() {
        assert_eq!(decide_mapped_pivots(None, Some(true), Some("Trash")), vec![]);
        assert_eq!(decide_mapped_pivots(Some(true), Some(true), Some("Trash")), vec![pivot("Starred", true)]);
    }

    #[test]
    fn only_issues_the_writes_whose_flag_was_touched() {
        assert_eq!(decide_mapped_pivots(None, Some(true), Some("Archive")), vec![pivot("Trash", true)]);
        assert_eq!(decide_mapped_pivots(Some(true), None, Some("Archive")), vec![pivot("Starred", true)]);
    }

    #[test]
    fn the_destination_is_case_sensitive() {
        assert_eq!(decide_mapped_pivots(Some(true), None, Some("starred")), vec![pivot("Starred", true)]);
    }
}
