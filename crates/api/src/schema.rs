//! Schema helper for embedding a large Kubernetes `core/v1` sub-object.
//!
//! schemars inlines the *entire* structural schema of an embedded `k8s-openapi`
//! type. The worst offender is the hooks `JobSpec`, which pulls in the full
//! `PodSpec`: inlined, it makes a single `SnapshotPolicy` CRD ~1.2 MB (~85% of the
//! whole bundle), which bloats Helm releases and breaks large-CRD apply paths (e.g.
//! client-side apply's 256 KB `last-applied-configuration` annotation limit).
//!
//! [`preserve_unknown_object`] renders such a field as an opaque object the
//! apiserver passes through verbatim (`x-kubernetes-preserve-unknown-fields: true`)
//! instead of inlining its schema. The Rust field stays its concrete typed
//! `k8s-openapi` type — kube still deserializes into it (so scalar type errors are
//! still rejected at admission) and the apiserver structurally validates the actual
//! object when the controller creates it (e.g. the hook Job at Job-creation time) —
//! so only *apply-time* structural validation of that field is deferred.
//!
//! Used **only** for the outsized hooks `jobSpec` ([`crate::snapshot_policy::RunJobHook`]).
//! The smaller embedded `core/v1` objects (mover/server `securityContext`,
//! `podSecurityContext`, `resources`, `affinity`) are deliberately left inlined so
//! the apiserver keeps validating them at apply time — the size they add is modest
//! and the early validation is worth keeping for a backup operator.

use schemars::{Schema, SchemaGenerator};

/// Render an embedded `core/v1` object field as
/// `{ type: object, x-kubernetes-preserve-unknown-fields: true }` rather than
/// inlining its full k8s-openapi schema. Use via
/// `#[schemars(schema_with = "crate::schema::preserve_unknown_object")]`. Reserved
/// for the outsized hooks `jobSpec`; see the module docs for why the smaller
/// embedded objects are left inlined.
pub fn preserve_unknown_object(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true,
    })
}
