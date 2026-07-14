//! Typed validation errors shared by the admission webhook and the controller.
//!
//! Per ADR-0003 §2.2 (principle 8) and the SKILL "one validator, two callers"
//! rule, cross-field validation lives in [`crate::validate`] as pure functions
//! returning these typed errors. The webhook rejects at admission; the controller
//! calls the same functions defensively before reconcile. The error type is the
//! contract between them, so messages must be **actionable** — they end up in a
//! `kubectl apply` rejection and in controller logs verbatim.
//!
//! ## Accumulation vs. fail-fast
//!
//! Per-field helpers (e.g. [`crate::validate::validate_repository_ref`]) are
//! **fail-fast**: they return the first problem they find as `ValidationResult`.
//! The per-CRD aggregate validators (`validate_backup_config`, …) **accumulate**
//! every independent problem into a `Vec<ValidationError>` so a user fixing one
//! manifest sees all issues at once rather than playing whack-a-mole across
//! re-applies. Both styles share this one error enum.
//!
//! ```
//! use kopiur_api::ValidationError;
//!
//! // Messages are written for a human reading a rejected `kubectl apply` — they
//! // say what is wrong and why, embedding the offending value.
//! let err = ValidationError::DiscoveredMustRetain { got: "Delete".to_string() };
//! assert!(err.to_string().contains("origin: discovered"));
//! assert!(err.to_string().contains("Delete"));
//!
//! // `ValidationResult` defaults its Ok type to `()` for the pass/fail case.
//! let ok: kopiur_api::ValidationResult = Ok(());
//! assert!(ok.is_ok());
//! ```

use thiserror::Error;

