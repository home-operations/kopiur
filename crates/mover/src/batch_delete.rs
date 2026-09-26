//! Pure planning for a `SnapshotDeleteBatch` Job (issue #477).
//!
//! The batch used to run one `kopia snapshot delete <id>` process per member,
//! and every process re-opened the repository and reloaded its full index. On a
//! large remote repository that cost minutes per member. This module plans the
//! whole batch against ONE `snapshot list` so the mover can delete every present
//! manifest in a single `kopia snapshot delete <id>... --delete`.
//!
//! Partitioning first is mandatory, not an optimization. kopia's multi-id
//! delete is **all-or-nothing** (its write session flushes only on success),
//! and ONE absent id aborts the whole call, so an id that is already gone must
//! never reach the bulk argv.
//!
//! Every per-member semantic of the legacy one-at-a-time path is preserved:
//! - A member whose recorded id is absent is already done (the idempotent
//!   "no snapshots matched" path).
//! - The stale-id self-heal (kopia rewrites the manifest id on pin) runs only
//!   behind the same [`anchor_self_heal_allowed`] data-loss gate. It uses the
//!   same [`match_current_manifest`] matcher over complete snapshots, with the
//!   member's OWN recorded id excluded, which is the post-delete view the legacy
//!   path re-listed after deleting it.
//!
//! No kopia, no kube: the IO shell lives in the mover binary.

use std::collections::{BTreeMap, HashMap, HashSet};

use kopiur_kopia::SnapshotListEntry;

use crate::resolve::match_current_manifest;
use crate::workspec::{SnapshotAnchor, SnapshotDeleteItem};

/// Whether the stale-id self-heal may re-resolve a live manifest for this
/// anchor. Gated on `start_time` alone. Without that disambiguator,
/// [`match_current_manifest`]'s path(+identity)-only fallback can uniquely match
/// a NEWER, unrelated snapshot at the same source path (e.g. the same identity's
/// very next backup), and deleting it would destroy data that was never
/// targeted (data-loss fix, adversarial review).
pub fn anchor_self_heal_allowed(anchor: &SnapshotAnchor) -> bool {
    anchor.start_time.is_some()
}

/// A stale recorded id the plan healed to its live, re-resolved manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealedId {
    /// Index of the member in the batch.
    pub member: usize,
    /// The id the `Snapshot` recorded (rewritten away by a pin).
    pub recorded: String,
    /// The live manifest id re-resolved from the member's anchor.
    pub live: String,
}

/// What one batch Job must delete, decided from a single listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchDeletePlan {
    /// Distinct manifest ids to delete, in first-seen member order. Every id is
    /// present in the listing the plan was built from.
    pub ids: Vec<String>,
    /// For each id in [`Self::ids`] (same position), the members it serves.
    /// Deduplicated: several `Snapshot` CRs can pin one kopia manifest.
    pub members_of: Vec<Vec<usize>>,
    /// Members with nothing left to delete. Already done, which is success.
    pub already_absent: Vec<usize>,
    /// Stale recorded ids healed to a live manifest, for the operator log.
    pub healed: Vec<HealedId>,
}

impl BatchDeletePlan {
    fn target(&mut self, index: &mut HashMap<String, usize>, id: &str, member: usize) {
        let pos = *index.entry(id.to_string()).or_insert_with(|| {
            self.ids.push(id.to_string());
            self.members_of.push(Vec::new());
            self.ids.len() - 1
        });
        let members = &mut self.members_of[pos];
        if members.last() != Some(&member) {
            members.push(member);
        }
    }
}

/// Plan a batch against `listing`, a raw `snapshot list --all --json` that
/// includes incomplete manifests (see
/// [`kopiur_kopia::KopiaClient::snapshot_list_all_with_incomplete`]). A
/// checkpoint a `Snapshot` points at is still a manifest id to delete. Only the
/// self-heal matcher is restricted to complete snapshots.
pub fn plan_batch_delete(
    items: &[SnapshotDeleteItem],
    listing: &[SnapshotListEntry],
) -> BatchDeletePlan {
    let present: HashSet<&str> = listing.iter().map(|e| e.id.as_str()).collect();
    let mut by_path: BTreeMap<&str, Vec<SnapshotListEntry>> = BTreeMap::new();
    for e in listing.iter().filter(|e| e.incomplete_reason().is_none()) {
        by_path
            .entry(e.source.path.as_str())
            .or_default()
            .push(e.clone());
    }

    let mut plan = BatchDeletePlan::default();
    let mut index = HashMap::new();
    for (member, item) in items.iter().enumerate() {
        let mut targeted = false;
        if present.contains(item.snapshot_id.as_str()) {
            plan.target(&mut index, &item.snapshot_id, member);
            targeted = true;
        }
        if let Some(live) = self_heal_target(item, &by_path) {
            plan.target(&mut index, &live, member);
            plan.healed.push(HealedId {
                member,
                recorded: item.snapshot_id.clone(),
                live,
            });
            targeted = true;
        }
        if !targeted {
            plan.already_absent.push(member);
        }
    }
    plan
}

