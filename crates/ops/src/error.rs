//! The shared operations layer's typed error surface. One exhaustive enum;
//! every message states what failed, why, and how to fix it (the message text
//! is unit-tested). Both `kubectl kopiur` and the web UI render these — the CLI
//! prints the message, a server maps [`OpsError::kind`] onto a status code.

/// Everything a kopiur operation can fail with. Exhaustive — a new failure mode
/// is a new variant, never a stringly-typed catch-all.
#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    /// A `pvcSelector` recipe matched no PersistentVolumeClaims, so there is
    /// nothing to snapshot.
    #[error(
        "SnapshotPolicy {policy} uses a pvcSelector that currently matches no \
         PersistentVolumeClaims in namespace {namespace}, so `snapshot now` has nothing to back \
         up. Fix: check the selector's labels against `kubectl -n {namespace} get pvc \
         --show-labels`"
    )]
    SelectorMatchedNothing {
        /// The recipe whose selector matched nothing.
        policy: String,
        /// Namespace the selector was evaluated in.
        namespace: String,
    },

    /// `--repository` named a repository that is not one of the policy's
    /// targets (multi-repo fan-out, #368) — refused rather than silently
    /// backing up into nothing.
    #[error(
        "--repository {given} does not name a repository of SnapshotPolicy {policy}; \
         valid: {valid}"
    )]
    UnknownPolicyRepository {
        /// The `--repository` value given.
        given: String,
        /// The recipe whose repository set was checked.
        policy: String,
        /// Comma-joined valid repository names.
        valid: String,
    },

    /// The API server refused the request with 403.
    #[error(
        "forbidden: cannot {verb} {resource}{scope}: {source}. \
         Your kubeconfig user lacks RBAC for this. \
         Fix: ask a cluster admin to grant `{verb}` on `{resource}` \
         (kopiur.home-operations.com) to your user, or run with a more \
         privileged kubeconfig/--context"
    )]
    Forbidden {
        /// The verb that was refused (`get`, `list`, `patch`, …).
        verb: &'static str,
        /// The (plural) resource the verb targeted.
        resource: &'static str,
        /// Human-readable scope suffix (`" in namespace x"` or `""`).
        scope: String,
        /// The API server's error.
        #[source]
        source: Box<kube::Error>,
    },

    /// The resource *type* is unknown to the API server — the kopiur CRDs are
    /// not installed (or not this version).
    #[error(
        "the API server does not know the {kind} resource type: {source}. \
         The kopiur CRDs are missing or outdated on this cluster. \
         Fix: install kopiur (`helm install kopiur oci://ghcr.io/home-operations/charts/kopiur`) \
         or apply the CRDs from deploy/crds/, then retry"
    )]
    KindNotInstalled {
        /// The kopiur kind that is missing.
        kind: &'static str,
        /// The API server's error.
        #[source]
        source: Box<kube::Error>,
    },

    /// A named object was not found.
    #[error(
        "{kind} {name:?} not found{scope}. \
         Fix: list what exists with `kubectl get {plural}{scope_flag}` and check \
         the name (and --namespace/--context)"
    )]
    NotFound {
        /// The kopiur kind looked up.
        kind: &'static str,
        /// Its plural, for the remediation command.
        plural: &'static str,
        /// The missing object's name.
        name: String,
        /// Human-readable scope suffix (`" in namespace x"` or `""`).
        scope: String,
        /// The matching `kubectl` scope flag (`" -n x"`, `" -A"`, or `""`).
        scope_flag: String,
    },

    /// Any other Kubernetes API failure.
    #[error(
        "Kubernetes API request failed: cannot {verb} {resource}{scope}: {source}. \
         Fix: check cluster/API-server health (`kubectl version`) and connectivity, \
         then retry"
    )]
    Api {
        /// The verb attempted.
        verb: &'static str,
        /// The (plural) resource targeted.
        resource: &'static str,
        /// Human-readable scope suffix.
        scope: String,
        /// The underlying kube client error.
        #[source]
        source: Box<kube::Error>,
    },

    /// An admission webhook (kopiur's, or a cluster policy engine) rejected
    /// the object.
    #[error(
        "an admission webhook rejected this object: {message}. \
         Fix: correct the flags/spec per the message above and retry \
         (the message names the webhook that denied it)"
    )]
    AdmissionDenied {
        /// The webhook's denial message (already actionable by project norm).
        message: String,
    },

    /// A `--wait` deadline expired before the object reached a terminal state.
    #[error(
        "timed out after {after} waiting for {what}. \
         The operation is still running in the cluster — waiting stopped, the work did not. \
         Fix: {hint}"
    )]
    WaitTimeout {
        /// What was being waited on.
        what: String,
        /// The timeout that expired, humanized.
        after: String,
        /// How to keep observing or adjust the deadline.
        hint: String,
    },

    /// The object being waited on was deleted mid-wait.
    #[error(
        "{what} was deleted while waiting for it to finish. \
         Something (a user, GitOps prune, or retention) removed the object. \
         Fix: check `kubectl get events` for who deleted it, then re-run"
    )]
    GoneWhileWaiting {
        /// What was being waited on.
        what: String,
    },

    /// A by-reference lookup matched more than one object.
    #[error(
        "{what}: {candidates}. \
         Fix: name the one you mean explicitly (pass it as the positional NAME argument)"
    )]
    AmbiguousTarget {
        /// What was looked up and how many matched.
        what: String,
        /// The matching object names.
        candidates: String,
    },

    /// A snapshot's repository lives in a different namespace than the
    /// session pod would.
    #[error(
        "the snapshot's repository ({repo}) lives in namespace {repo_namespace}, but the \
         browse session pod runs in the snapshot's namespace ({session_namespace}) — \
         Kubernetes forbids cross-namespace owners, so the pod would be garbage-collected \
         mid-read. Fix: browse a snapshot in the repository's namespace, or use --local"
    )]
    RepoOutsideSessionNamespace {
        /// `kind/name` of the repository.
        repo: String,
        /// Where the repository lives.
        repo_namespace: String,
        /// Where the session pod would run.
        session_namespace: String,
    },

    /// The session mover image could not be resolved safely.
    #[error("cannot resolve the mover image for the browse session: {why}. Fix: {fix}")]
    MoverImageUnresolvable {
        /// What went wrong with the lookup.
        why: String,
        /// What to do about it.
        fix: String,
    },

    /// A ClusterRepository's `tls.caBundleRef` ConfigMap lives in the
    /// operator's namespace, which could not be discovered (zero or several
    /// controller Deployments matched the chart labels).
    #[error(
        "cannot locate the operator namespace holding ClusterRepository {repository:?}'s \
         tls.caBundleRef ConfigMap {configmap:?}: {why}. \
         A ClusterRepository's CA bundle lives in the namespace the kopiur controller \
         runs in (KOPIUR_NAMESPACE), which the CLI discovers from the controller \
         Deployment. Fix: {fix}"
    )]
    OperatorNamespaceUnresolvable {
        /// The ClusterRepository whose CA bundle needs the namespace.
        repository: String,
        /// The `tls.caBundleRef` ConfigMap name.
        configmap: String,
        /// What went wrong with the discovery.
        why: String,
        /// What to do about it.
        fix: String,
    },

    /// A backend's `tls.caBundleRef` could not be resolved to PEM content
    /// (missing ConfigMap, missing key, or non-PEM data).
    #[error(
        "cannot resolve tls.caBundleRef for repository {repository:?}: {detail}. \
         The CA bundle is inlined into the kopia connect so the backend's \
         private-CA TLS endpoint is trusted. Fix: {fix}"
    )]
    CaBundleUnresolvable {
        /// The repository whose backend declares the caBundleRef.
        repository: String,
        /// What exactly failed (names the ConfigMap/namespace/key).
        detail: String,
        /// What to do about it.
        fix: String,
    },

    /// A path component that must be a directory is something else.
    #[error(
        "{path:?} is not a directory (kopia entry type {entry_type:?}); \
         `ls` lists directories — use `cat`/`download` to read a file"
    )]
    NotADirectory {
        /// The offending path.
        path: String,
        /// The kopia entry type encountered (`f`, `s`, …).
        entry_type: String,
    },

    /// (De)serializing an object for output failed — a kopiur bug, not a user error.
    #[error(
        "failed to serialize {what} for output: {source}. \
         This is a kubectl-kopiur bug — please report it at \
         https://github.com/home-operations/kopiur/issues"
    )]
    Serialization {
        /// What was being serialized.
        what: &'static str,
        /// The serde error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    // --- browse data-plane (ls / cat / download / browse / session end) ---
    /// The Snapshot has no kopia snapshot id pinned in status, so there is
    /// nothing to read.
    #[error(
        "Snapshot {name:?} cannot be browsed: {reason}. \
         Browsing reads the kopia snapshot recorded in status.snapshot.kopiaSnapshotID, \
         which only exists once a snapshot succeeded (or was discovered). \
         Fix: wait for it to reach Succeeded, or pick another with \
         `kubectl kopiur snapshots list`"
    )]
    SnapshotNotBrowsable {
        /// The Snapshot name.
        name: String,
        /// Why it cannot be browsed (current phase / missing status).
        reason: String,
    },

    /// The Snapshot's repository cannot be derived (no pinned resolved ref, no
    /// owning repository).
    #[error(
        "cannot determine which repository Snapshot {snapshot:?} lives in: it has \
         neither a pinned status.resolved.repository nor a Repository/ClusterRepository \
         ownerReference. \
         Fix: this usually means the snapshot never ran — create a fresh one with \
         `kubectl kopiur snapshot now`, or browse a discovered snapshot"
    )]
    RepositoryUnderivable {
        /// The Snapshot name.
        snapshot: String,
    },

    /// The repository's credential Secret lives outside the namespace the
    /// session pod would run in (a pod cannot `envFrom` across namespaces).
    #[error(
        "the repository credential Secret {secret:?} lives in namespace \
         {secret_namespace}, but the browse session pod runs in namespace \
         {session_namespace}, and a pod cannot load a Secret from another namespace. \
         Fix: browse a snapshot in namespace {secret_namespace}, copy the Secret \
         into {session_namespace}, or read locally with --local"
    )]
    CredsOutsideSessionNamespace {
        /// The credential Secret name.
        secret: String,
        /// Where the Secret actually lives.
        secret_namespace: String,
        /// Where the session pod runs (the Snapshot's namespace).
        session_namespace: String,
    },

    /// A ClusterRepository credential reference pins no namespace, so the
    /// Secret cannot be located.
    #[error(
        "ClusterRepository {repository:?} references credential Secret {secret:?} \
         without a namespace, so it cannot be located from a browse session. \
         Cluster-scoped repositories must pin secretRef.namespace explicitly. \
         Fix: set the namespace on the ClusterRepository's secret references"
    )]
    ClusterRepoSecretNamespaceMissing {
        /// The credential Secret name.
        secret: String,
        /// The ClusterRepository name.
        repository: String,
    },

    /// The session pod failed before becoming ready.
    #[error(
        "the browse session pod (Job {job} in namespace {namespace}) failed before \
         becoming ready: {detail}. \
         The session connects to the repository read-only; the pod logs above name \
         the cause. Fix: check credentials and backend reachability \
         (`kubectl kopiur doctor`), then retry"
    )]
    SessionPodFailed {
        /// The session Job name.
        job: String,
        /// The Job's namespace.
        namespace: String,
        /// Failure detail (pod state + log tail when available).
        detail: String,
    },

    /// Waiting for the session pod to become ready timed out.
    #[error(
        "timed out after {after} waiting for the browse session pod (Job {job}) to \
         become ready. It may still be pulling its image or connecting to a slow \
         backend. Fix: inspect it with \
         `kubectl get pods -l batch.kubernetes.io/job-name={job}` (and its logs), \
         then retry — a warm session answers instantly"
    )]
    SessionNotReady {
        /// The session Job name.
        job: String,
        /// How long we waited, humanized.
        after: String,
    },

    /// An exec'd in-session kopia read failed.
    #[error(
        "the in-session kopia read failed ({what}): {stderr}. \
         The session is connected read-only, so this is a read/availability problem, \
         never a mutation. Fix: retry; if it persists, end the session \
         (`kubectl kopiur session end`) and start fresh"
    )]
    SessionExec {
        /// Which read failed.
        what: String,
        /// kopia's stderr (tail).
        stderr: String,
    },

    /// A user-supplied snapshot path is malformed or escapes the snapshot root.
    #[error(
        "invalid snapshot path {path:?}: {reason}. \
         Paths are relative to the snapshot root (e.g. `sub/file.txt`); `..` and \
         absolute paths are not allowed. \
         Fix: pass a relative path — list the root first with `kubectl kopiur ls <snapshot>`"
    )]
    InvalidPath {
        /// The offending path.
        path: String,
        /// What is wrong with it.
        reason: String,
    },

    /// A snapshot path does not exist.
    #[error(
        "path {path:?} does not exist in this snapshot. \
         Fix: list the directory with `kubectl kopiur ls <snapshot> [dir]` and check \
         the spelling (names are case-sensitive)"
    )]
    PathNotFound {
        /// The missing path.
        path: String,
    },

    /// `cat`/`download` was pointed at a directory.
    #[error(
        "{path:?} is a directory; cat/download read files. \
         Fix: list it with `kubectl kopiur ls <snapshot> {path}`, then name a file inside it"
    )]
    IsADirectory {
        /// The directory path.
        path: String,
    },

    /// `cat`/`download` was pointed at a non-regular-file entry (symlink, …).
    #[error(
        "{path:?} is not a regular file (kopia entry type {entry_type:?}), so its \
         bytes cannot be streamed. Fix: only regular files can be read; list the \
         directory with `kubectl kopiur ls` to see entry types"
    )]
    NotAFile {
        /// The entry path.
        path: String,
        /// The kopia entry type (`d`, `s`, …).
        entry_type: String,
    },

    /// The kopia snapshot id pinned in status is gone from the repository.
    #[error(
        "kopia snapshot {id} is not in the repository catalog (it may have been \
         expired by retention or deleted out-of-band). \
         Fix: pick a current snapshot with `kubectl kopiur snapshots list`"
    )]
    SnapshotMissingInRepo {
        /// The kopia snapshot manifest id.
        id: String,
    },

    /// kopia produced output the CLI could not interpret.
    #[error(
        "unexpected kopia output while reading {what}: {detail}. \
         Fix: retry; if it persists this is likely a kopiur/kopia version mismatch — \
         report it at https://github.com/home-operations/kopiur/issues"
    )]
    UnexpectedKopiaOutput {
        /// What was being read.
        what: String,
        /// Why the output couldn't be interpreted.
        detail: String,
    },

    /// An I/O error while copying bytes between the session pod and the caller.
    #[error("I/O error while {what}: {source}")]
    StreamIo {
        /// What was being copied.
        what: String,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },
}