/// A single cross-field validation failure. `PartialEq` so tests can assert the
/// exact variant; messages are written for an end user reading a rejected apply.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum ValidationError {
    /// The maintenance `run-requested`/`run-mode` annotations are malformed
    /// (message produced by [`crate::maintenance::parse_run_annotations`],
    /// already what/why/fix).
    #[error("invalid maintenance run annotation: {message}")]
    InvalidRunAnnotation {
        /// The shared parser's actionable message.
        message: String,
    },

    /// A `Repository`/`ClusterRepository`'s own credential refs, or a consumer's
    /// `repository.namespace`, set a namespace that the variant forbids.
    /// For `kind: ClusterRepository`, `repository.namespace` MUST be absent
    /// (ADR §3.2/§3.3) — the reference is cluster-scoped by name alone.
    #[error(
        "repository.namespace must not be set when repository.kind is ClusterRepository \
         (a ClusterRepository is referenced by name only; got namespace {namespace:?})"
    )]
    ClusterRepoNamespaceForbidden {
        /// The forbidden namespace that was set on the reference.
        namespace: String,
    },

    /// A consumer namespace is not permitted by the target `ClusterRepository`'s
    /// `allowedNamespaces` tenancy gate (ADR §3.2/§4.3).
    #[error(
        "namespace {namespace:?} is not in the allowedNamespaces of ClusterRepository {repo:?}"
    )]
    ConsumerNamespaceNotAllowed {
        /// The consumer namespace that was denied.
        namespace: String,
        /// The `ClusterRepository` whose tenancy gate denied it.
        repo: String,
    },

    /// A `Snapshot` with `origin: discovered` tried to set a `deletionPolicy` other
    /// than `Retain`. Discovered snapshots are forced `Retain` so the operator
    /// never deletes data it did not create (ADR §4.5).
    #[error(
        "origin: discovered snapshots must use deletionPolicy: Retain (got {got:?}); \
         the operator never deletes snapshots it did not create"
    )]
    DiscoveredMustRetain {
        /// The rejected `deletionPolicy` that was set (anything but `Retain`).
        got: String,
    },

    /// A `Restore` with `source.identity` did not set `spec.repository`. Identity
    /// sources cannot derive a repository, so it is required (ADR §3.6/§4.6).
    #[error(
        "restore source.identity requires spec.repository to be set (no Snapshot/SnapshotPolicy to derive it from)"
    )]
    RestoreSourceRepositoryRequired,

    /// A `Repository`/`ClusterRepository` spec carried kopia-side (repo-level)
    /// retention policy fields, which conflict with CR-driven GFS retention and
    /// risk double-deletion (ADR §4.4 exclusivity).
    #[error(
        "inline kopia-side retention policy on a Repository spec is unsupported (field {field:?}); retention is driven exclusively by SnapshotPolicy.spec.retention (ADR §4.4)"
    )]
    InlineRetentionForbidden {
        /// The offending repo-level retention field that was set.
        field: String,
    },

    /// A cron expression failed to parse with the same parser the controller uses
    /// at runtime, so it is rejected at apply time rather than at first reconcile
    /// (ADR §4.1).
    #[error("invalid cron expression {expr:?}: {reason}")]
    InvalidCron {
        /// The cron expression that failed to parse.
        expr: String,
        /// The parser's reason for rejecting it.
        reason: String,
    },

    /// A schedule's `timezone` is not a recognized IANA timezone name (e.g. a typo
    /// like `America/Chicgo`), rejected at apply time rather than silently falling
    /// back to UTC at reconcile.
    #[error("invalid timezone {name:?}: not a recognized IANA timezone name")]
    InvalidTimezone {
        /// The timezone string that failed to parse.
        name: String,
    },

    /// Two fields that may not both be set were both set (e.g. a `Source` with
    /// both `pvc` and `pvcSelector`).
    #[error("fields {a:?} and {b:?} are mutually exclusive but both were set ({context})")]
    MutuallyExclusive {
        /// The first of the two conflicting fields.
        a: String,
        /// The second of the two conflicting fields.
        b: String,
        /// Where the conflict occurred (e.g. `"snapshot source"`), for the message.
        context: String,
    },

    /// A required field (or "at least one of" surface) was empty.
    #[error("missing required field: {field}")]
    MissingRequiredField {
        /// The required field (or "at least one of" surface) that was empty.
        field: String,
    },

    /// A field was set but its value is malformed (e.g. an NFS export path that is
    /// not absolute). The schema can't express the constraint, so the webhook does.
    #[error("invalid value for {field}: {reason}")]
    InvalidFieldValue {
        /// The offending field (e.g. `"snapshot source nfs.path"`).
        field: String,
        /// What's wrong and how to fix it (e.g. `"must be an absolute path"`).
        reason: String,
    },

    /// A `ClusterRepository.identityDefaults` CEL expression (`hostnameExpr` /
    /// `usernameExpr`) failed to **compile** (a syntax error, or it exceeds the
    /// length budget). Surfaced at admission so a bad expression never reaches
    /// status (ADR-0004 §5).
    #[error("identity CEL expression {expr:?} failed to compile: {reason} (check the CEL syntax)")]
    IdentityExprCompile {
        /// The offending CEL expression.
        expr: String,
        /// The parser's reason (or the length-budget message).
        reason: String,
    },

    /// A `ClusterRepository.identityDefaults` CEL expression referenced a variable
    /// outside its environment (e.g. a typo), or otherwise failed to evaluate at
    /// admission (ADR-0004 §5). The environment is `namespace`, `policyName`,
    /// `labels`, `annotations`, `cluster`.
    #[error(
        "identity CEL expression {expr:?} failed to evaluate: {reason} \
         (available variables: namespace, policyName, labels, annotations, cluster)"
    )]
    IdentityExprEval {
        /// The offending CEL expression.
        expr: String,
        /// The evaluation error (e.g. an undeclared-variable reference).
        reason: String,
    },

    /// A `ClusterRepository.identityDefaults` CEL expression evaluated to a
    /// non-string value. `hostnameExpr`/`usernameExpr` must return a string
    /// (ADR-0004 §5).
    #[error(
        "identity CEL expression {expr:?} must return a string, got {got} \
         (hostnameExpr/usernameExpr must evaluate to a string)"
    )]
    IdentityExprType {
        /// The offending CEL expression.
        expr: String,
        /// The CEL value type it returned instead of a string.
        got: String,
    },

    /// `spec.server.auth.insecure` was selected without `acknowledgeInsecure: true`.
    /// The no-auth server exposes full read/write/delete of the repository with no
    /// login, so it must be explicitly acknowledged (server addendum).
    #[error(
        "spec.server.auth.insecure requires acknowledgeInsecure: true — a no-auth kopia \
         server exposes full read/write/delete of every backup with no login"
    )]
    InsecureServerNotAcknowledged,

    /// `spec.server.service.port` was set to an invalid value (0).
    #[error("spec.server.service.port {port} is invalid (must be 1–65535)")]
    InvalidServerPort {
        /// The rejected port value (always `0` today).
        port: u16,
    },

    /// A `ClusterRepository.spec.server` did not set the required target `namespace`.
    #[error(
        "spec.server.namespace is required for a ClusterRepository server (cluster-scoped \
         resources have no implicit namespace)"
    )]
    ServerNamespaceRequired,

    /// A label selector was supplied as the tenancy gate but the caller could not
    /// provide the consumer namespace's labels to match against. We fail closed
    /// (deny) rather than guess (ADR §3.2 — the webhook never trusts unfiltered
    /// input).
    #[error(
        "ClusterRepository {repo:?} gates by label selector but namespace {namespace:?} labels \
         were not available to evaluate; denying (fail-closed)"
    )]
    SelectorLabelsUnavailable {
        /// The consumer namespace whose labels could not be evaluated.
        namespace: String,
        /// The `ClusterRepository` gating by label selector.
        repo: String,
    },

    /// An UPDATE changed a repository field that is fixed at repository-creation
    /// time (`encryption`, `create.splitter`, `create.hash`, `create.encryption`).
    /// Kopia bakes these into the repository's on-disk format, so they cannot change
    /// after creation — the webhook rejects the edit rather than silently ignoring it
    /// (ADR-0005 §7).
    #[error(
        "{field} is immutable after repository creation (it is fixed in the kopia repository \
         format); create a new Repository/ClusterRepository instead of editing this field"
    )]
    Immutable {
        /// The immutable field that an UPDATE attempted to change.
        field: String,
    },

    /// A `SnapshotPolicy`'s resolved kopia identity (`username@hostname[:path]`)
    /// collides with an already-admitted `SnapshotPolicy`'s identity in the **same**
    /// repository. Two recipes interleaving snapshots into one kopia identity corrupts
    /// the snapshot history, so the webhook rejects the second one (ADR-0005 §6).
    #[error(
        "resolved identity {identity:?} collides with existing SnapshotPolicy {conflict:?} in the \
         same repository; two policies must not share a kopia identity (give this policy a distinct \
         spec.identity, or target a different repository)"
    )]
    IdentityCollision {
        /// The resolved `username@hostname[:path]` identity that collided.
        identity: String,
        /// `namespace/name` of the already-admitted conflicting `SnapshotPolicy`.
        conflict: String,
    },

    /// A kopia identity component (`username` or `hostname`) — whether an explicit
    /// `spec.identity` override or the value an `identityDefaults` CEL expression
    /// resolved to — contains a character that breaks kopia's
    /// `username@hostname:path` contract. kopia parses a source on the **first** `@`
    /// and **first** `:` with no escaping, so an embedded `@`, `:`, ASCII whitespace,
    /// or control character silently misparses the identity into a *different* one
    /// (or makes the snapshot un-findable on `snapshot list --source`). Rejected at
    /// admission/resolution so it never reaches a mover Job. Shape-only — every other
    /// character (dots, dashes, slashes, unicode letters) is allowed.
    #[error(
        "{field} {value:?} is not a valid kopia identity component: {reason} \
         (kopia parses username@hostname:path on the first @ and first :, with no escaping)"
    )]
    IdentityComponentInvalid {
        /// The offending field (e.g. `"spec.identity.username"` or `"resolved hostname"`).
        field: String,
        /// The rejected value.
        value: String,
        /// What's wrong (e.g. `"must not contain ':'"`).
        reason: String,
    },

    /// A kopia identity `sourcePath` is malformed — empty, or it contains a newline
    /// or an ASCII control character. The path is everything after the first `:` in
    /// `username@hostname:path`; it may legitimately contain spaces and further `:`,
    /// but not control characters, and must be non-empty when set.
    #[error("{field} {value:?} is not a valid kopia source path: {reason}")]
    IdentitySourcePathInvalid {
        /// The offending field (e.g. `"spec.sources[0].sourcePathOverride"`).
        field: String,
        /// The rejected value.
        value: String,
        /// What's wrong and how to fix it.
        reason: String,
    },

    /// `ClusterRepository.identityDefaults.cluster` is not a valid RFC 1123 label,
    /// or contains a `.`. `cluster` is appended onto the namespace as
    /// `<namespace>.<cluster>` for the default hostname, and
    /// [`crate::identity::classify_hostname`] splits that hostname back apart at
    /// the FIRST `.` — a `cluster` value with an embedded dot would just shift
    /// which suffix classifies as "own cluster" rather than error, so the value is
    /// rejected outright at admission instead of letting classification silently
    /// disagree with intent.
    #[error("identityDefaults.cluster {value:?} is not a valid cluster identity suffix: {reason}")]
    ClusterNameInvalid {
        /// The rejected cluster name.
        value: String,
        /// What's wrong and how to fix it (e.g. the RFC 1123 shape, or the dot-as-delimiter rule).
        reason: String,
    },

    /// An UPDATE to a `SnapshotPolicy` would change its resolved kopia identity
    /// (`username@hostname`, or a source's path) while the policy already has snapshot
    /// history. New snapshots would land under the new kopia source, orphaning the old
    /// history — GFS retention treats them as separate sources and never prunes the
    /// old set (a storage leak), and the new identity restarts retention from zero.
    /// Rejected unless the change is acknowledged with the
    /// `kopiur.home-operations.com/allow-identity-change` annotation
    /// ([`crate::consts::ALLOW_IDENTITY_CHANGE_ANNOTATION`]).
    #[error(
        "this edit changes the policy's resolved kopia identity from {old:?} to {new:?}, but the \
         policy already has snapshot history; new snapshots would orphan the old history (kopia \
         treats them as a separate source and GFS retention never prunes the old set). To \
         intentionally re-identify, set annotation \
         kopiur.home-operations.com/allow-identity-change (any non-empty value)"
    )]
    IdentityWouldFork {
        /// The previously-pinned identity (or source path).
        old: String,
        /// The new identity (or source path) this edit would resolve to.
        new: String,
    },

    /// An UPDATE to a `Repository`/`ClusterRepository`'s `identityDefaults`
    /// (`cluster`, `hostnameExpr`, or `usernameExpr`) would silently re-identify
    /// every consumer `SnapshotPolicy` that resolves through those defaults —
    /// identity is re-resolved from the LIVE repository on every reconcile/backup
    /// (nothing about a *repository's* defaults is pinned the way a policy's own
    /// `spec.identity` is), so this edit changes what each affected policy
    /// resolves to on its very next backup with **no per-policy edit** to
    /// acknowledge it. Exactly like [`Self::IdentityWouldFork`], new snapshots
    /// would land under a new kopia source while the old history orphans (GFS
    /// retention treats it as a separate source and never prunes the old set) —
    /// but here it happens fleet-wide in one apply. Rejected unless the
    /// repository carries the `kopiur.home-operations.com/allow-identity-change`
    /// annotation ([`crate::consts::ALLOW_IDENTITY_CHANGE_ANNOTATION`]).
    #[error(
        "this edit changes identityDefaults, which would re-identify {} — new snapshots would \
         fork to a new kopia lineage while the old history stays under the old identity (GFS \
         retention never prunes across the split). To intentionally re-identify, set annotation \
         kopiur.home-operations.com/allow-identity-change (any non-empty value, e.g. \
         \"intentional\") on this repository; or pin an explicit spec.identity (both username \
         AND hostname) on the affected policies first",
        describe_identity_change_consumers(consumers)
    )]
    RepositoryIdentityWouldFork {
        /// `namespace/name` of every consumer `SnapshotPolicy` with existing
        /// snapshot history that this edit would re-identify (the message
        /// truncates the rendered list to 5 names; this field carries all of
        /// them).
        consumers: Vec<String>,
    },

    /// A verification `successExpr` (ADR-0005 §4/§15) failed to **compile** (a
    /// syntax error, or it exceeds the length budget). Surfaced at admission.
    #[error("successExpr {expr:?} failed to compile: {reason} (check the CEL syntax)")]
    SuccessExprCompile {
        /// The offending CEL expression.
        expr: String,
        /// The parser's reason (or the length-budget message).
        reason: String,
    },

    /// A verification `successExpr` referenced a variable outside its environment
    /// (e.g. a typo), or otherwise failed to evaluate (ADR-0005 §4/§15). The
    /// environment is `stats{files,bytes,errors}`, `snapshot`, `restored`.
    #[error(
        "successExpr {expr:?} failed to evaluate: {reason} \
         (available variables: stats, snapshot, restored)"
    )]
    SuccessExprEval {
        /// The offending CEL expression.
        expr: String,
        /// The evaluation error (e.g. an undeclared-variable reference).
        reason: String,
    },

    /// A verification `successExpr` evaluated to a non-bool value. A `successExpr`
    /// is a pass/fail predicate and must return a bool (ADR-0005 §4/§15).
    #[error("successExpr {expr:?} must return a bool, got {got} (it is a pass/fail predicate)")]
    SuccessExprType {
        /// The offending CEL expression.
        expr: String,
        /// The CEL value type it returned instead of a bool.
        got: String,
    },

    /// A `SnapshotPolicy.spec.preflight` check expression failed to compile (CEL
    /// syntax error, or it exceeds the length budget). Surfaced at admission.
    #[error(
        "preflight check expression {expr:?} failed to compile: {reason} (check the CEL syntax)"
    )]
    PreflightExprCompile {
        /// The offending CEL expression.
        expr: String,
        /// The parser's reason (or the length-budget message).
        reason: String,
    },

    /// A preflight check expression referenced a variable outside its environment
    /// (e.g. a typo), or otherwise failed to evaluate. The environment is the
    /// `repository` and `maintenance` maps.
    #[error(
        "preflight check expression {expr:?} failed to evaluate: {reason} \
         (available variables: repository.{{phase,ready,backendReachable,snapshotCountKnown,\
         snapshotCount,indexBlobCountKnown,indexBlobCount,sizeBytesKnown,sizeBytes,\
         lastHealthyKnown,lastHealthyAgeSeconds,lastReverifyKnown,lastReverifyAgeSeconds}}, \
         maintenance.{{hasRun,lastSuccessAgeSeconds}})"
    )]
    PreflightExprEval {
        /// The offending CEL expression.
        expr: String,
        /// The evaluation error (e.g. an undeclared-variable reference).
        reason: String,
    },

    /// A preflight check expression evaluated to a non-bool value. A preflight
    /// check is a pass/fail predicate and must return a bool.
    #[error(
        "preflight check expression {expr:?} must return a bool, got {got} \
         (it is a pass/fail predicate)"
    )]
    PreflightExprType {
        /// The offending CEL expression.
        expr: String,
        /// The CEL value type it returned instead of a bool.
        got: String,
    },

    /// A `RepositoryReplication`'s `destination` backend is identical to its
    /// source repository's backend (ADR-0005 §13(d)) — replicating a repository to
    /// itself is a no-op (or worse, a loop). The webhook rejects it.
    #[error(
        "RepositoryReplication destination must differ from the source repository's backend \
         (both resolved to the same {backend} target); pick a distinct destination backend"
    )]
    ReplicationDestinationSameAsSource {
        /// The backend kind that both source and destination resolved to.
        backend: String,
    },

    /// A namespaced `Repository` set `spec.maintenance.namespace`, which only
    /// applies to a cluster-scoped `ClusterRepository` (a namespaced
    /// `Repository`'s managed `Maintenance` always lives in the repository's own
    /// namespace). ADR §3.7.
    #[error(
        "spec.maintenance.namespace ({namespace:?}) is only valid on a ClusterRepository; \
         a namespaced Repository's managed Maintenance always lives in the repository's namespace"
    )]
    MaintenanceNamespaceOnNamespacedRepo {
        /// The `spec.maintenance.namespace` value set on the namespaced `Repository`.
        namespace: String,
    },

    /// `catalog.foreignSnapshots` is set, but there is no cluster identity to
    /// classify a snapshot's origin against — `Ignore`/`Fallback` decide what
    /// to do with a snapshot [`crate::identity::classify_hostname`] classifies
    /// as another cluster's, and that classification is undecidable without
    /// `identityDefaults.cluster`. Fires on a `ClusterRepository` whose
    /// `identityDefaults.cluster` is unset, and — kind-neutral wording — on a
    /// namespaced `Repository`, which has no `identityDefaults` field at all
    /// today (M5): the message never claims the field doesn't exist, since a
    /// later milestone may add it.
    #[error(
        "catalog.foreignSnapshots is set, but classifying a snapshot as \"foreign\" requires a \
         cluster identity (`identityDefaults.cluster`); without one there is nothing to compare \
         it against. Fix: set identityDefaults.cluster, or remove catalog.foreignSnapshots"
    )]
    ForeignSnapshotsRequiresCluster,

    /// A `ClusterRepository` sets both `identityDefaults.cluster` and
    /// `catalog.fallbackNamespace` but leaves `catalog.foreignSnapshots`
    /// unset. Both being set at once is a strong signal the fallback
    /// collector is actually relied upon, so adopting a cluster identity must
    /// never silently switch it off by defaulting to `Ignore` — the choice is
    /// forced explicit instead.
    #[error(
        "catalog.foreignSnapshots must be set explicitly: both identityDefaults.cluster and \
         catalog.fallbackNamespace are set, so adopting a cluster identity must not silently \
         change what the fallback collector does. `Ignore` stops materializing foreign \
         snapshots (existing rows in fallbackNamespace age out under catalog.retain); \
         `Fallback` keeps collecting them there. Set one explicitly"
    )]
    ForeignSnapshotsChoiceRequired,
}

