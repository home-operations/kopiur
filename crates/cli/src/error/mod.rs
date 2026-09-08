//! The CLI's typed error surface. One exhaustive enum; every message states
//! what failed, why, and how to fix it (the message text is unit-tested).
//!
//! Only failures that are genuinely CLI-only live here — loading a kubeconfig,
//! following a log stream, translating VolSync input, and the whole `--local`
//! browse transport, which reads credentials with the caller's own RBAC and so
//! cannot exist in a server. Everything a front end shares with the web UI is a
//! [`kopiur_ops::OpsError`] and reaches the user verbatim through
//! [`CliError::Ops`].

/// Everything `kubectl kopiur` can fail with. Exhaustive — a new failure mode
/// is a new variant, never a stringly-typed catch-all.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The kubeconfig could not be loaded or the requested context resolved.
    #[error(
        "could not load a Kubernetes client configuration: {source}. \
         kubectl-kopiur reads the same configuration kubectl does \
         ($KUBECONFIG, ~/.kube/config, or in-cluster). \
         Fix: check --kubeconfig/--context, or verify your setup with \
         `kubectl config current-context`"
    )]
    KubeConfig {
        /// The underlying kube config/client construction error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A log stream broke mid-flight (network blip, apiserver restart).
    #[error(
        "the log stream was interrupted: {source}. \
         The connection to the API server dropped mid-stream; the run itself is unaffected. \
         Fix: re-run the same `kubectl kopiur logs` command to resume following"
    )]
    LogStreamInterrupted {
        /// The underlying stream error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A migration input (VolSync object / restic Secret) can't be translated.
    #[error("{what}. Fix: {fix}")]
    MigrationInput {
        /// What is wrong with the input.
        what: String,
        /// What to do about it.
        fix: String,
    },

    /// `-A` was passed to a command that targets exactly one object.
    #[error(
        "{command} targets a single object in one namespace, so -A/--all-namespaces \
         does not apply. Fix: drop -A and pass -n <namespace> instead"
    )]
    AllNamespacesNotApplicable {
        /// The command that rejected `-A`.
        command: &'static str,
    },

    // --- the `--local` browse transport (CLI-only: it needs `get secrets`) ---
    /// `--local` was passed but no kopia binary is available.
    #[error(
        "--local needs a kopia binary on this machine, but {bin:?} was not found. \
         Fix: install kopia (https://kopia.io/docs/installation/) or pass \
         --kopia-bin PATH — or drop --local to use the in-cluster session, \
         which needs no local kopia"
    )]
    LocalKopiaMissing {
        /// The binary that was looked for.
        bin: String,
    },

    /// A `--local` kopia invocation failed.
    #[error(
        "--local kopia operation failed ({what}): {source}. \
         --local talks to the backend FROM THIS MACHINE with the repository's \
         credentials. Fix: verify the endpoint is reachable from here (in-cluster-only \
         endpoints need a port-forward) and the credentials Secret is valid — or drop \
         --local to read through the in-cluster session"
    )]
    LocalKopia {
        /// Which operation failed.
        what: String,
        /// The kopia client error.
        #[source]
        source: Box<kopiur_kopia::KopiaError>,
    },

    /// `--local` cannot mount a cluster-volume filesystem repository.
    #[error(
        "--local cannot read repository {repository:?}: its filesystem backend lives \
         on a cluster volume (PVC/inline NFS) this machine cannot mount. \
         Fix: drop --local and use the in-cluster session, which mounts the \
         repository volume read-only"
    )]
    LocalRepoVolume {
        /// The repository name.
        repository: String,
    },

    /// `--local` cannot authenticate a workload-identity repository.
    #[error(
        "--local cannot read repository {repository:?}: its backend authenticates via workload \
         identity (ServiceAccount {service_account:?}), whose federated credentials exist only \
         inside a pod running as that ServiceAccount — there is no Secret to copy here. Fix: \
         drop --local and use the in-cluster session, which runs as the federated ServiceAccount"
    )]
    LocalWorkloadIdentity {
        /// The repository name.
        repository: String,
        /// The federated ServiceAccount the backend names.
        service_account: String,
    },

    /// Reading the credential Secret for `--local` was refused.
    #[error(
        "forbidden: cannot get Secret {secret:?} in namespace {namespace}: {source}. \
         --local copies the repository credentials onto this machine, which needs \
         `get` on `secrets` — RBAC the in-cluster session path deliberately does NOT \
         need. Fix: ask a cluster admin for `get secrets` in {namespace}, or drop --local"
    )]
    SecretsForbidden {
        /// The Secret name.
        secret: String,
        /// Its namespace.
        namespace: String,
        /// The API server's error.
        #[source]
        source: Box<kube::Error>,
    },

    /// A download wrote fewer/more bytes than the snapshot manifest records.
    #[error(
        "download of {path:?} is incomplete: expected {expected} bytes, wrote {actual}. \
         The partial file at {dest} was removed so a truncated restore can't be \
         mistaken for the real one. Fix: retry; if it persists, verify the snapshot \
         (kopiur's verification, or `kopia snapshot verify`)"
    )]
    DownloadIncomplete {
        /// The snapshot path downloaded.
        path: String,
        /// Bytes the manifest records.
        expected: i64,
        /// Bytes actually written.
        actual: u64,
        /// Destination whose partial content was removed.
        dest: String,
    },

    /// A local filesystem operation (download dest, --local staging dir) failed.
    #[error(
        "local file operation failed ({what}): {source}. \
         Fix: check the path exists, is writable, and has free space, then retry"
    )]
    LocalIo {
        /// What was being done.
        what: String,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A failure raised by the shared operations layer; its text is already
    /// what/why/fix.
    #[error(transparent)]
    Ops(#[from] kopiur_ops::OpsError),
}

// The kube-error classifier and its scope-suffix helper moved to
// `kopiur_ops::error` when the operations layer was extracted; re-exported so
// every CLI call site keeps resolving them through `crate::error`. They build
// `OpsError`s, which `?` converts via [`CliError::Ops`].
pub use kopiur_ops::error::{classify_kube, scope_suffix};

#[cfg(test)]
mod tests;