/// Coarse class of a failure, for HTTP status mapping and retry decisions.
/// The CLI never needs this (it prints the message); a server does — the
/// message text is for humans, this is for the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpsErrorKind {
    /// RBAC refused the caller (403).
    Forbidden,
    /// The named object, path, or catalog entry does not exist (404).
    NotFound,
    /// The kopiur CRDs are missing from the cluster — an install problem, not
    /// a request problem.
    KindNotInstalled,
    /// An admission webhook rejected the object.
    Admission,
    /// The request cannot be satisfied as stated because the cluster's shape
    /// conflicts with it (cross-namespace reference, ambiguous target).
    Conflict,
    /// The caller's input is malformed or names something invalid (400).
    Invalid,
    /// A dependency (the API server, the session pod, kopia) failed or was
    /// unreachable — usually retryable.
    Upstream,
    /// A bounded wait expired; the work itself may still be running.
    Timeout,
    /// A kopiur bug: serialization failures and output we cannot interpret.
    Internal,
}

impl OpsError {
    /// This failure's coarse class. Exhaustive — a new variant must pick a
    /// kind here before it compiles, so no failure can reach a server without
    /// a deliberate status-code decision.
    pub fn kind(&self) -> OpsErrorKind {
        match self {
            OpsError::Forbidden { .. } => OpsErrorKind::Forbidden,
            OpsError::NotFound { .. }
            | OpsError::SnapshotMissingInRepo { .. }
            | OpsError::PathNotFound { .. } => OpsErrorKind::NotFound,
            OpsError::KindNotInstalled { .. } => OpsErrorKind::KindNotInstalled,
            OpsError::AdmissionDenied { .. } => OpsErrorKind::Admission,
            OpsError::RepoOutsideSessionNamespace { .. }
            | OpsError::CredsOutsideSessionNamespace { .. }
            | OpsError::ClusterRepoSecretNamespaceMissing { .. }
            | OpsError::AmbiguousTarget { .. } => OpsErrorKind::Conflict,
            OpsError::SelectorMatchedNothing { .. }
            | OpsError::UnknownPolicyRepository { .. }
            | OpsError::InvalidPath { .. }
            | OpsError::IsADirectory { .. }
            | OpsError::NotADirectory { .. }
            | OpsError::NotAFile { .. }
            | OpsError::SnapshotNotBrowsable { .. }
            | OpsError::RepositoryUnderivable { .. } => OpsErrorKind::Invalid,
            OpsError::WaitTimeout { .. } | OpsError::SessionNotReady { .. } => {
                OpsErrorKind::Timeout
            }
            OpsError::Api { .. }
            | OpsError::SessionExec { .. }
            | OpsError::SessionPodFailed { .. }
            | OpsError::MoverImageUnresolvable { .. }
            | OpsError::OperatorNamespaceUnresolvable { .. }
            | OpsError::CaBundleUnresolvable { .. }
            | OpsError::GoneWhileWaiting { .. } => OpsErrorKind::Upstream,
            OpsError::Serialization { .. }
            | OpsError::UnexpectedKopiaOutput { .. }
            | OpsError::StreamIo { .. } => OpsErrorKind::Internal,
        }
    }
}