/// The live manifest a stale recorded id was rewritten to, if the self-heal
/// gate is open and the anchor re-resolves UNIQUELY to a different id.
fn self_heal_target(
    item: &SnapshotDeleteItem,
    by_path: &BTreeMap<&str, Vec<SnapshotListEntry>>,
) -> Option<String> {
    let anchor = &item.anchor;
    if !anchor_self_heal_allowed(anchor) {
        return None;
    }
    let candidates: Vec<SnapshotListEntry> = by_path
        .get(anchor.source_path.as_str())?
        .iter()
        .filter(|e| e.id != item.snapshot_id)
        .cloned()
        .collect();
    match_current_manifest(
        &candidates,
        &anchor.source_path,
        anchor.start_instant(),
        anchor.identity_filter(),
    )
    .map(|e| e.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = "2026-06-19T05:54:19Z";

    fn entry(id: &str, path: &str, start: &str) -> SnapshotListEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "source": { "host": "prod", "userName": "mydb", "path": path },
            "startTime": start,
            "endTime": start,
        }))
        .expect("entry fixture")
    }

    fn item(id: &str, anchor: SnapshotAnchor) -> SnapshotDeleteItem {
        SnapshotDeleteItem {
            snapshot_id: id.into(),
            anchor,
        }
    }

    fn anchored(path: &str, start: Option<&str>) -> SnapshotAnchor {
        SnapshotAnchor {
            source_path: path.into(),
            start_time: start.map(str::to_string),
            username: Some("mydb".into()),
            hostname: Some("prod".into()),
        }
    }

    #[test]
    fn present_ids_are_deleted_absent_ones_are_already_done() {
        let listing = vec![entry("a", "/p/a", START), entry("c", "/p/c", START)];
        let items = [
            item("a", SnapshotAnchor::default()),
            item("gone", SnapshotAnchor::default()),
            item("c", SnapshotAnchor::default()),
        ];
        let plan = plan_batch_delete(&items, &listing);
        assert_eq!(plan.ids, ["a", "c"]);
        assert_eq!(plan.members_of, [vec![0], vec![2]]);
        assert_eq!(plan.already_absent, [1]);
        assert!(plan.healed.is_empty());
    }

    #[test]
    fn members_sharing_one_manifest_dedupe_to_one_id() {
        let listing = vec![entry("same", "/p", START)];
        let items = [
            item("same", SnapshotAnchor::default()),
            item("same", SnapshotAnchor::default()),
        ];
        let plan = plan_batch_delete(&items, &listing);
        assert_eq!(plan.ids, ["same"]);
        assert_eq!(plan.members_of, [vec![0, 1]]);
    }

    #[test]
    fn stale_recorded_id_heals_to_the_live_manifest() {
        let listing = vec![entry("live", "/pvc/db", START)];
        let items = [item("stale", anchored("/pvc/db", Some(START)))];
        let plan = plan_batch_delete(&items, &listing);
        assert_eq!(plan.ids, ["live"]);
        assert!(plan.already_absent.is_empty());
        assert_eq!(
            plan.healed,
            [HealedId {
                member: 0,
                recorded: "stale".into(),
                live: "live".into(),
            }]
        );
    }

    /// Mid-rewrite, both the recorded manifest and its live rewrite exist and
    /// share `(path, identity, startTime)`. Matching over the raw listing would
    /// be AMBIGUOUS and orphan the live manifest. Excluding the member's own
    /// recorded id reproduces the post-delete view the legacy path re-listed.
    #[test]
    fn recorded_and_live_both_present_deletes_both() {
        let listing = vec![
            entry("rec", "/pvc/db", START),
            entry("live", "/pvc/db", START),
        ];
        let items = [item("rec", anchored("/pvc/db", Some(START)))];
        let plan = plan_batch_delete(&items, &listing);
        assert_eq!(plan.ids, ["rec", "live"]);
        assert_eq!(plan.members_of, [vec![0], vec![0]]);
    }

    /// The data-loss gate: no `start_time` means no self-heal, even though a
    /// path-only match would uniquely find another (newer, unrelated) snapshot.
    #[test]
    fn no_start_time_never_self_heals() {
        let listing = vec![entry("other", "/pvc/db", START)];
        let items = [item("stale", anchored("/pvc/db", None))];
        let plan = plan_batch_delete(&items, &listing);
        assert!(plan.ids.is_empty(), "must not delete an unrelated snapshot");
        assert_eq!(plan.already_absent, [0]);
    }

    #[test]
    fn ambiguous_live_match_heals_nothing() {
        let listing = vec![entry("x", "/pvc/db", START), entry("y", "/pvc/db", START)];
        let items = [item("stale", anchored("/pvc/db", Some(START)))];
        let plan = plan_batch_delete(&items, &listing);
        assert!(plan.ids.is_empty());
        assert_eq!(plan.already_absent, [0]);
    }

    /// A checkpoint a `Snapshot` points at is a real manifest to delete, but a
    /// checkpoint is never the "live" manifest the self-heal re-resolves to.
    #[test]
    fn checkpoints_are_deletable_but_never_heal_targets() {
        let mut ckpt = entry("ckpt", "/pvc/db", START);
        ckpt.incomplete = Some("checkpoint".into());
        let listing = vec![ckpt];
        let direct = plan_batch_delete(&[item("ckpt", SnapshotAnchor::default())], &listing);
        assert_eq!(direct.ids, ["ckpt"]);
        let heal = plan_batch_delete(&[item("stale", anchored("/pvc/db", Some(START)))], &listing);
        assert!(heal.ids.is_empty());
    }

    #[test]
    fn self_heal_gate_is_start_time_alone() {
        assert!(!anchor_self_heal_allowed(&anchored("/p", None)));
        assert!(anchor_self_heal_allowed(&anchored("/p", Some(START))));
    }
}
