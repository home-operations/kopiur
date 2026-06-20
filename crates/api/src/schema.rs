//! Schema helpers for embedding large Kubernetes `core/v1` sub-objects.
//!
//! schemars inlines the *entire* structural schema of an embedded `k8s-openapi`
//! type (`JobSpec` pulls in the full `PodSpec`, `Affinity`, `SecurityContext`,
//! `ResourceRequirements`, …). Those inlined schemas dominate the generated CRDs —
//! a single `SnapshotPolicy` is ~1.2 MB, 95% of it the inlined `JobSpec` for hooks —
//! which bloats Helm releases and breaks large-CRD apply paths (e.g. client-side
//! apply's 256 KB `last-applied-configuration` annotation limit).
//!
//! [`preserve_unknown_object`] renders such a field as an opaque object the
//! apiserver passes through verbatim (`x-kubernetes-preserve-unknown-fields: true`)
//! instead of inlining its schema. The Rust field stays its concrete typed
//! `k8s-openapi` type — kube still deserializes into it and the admission webhook
//! still validates it — so only the apiserver's *structural* validation of the
//! object's internals is relaxed; Kopiur's own type-safety and webhook checks are
//! unchanged.

use schemars::{Schema, SchemaGenerator};

/// Render an embedded `core/v1` object field as
/// `{ type: object, x-kubernetes-preserve-unknown-fields: true }` rather than
/// inlining its full k8s-openapi schema. Use via
/// `#[schemars(schema_with = "crate::schema::preserve_unknown_object")]` on object
/// fields like `securityContext`, `podSecurityContext`, `resources`, `affinity`,
/// and the hooks `jobSpec`.
pub fn preserve_unknown_object(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true,
    })
}