/// Render the consumer list for [`ValidationError::RepositoryIdentityWouldFork`]:
/// the count, then up to [`CONSUMER_LIST_SHOWN`] `namespace/name`s, then
/// `"and N more"` for the rest. Also reused verbatim by the webhook to build the
/// admission WARNING when the same change is acknowledged, so the deny message
/// and the warning always name the same policies the same way.
///
/// ```
/// use kopiur_api::error::describe_identity_change_consumers;
///
/// assert_eq!(
///     describe_identity_change_consumers(&["billing/pg".to_string()]),
///     "1 SnapshotPolicy consumer(s) with existing snapshot history (billing/pg)",
/// );
/// let many: Vec<String> = (0..7).map(|i| format!("ns/pg-{i}")).collect();
/// let rendered = describe_identity_change_consumers(&many);
/// assert!(rendered.starts_with("7 SnapshotPolicy consumer(s)"));
/// assert!(rendered.ends_with("and 2 more)"), "{rendered}");
/// ```
pub fn describe_identity_change_consumers(consumers: &[String]) -> String {
    let total = consumers.len();
    let mut names = consumers
        .iter()
        .take(CONSUMER_LIST_SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if total > CONSUMER_LIST_SHOWN {
        names.push_str(&format!(", and {} more", total - CONSUMER_LIST_SHOWN));
    }
    format!("{total} SnapshotPolicy consumer(s) with existing snapshot history ({names})")
}

/// How many consumer names [`describe_identity_change_consumers`] spells out
/// before collapsing the rest into `"and N more"`.
const CONSUMER_LIST_SHOWN: usize = 5;

/// Result alias for validators. Defaults to `()` for the common "pass/fail with no
/// value" case.
pub type ValidationResult<T = ()> = Result<T, ValidationError>;
