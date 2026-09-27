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
        "could not load the Kubernetes config: {source}. \
         Fix: check --kubeconfig/--context, or run `kubectl config current-context`"
    )]
    KubeConfig {
        /// The underlying kube config/client construction error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A log stream broke mid-flight (network blip, apiserver restart).
    #[error(
        "the log stream was interrupted: {source}. The run itself is unaffected. \
         Fix: re-run the same `kubectl kopiur logs` command"
    )]
    LogStreamInterrupted {
        /// The underlying stream error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The `restore` flags did not map onto exactly one `RestoreTarget`.
    ///
    /// Two distinct causes share one variant because the fix is the same: clap's
    /// `target` ArgGroup should make both impossible, so reaching this means
    /// either the group config regressed or the requested target has no flag.
    /// Notably `target.streamExec` is manifest-only — there is no flag for it —
    /// so the message names it rather than leaving a user guessing.
    #[error(
        "no single restore target in the flags ({given}). \
         Fix: pass exactly one of --to-pvc, --create-pvc or --populator. \
         For a `target.streamExec` restore, apply a `Restore` manifest \
         (see docs/stream-sources.md)"
    )]
    UnresolvedRestoreTarget {
        /// Which target flags were seen, for the message.
        given: String,
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
        "{command} targets a single object, so -A does not apply. \
         Fix: drop -A and pass -n <namespace>"
    )]
    AllNamespacesNotApplicable {
        /// The command that rejected `-A`.
        command: &'static str,
    },

    // --- the `--local` browse transport (CLI-only: it needs `get secrets`) ---
    /// `--local` was passed but no kopia binary is available.
    #[error(
        "--local needs kopia on this machine, but {bin:?} was not found. \
         Fix: install kopia (https://kopia.io/docs/installation/), pass --kopia-bin PATH, \
         or drop --local"
    )]
    LocalKopiaMissing {
        /// The binary that was looked for.
        bin: String,
    },

    /// A `--local` kopia invocation failed.
    #[error(
        "--local kopia operation failed ({what}): {source}. \
         Fix: check the backend is reachable from this machine and the credentials \
         are valid, or drop --local"
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
        "--local cannot read repository {repository:?}: it is on a cluster volume \
         (PVC/NFS). Fix: drop --local to use the in-cluster session"
    )]
    LocalRepoVolume {
        /// The repository name.
        repository: String,
    },

    /// `--local` cannot authenticate a workload-identity repository.
    #[error(
        "--local cannot read repository {repository:?}: it uses workload identity \
         (ServiceAccount {service_account:?}), which only works inside the cluster. \
         Fix: drop --local to use the in-cluster session"
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
         --local needs `get` on `secrets`. Fix: ask an admin for that access, or drop --local"
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
         The partial file at {dest} was removed. Fix: retry; if it keeps failing, \
         run `kopia snapshot verify`"
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
         Fix: check the path exists, is writable, and has free space"
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
