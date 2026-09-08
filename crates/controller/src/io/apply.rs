use kube::api::{Patch, PatchParams};
use kube::{Api, Resource};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::Result;

/// The field-manager used for every server-side apply the controller performs.
pub const FIELD_MANAGER: &str = "kopiur.home-operations.com/controller";

/// Server-side apply an object into the given namespaced API. Idempotent: the
/// controller owns the fields it sets; reapplying converges. Uses
/// [`FIELD_MANAGER`] with `force` so the controller reliably re-takes ownership
/// of fields after a restart (ADR §5.2).
pub async fn apply<K>(api: &Api<K>, name: &str, obj: &K) -> Result<K>
where
    K: Resource + Serialize + DeserializeOwned + Clone + std::fmt::Debug,
{
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    Ok(api.patch(name, &pp, &Patch::Apply(obj)).await?)
}

/// Patch an object's `.status` subresource with a strategic-merge body.
pub async fn patch_status<K>(api: &Api<K>, name: &str, status: serde_json::Value) -> Result<()>
where
    K: Resource + DeserializeOwned + Clone + std::fmt::Debug,
{
    let body = serde_json::json!({ "status": status });
    let pp = PatchParams::apply(FIELD_MANAGER);
    api.patch_status(name, &pp, &Patch::Merge(&body)).await?;
    Ok(())
}

/// Whether merge-patching `desired` over `current` would be a no-op — i.e. every
/// key in `desired` already holds the same value in `current`. `current` is the
/// object's existing `.status` serialized to JSON (or `None` when there is no
/// status yet, which is never a no-op).
///
/// This is the predicate behind [`patch_status_if_changed`]. It deliberately only
/// inspects the keys present in `desired` (a strategic merge never removes the
/// keys it omits), so a reconciler that patches a *subset* of status compares only
/// that subset.
pub fn status_patch_is_noop(
    current: Option<&serde_json::Value>,
    desired: &serde_json::Value,
) -> bool {
    let (Some(current), Some(desired_obj)) = (current, desired.as_object()) else {
        return false;
    };
    let Some(current_obj) = current.as_object() else {
        return false;
    };
    desired_obj
        .iter()
        .all(|(k, v)| current_obj.get(k) == Some(v))
}

/// Apply an RFC-7386 JSON merge patch of `patch` onto `target`, returning the
/// result. Pure.
///
/// The three rules, verbatim from the RFC and from what the API server does with
/// a `Patch::Merge` body: a `null` REMOVES the key, two objects MERGE key by key
/// (recursively), and anything else — arrays included — REPLACES.
pub(crate) fn apply_merge_patch(
    target: &serde_json::Value,
    patch: &serde_json::Value,
) -> serde_json::Value {
    let serde_json::Value::Object(patch_obj) = patch else {
        return patch.clone();
    };
    let mut out = match target {
        serde_json::Value::Object(t) => t.clone(),
        _ => serde_json::Map::new(),
    };
    for (key, value) in patch_obj {
        if value.is_null() {
            out.remove(key);
            continue;
        }
        let existing = out.get(key).cloned().unwrap_or(serde_json::Value::Null);
        out.insert(key.clone(), apply_merge_patch(&existing, value));
    }
    serde_json::Value::Object(out)
}

/// Whether merge-patching `desired` over `current` would leave the status
/// BYTE-IDENTICAL, evaluated with full RFC-7386 semantics — the deep counterpart
/// to [`status_patch_is_noop`].
///
/// [`status_patch_is_noop`] compares each top-level key by whole-value equality,
/// which is exact for a reconciler that writes whole values but wrong for one
/// that writes a PARTIAL sub-object. The fanned-out populator (#443) patches
/// `claims: { "<one pvc>": {…} }` while `status.claims` holds every claimant, so
/// the shallow check can never see a no-op: every pass would PATCH, bump
/// `resourceVersion`, wake the watch and re-trigger itself — the exact hot-loop
/// [`patch_status_if_changed`] exists to break, on the 120s cadence times N
/// claims.
///
/// It also reads an explicit `null` correctly (a merge DELETES that key), where
/// the shallow check compares it against the current value and effectively never
/// matches.
///
/// Pure and cluster-free, so the semantics are unit-asserted rather than
/// discovered from a busy cluster.
pub fn status_merge_patch_is_noop(
    current: Option<&serde_json::Value>,
    desired: &serde_json::Value,
) -> bool {
    let Some(current) = current else {
        return false;
    };
    apply_merge_patch(current, desired) == *current
}