/// Human-readable scope suffix for error messages: `" in namespace x"` for a
/// namespaced call, `""` for a cluster-scoped one.
pub fn scope_suffix(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!(" in namespace {ns}"),
        None => String::new(),
    }
}

/// The API server's NotFoundHandler message when the URL's resource *type* is
/// unknown (the CRD is absent). An object-level 404 instead names the object
/// (`snapshots.kopiur… "x" not found`). kube's `Status` carries no structured
/// discriminator between the two, so this message match is the only signal —
/// single definition here, exercised by the tests below.
const KIND_NOT_FOUND_NEEDLE: &str = "could not find the requested resource";

/// Classify a `kube::Error` from a `{verb} {resource}` call into the matching
/// [`OpsError`] variant, so every command surfaces the same actionable
/// messages without forking the mapping logic. Pass `name` for object-level
/// calls (get/patch/delete) so their 404 maps to [`OpsError::NotFound`]; a 404
/// whose message says the *resource type* is unknown maps to
/// [`OpsError::KindNotInstalled`] either way.
pub fn classify_kube(
    verb: &'static str,
    kind: &'static str,
    resource: &'static str,
    namespace: Option<&str>,
    name: Option<&str>,
    source: kube::Error,
) -> OpsError {
    let scope = scope_suffix(namespace);
    match &source {
        // An admission-webhook denial (apiserver relays it as 400/403 with the
        // webhook's message). Checked before the RBAC arm — a denial can be 403.
        kube::Error::Api(ae) if ae.message.contains("denied the request") => {
            OpsError::AdmissionDenied {
                message: ae.message.clone(),
            }
        }
        kube::Error::Api(ae) if ae.code == 403 => OpsError::Forbidden {
            verb,
            resource,
            scope,
            source: Box::new(source),
        },
        kube::Error::Api(ae) if ae.code == 404 && ae.message.contains(KIND_NOT_FOUND_NEEDLE) => {
            OpsError::KindNotInstalled {
                kind,
                source: Box::new(source),
            }
        }
        kube::Error::Api(ae) if ae.code == 404 => match name {
            Some(n) => OpsError::NotFound {
                kind,
                plural: resource,
                name: n.to_string(),
                scope,
                scope_flag: namespace.map(|ns| format!(" -n {ns}")).unwrap_or_default(),
            },
            // A collection-level 404 that doesn't carry the unknown-type
            // message: don't guess, surface it as a plain API failure.
            None => OpsError::Api {
                verb,
                resource,
                scope,
                source: Box::new(source),
            },
        },
        _ => OpsError::Api {
            verb,
            resource,
            scope,
            source: Box::new(source),
        },
    }
}

#[cfg(test)]
mod tests;