/// Idempotent status patch: skip the PATCH entirely when `desired` matches the
/// object's existing status (`current`), returning `false`; otherwise merge-patch
/// and return `true`.
///
/// This is what breaks the reconcile hot-loop: a controller that re-writes an
/// unchanged `Failed` status would bump `resourceVersion`, emit a watch event, and
/// re-trigger itself in a tight loop. Skipping the no-op write means no event, no
/// re-trigger. For this to hold, the `desired` status must be byte-stable across
/// repeated identical failures — hence the condition message comes from
/// [`kopiur_kopia::KopiaErrorClass::summary`] (volatile-free) and
/// [`crate::io::upsert_condition`] preserves `lastTransitionTime` while the status is
/// unchanged. The returned bool lets the caller fire its Warning Event only on a
/// real transition.
///
/// The no-op test is [`status_merge_patch_is_noop`] — full RFC-7386 semantics,
/// i.e. exactly what the API server would do with this body. The shallow
/// [`status_patch_is_noop`] agrees with it on every FLAT status (asserted in
/// `io::tests`), and the two differ only where the deep one is right: a PARTIAL
/// sub-object patch (the fanned-out populator's `claims: { "<one pvc>": {…} }`,
/// #443) and an explicit `null` (a merge DELETES that key). Using the shallow
/// predicate there would make every pass a write, bumping `resourceVersion`,
/// waking the watch and re-triggering the reconcile — the very hot-loop this
/// function exists to break.
pub async fn patch_status_if_changed<K>(
    api: &Api<K>,
    name: &str,
    current: Option<&serde_json::Value>,
    desired: serde_json::Value,
) -> Result<bool>
where
    K: Resource + DeserializeOwned + Clone + std::fmt::Debug,
{
    if status_merge_patch_is_noop(current, &desired) {
        return Ok(false);
    }
    patch_status(api, name, desired).await?;
    Ok(true)
}

/// Whether `status` already records a **terminal** `Failed` for the given spec
/// `generation` — i.e. the reconciler hard-stopped on a non-retryable failure and
/// nothing in the spec has changed since (`observedGeneration == generation`).
///
/// A repository reconciler checks this before re-reading secrets or re-connecting
/// to the backend: once terminal for the current generation, it returns a quiet
/// heartbeat instead of re-hitting a backend that cannot succeed until the user
/// edits the CR (which bumps `metadata.generation` and reopens the gate). Only
/// `Failed` is treated as terminal — `Degraded` (a *retryable* failure) keeps
/// retrying on the transient cadence.
pub fn is_terminal_for_generation(
    phase: Option<&kopiur_api::RepositoryPhase>,
    observed_generation: Option<i64>,
    generation: Option<i64>,
) -> bool {
    use kopiur_api::RepositoryPhase as P;
    // Exhaustive, not `== Failed`: this is a *classification* (is the reconciler
    // hard-stopped?), so a new phase must not silently land on the "keep
    // retrying" side by default — the compiler asks here first.
    let hard_stopped = phase.is_some_and(|p| match p {
        P::Failed => true,
        // `Degraded` is a RETRYABLE failure (open circuit breaker / retryable
        // bootstrap error) and keeps its transient cadence; the rest are
        // in-flight or healthy.
        P::Pending | P::Initializing | P::Ready | P::Degraded => false,
        // A phase this build cannot interpret is never a hard stop: parking a
        // newer operator's repository forever on an unreadable phase is strictly
        // worse than one extra (cheap, idempotent) connect attempt.
        P::Unknown(_) => false,
    });
    generation.is_some() && hard_stopped && observed_generation == generation
}

/// Whether the terminal-failure hard-stop still holds — i.e. we should return a
/// quiet heartbeat instead of re-attempting the backend.
///
/// Extends [`is_terminal_for_generation`] with a *credential* check: a terminal
/// failure means "won't succeed until an **input** changes", and the inputs are
/// the spec (`generation`) **and** the referenced password Secret. A Secret
/// content edit does NOT bump `metadata.generation`, so gating on generation alone
/// parks the object forever even after the user fixes the credential. We therefore
/// also reopen the gate when the password Secret's `resourceVersion` differs from
/// the one (`recorded_version`) observed at the last failed connect — `current_version`
/// is the Secret's live `resourceVersion`, read cheaply before this check.
///
/// A third opener, for the same reason (issue #435): a live, VALID
/// `allow-reinitialize` ack is a new input too, and applying an annotation bumps
/// neither `metadata.generation` nor the Secret's `resourceVersion`. Without
/// `reinit_requested` here, the user's deliberate re-initialize of a wiped
/// backend would sit behind the 30-minute heartbeat on exactly the bare-path
/// repositories this gate protects. `reinit_requested` is computed by the caller
/// as `phase != Ready && matches!(create_gate(..), Allowed { via_reinit_ack: true })`
/// — the `phase != Ready` half is load-bearing: on a healthy repository a
/// standing ack must be a complete no-op, never a nudge that re-opens the
/// backend.
///
/// Holds (skip the backend) only when ALL THREE are unchanged: terminal for this
/// generation, the credential is byte-for-byte the same Secret revision, and no
/// re-initialize was requested. Any difference (including a first failure that
/// recorded no version) reopens it.
pub fn terminal_gate_holds(
    phase: Option<&kopiur_api::RepositoryPhase>,
    observed_generation: Option<i64>,
    generation: Option<i64>,
    recorded_version: Option<&str>,
    current_version: &str,
    reinit_requested: bool,
) -> bool {
    !reinit_requested
        && is_terminal_for_generation(phase, observed_generation, generation)
        && recorded_version == Some(current_version)
}
