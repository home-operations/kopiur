//! The single place for the web UI's runtime configuration: the clap surface
//! ([`UiArgs`]), the resolved config the rest of the crate consumes
//! ([`UiConfig`]), and the names of every environment variable `kopiur-ui`
//! reads. Every knob is a `--flag` with its `KOPIUR_*` env var as fallback
//! (flag > env > default); the env names are the chart contract and must never
//! change. OTLP/logging env names are owned by [`kopiur_telemetry::env`] and
//! re-exported here so callers have one import.
//!
//! # Why [`UiArgs::resolve`] fails closed
//!
//! `kopiur-ui` turns an HTTP request into an *impersonated* Kubernetes request.
//! Whoever the UI decides you are is who the apiserver enforces RBAC against, so
//! a configuration that leaves that decision ambiguous is not a degraded mode —
//! it is a privilege-escalation surface. Three rules follow from that, and each
//! one is a [`ConfigError`] rather than a warning:
//!
//! * **Identity must have a source.** With neither an identity header nor an
//!   explicit anonymous identity, every request would run as the UI's own
//!   ServiceAccount, which can impersonate. → [`ConfigError::NoIdentitySource`].
//! * **The trust boundary must be a real one.** Identity headers are trusted
//!   because a proxy sets them — but any pod in the cluster (or anyone with
//!   `services/proxy`) can also set them. Header mode therefore requires a shared
//!   secret the proxy proves it knows, or an explicit acknowledgement that
//!   something else (a mesh, a NetworkPolicy) closes that hole.
//!   → [`ConfigError::ProxySecretRequired`].
//! * **Reserved headers are never identity inputs.** A header named
//!   `Impersonate-User`, `Authorization` or `Cookie` either collides with the
//!   impersonation headers the UI emits itself or repurposes a credential the
//!   browser sends automatically. → [`ConfigError::ReservedHeaderName`].
//!
//! An empty string means "unset" everywhere, because a Helm chart routinely
//! renders a nulled value as `""` and clap consults the environment before
//! [`UiArgs::resolve`] can filter it.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser};
use http::header::HeaderName;

/// OTLP + logging env var names, owned by the telemetry crate so `kopiur-ui`'s
/// callers have a single import for the whole environment surface.
pub use kopiur_telemetry::env::{
    KOPIUR_LOG_FORMAT, KOPIUR_OTEL_STRICT, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_HEADERS,
    OTEL_EXPORTER_OTLP_PROTOCOL, RUST_LOG,
};

/// `metadata.managedFields` owner for every patch and apply `kopiur-ui` sends.
/// Distinct from the controller's manager so a field the UI set is attributable
/// to a human action in the UI, not to a reconcile.
pub const FIELD_MANAGER: &str = "kopiur-ui";

// --- bind addresses ---------------------------------------------------------

/// Address the user-facing HTTP server (the SPA + `/api`) binds to.
pub const ADDR_ENV: &str = "KOPIUR_UI_ADDR";

/// Default for [`ADDR_ENV`]. Dual-stack `[::]` so one bind serves IPv4 and IPv6
/// (a wildcard IPv6 bind also accepts IPv4 on Linux with the default
/// `net.ipv6.bindv6only=0`); set `0.0.0.0:8090` only where IPv6 is disabled in
/// the pod network namespace.
pub const DEFAULT_ADDR: &str = "[::]:8090";

/// Address the ops server (`/metrics`, `/healthz`, `/readyz`) binds to.
///
/// Deliberately a SECOND listener rather than routes on the app port: probes and
/// Prometheus must stay reachable from the monitoring namespace while the app
/// port is restricted to the authenticating proxy by a NetworkPolicy, and
/// `/metrics` must never sit behind the identity middleware.
pub const OPS_ADDR_ENV: &str = "KOPIUR_UI_OPS_ADDR";

/// Default for [`OPS_ADDR_ENV`]. See [`DEFAULT_ADDR`] on the `[::]` choice.
pub const DEFAULT_OPS_ADDR: &str = "[::]:8091";

// --- identity ---------------------------------------------------------------

/// Header carrying the authenticated username, injected by the proxy in front of
/// the UI (e.g. `X-Forwarded-User`). Setting it selects header mode.
pub const USER_HEADER_ENV: &str = "KOPIUR_UI_USER_HEADER";

/// Header carrying the caller's groups, split on [`GROUPS_SEPARATOR_ENV`].
/// Optional: an identity with no groups is valid.
pub const GROUPS_HEADER_ENV: &str = "KOPIUR_UI_GROUPS_HEADER";

/// Separator the groups header uses. oauth2-proxy emits `,`; some proxies use
/// `|` or `;`.
pub const GROUPS_SEPARATOR_ENV: &str = "KOPIUR_UI_GROUPS_SEPARATOR";

/// Default for [`GROUPS_SEPARATOR_ENV`].
pub const DEFAULT_GROUPS_SEPARATOR: &str = ",";

/// Header carrying the caller's email address. Display only — never
/// impersonated, never used for authorization.
pub const EMAIL_HEADER_ENV: &str = "KOPIUR_UI_EMAIL_HEADER";

/// Comma-separated `userextras` keys the UI may impersonate. Each one becomes a
/// `userextras/<key>` resource in the UI's ClusterRole, so the RBAC is an exact
/// list and never `userextras/*`.
pub const IMPERSONATE_EXTRA_KEYS_ENV: &str = "KOPIUR_UI_IMPERSONATE_EXTRA_KEYS";

/// Comma-separated allowlist of groups the UI may impersonate. Unset means "any
/// group the proxy asserts"; set, it both filters inbound groups and renders as
/// `resourceNames` on the `groups` impersonate rule, so the apiserver enforces
/// the same list the UI does.
pub const ALLOWED_GROUPS_ENV: &str = "KOPIUR_UI_ALLOWED_GROUPS";

/// Username every request runs as when no identity header is configured
/// (anonymous-only mode), or when [`ANONYMOUS_FALLBACK_ENV`] lets a
/// header-mode request without the header through.
pub const ANONYMOUS_USER_ENV: &str = "KOPIUR_UI_ANONYMOUS_USER";

/// Comma-separated groups paired with [`ANONYMOUS_USER_ENV`].
pub const ANONYMOUS_GROUPS_ENV: &str = "KOPIUR_UI_ANONYMOUS_GROUPS";

/// Opt-in: in header mode, serve a request whose identity header is absent as
/// the anonymous identity instead of rejecting it with 401.
///
/// Off by default and never implicit — a silent downgrade from "the proxy said
/// who you are" to "everyone is this fixed user" is the failure mode this flag
/// exists to make deliberate and visible (it also shows up on
/// `kopiur_ui_requests_total{identity_source}`).
pub const ANONYMOUS_FALLBACK_ENV: &str = "KOPIUR_UI_ANONYMOUS_FALLBACK";

/// Path to a file holding the shared secret the proxy must present in
/// `X-Kopiur-Proxy-Token`. The chart mounts it from a Secret.
pub const PROXY_SECRET_FILE_ENV: &str = "KOPIUR_UI_PROXY_SECRET_FILE";

/// Acknowledge running header mode WITHOUT a proxy shared secret. Mirrors the
/// repository CRDs' `acknowledgeInsecure`: the unsafe posture stays reachable,
/// but only for someone who wrote down that they meant it.
pub const ACKNOWLEDGE_NO_PROXY_SECRET_ENV: &str = "KOPIUR_UI_ACKNOWLEDGE_NO_PROXY_SECRET";

// --- operator wiring --------------------------------------------------------

/// The operator's own namespace (the chart injects it via the downward API).
/// Names where browse-session Jobs land and where doctor's namespaced
/// controller check looks.
pub const OPERATOR_NAMESPACE_ENV: &str = "KOPIUR_NAMESPACE";

/// Container image browse-session pods run. Shares the controller's env name so
/// one chart value stamps both.
pub const MOVER_IMAGE_ENV: &str = "KOPIUR_MOVER_IMAGE";

/// Whether to back reads with watch-fed reflector stores under the UI's own
/// ServiceAccount (SAR-gated per identity) instead of one impersonated LIST per
/// request. On by default: it is the difference between a page load costing one
/// apiserver call and costing a dozen.
pub const CACHE_ENV: &str = "KOPIUR_UI_CACHE";

// --- browse sessions --------------------------------------------------------

/// How long an idle browse session pod lives before the operator reaps it.
pub const SESSION_TTL_ENV: &str = "KOPIUR_UI_SESSION_TTL";
/// Default for [`SESSION_TTL_ENV`].
pub const DEFAULT_SESSION_TTL: &str = "15m";

/// How long `POST …/session` waits for the session pod to become ready before
/// giving up. Generous: the pod may have to pull the mover image.
pub const SESSION_READY_TIMEOUT_ENV: &str = "KOPIUR_UI_SESSION_READY_TIMEOUT";
/// Default for [`SESSION_READY_TIMEOUT_ENV`].
pub const DEFAULT_SESSION_READY_TIMEOUT: &str = "300s";

/// How long a file download may make **no progress** before it is abandoned.
///
/// Not a total-transfer budget — a legitimate multi-gigabyte restore takes far
/// longer than any fixed deadline. This bounds the *silence*: if neither the
/// session pod (a hung kopia, a wedged exec websocket) nor the browser (a closed
/// laptop, a half-open TCP connection) moves a byte for this long, the copy is
/// dropped. Nothing else bounds `GET …/file` — it is deliberately exempt from
/// the API's request timeout, because a real download outlives one — so without
/// this a single stalled transfer holds a `pods/exec` slot, an apiserver
/// websocket and a kopia process indefinitely.
pub const DOWNLOAD_CHUNK_TIMEOUT_ENV: &str = "KOPIUR_UI_DOWNLOAD_CHUNK_TIMEOUT";
/// Default for [`DOWNLOAD_CHUNK_TIMEOUT_ENV`].
pub const DEFAULT_DOWNLOAD_CHUNK_TIMEOUT: &str = "60s";

/// Cap on concurrent session-pod CREATIONS. Bounds how fast a burst of browse
/// clicks can ask the cluster for pods.
pub const MAX_SESSION_STARTS_ENV: &str = "KOPIUR_UI_MAX_SESSION_STARTS";
/// Default for [`MAX_SESSION_STARTS_ENV`].
pub const DEFAULT_MAX_SESSION_STARTS: usize = 4;

/// Cap on concurrent `pods/exec` calls ONE identity may have in flight.
pub const MAX_EXEC_PER_IDENTITY_ENV: &str = "KOPIUR_UI_MAX_EXEC_PER_IDENTITY";
/// Default for [`MAX_EXEC_PER_IDENTITY_ENV`].
pub const DEFAULT_MAX_EXEC_PER_IDENTITY: usize = 4;

/// Process-wide cap on concurrent `pods/exec` calls, so many identities cannot
/// each sit under the per-identity cap and jointly exhaust the process.
pub const MAX_EXEC_GLOBAL_ENV: &str = "KOPIUR_UI_MAX_EXEC_GLOBAL";
/// Default for [`MAX_EXEC_GLOBAL_ENV`].
pub const DEFAULT_MAX_EXEC_GLOBAL: usize = 64;

// --- resource bounds --------------------------------------------------------

/// Largest single file `GET …/file` will stream, in bytes. Checked before
/// streaming starts, so an oversized request is a 413 rather than a
/// half-gigabyte of wasted egress.
pub const MAX_DOWNLOAD_BYTES_ENV: &str = "KOPIUR_UI_MAX_DOWNLOAD_BYTES";
/// Default for [`MAX_DOWNLOAD_BYTES_ENV`]: 1 GiB.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;

/// Largest kopia JSON manifest the UI will buffer from an exec, in bytes. A
/// directory listing with a million entries must fail as 422
/// `directory-too-large`, not as an OOM kill.
pub const MAX_MANIFEST_BYTES_ENV: &str = "KOPIUR_UI_MAX_MANIFEST_BYTES";
/// Default for [`MAX_MANIFEST_BYTES_ENV`]: 64 MiB.
pub const DEFAULT_MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// Largest `limit` the snapshot list endpoint accepts; a larger one is a 422
/// rather than a serialization of the whole fleet.
pub const SNAPSHOT_LIST_CAP_ENV: &str = "KOPIUR_UI_SNAPSHOT_LIST_CAP";
/// Default for [`SNAPSHOT_LIST_CAP_ENV`].
pub const DEFAULT_SNAPSHOT_LIST_CAP: usize = 5000;

/// How many per-identity impersonating `kube::Client`s to keep. Each entry holds
/// a connection pool, so this is a memory and socket bound, not just a cache size.
pub const CLIENT_CACHE_SIZE_ENV: &str = "KOPIUR_UI_CLIENT_CACHE_SIZE";
/// Default for [`CLIENT_CACHE_SIZE_ENV`].
pub const DEFAULT_CLIENT_CACHE_SIZE: usize = 256;

/// How long an idle cached client survives, so a departed user's impersonating
/// client does not linger indefinitely.
pub const CLIENT_CACHE_TTL_ENV: &str = "KOPIUR_UI_CLIENT_CACHE_TTL";
/// Default for [`CLIENT_CACHE_TTL_ENV`].
pub const DEFAULT_CLIENT_CACHE_TTL: &str = "10m";

/// How long a `SubjectAccessReview` answer is reused for an identity. Short by
/// design: a revoked RoleBinding must stop hiding behind the cache quickly.
pub const SAR_TTL_ENV: &str = "KOPIUR_UI_SAR_TTL";
/// Default for [`SAR_TTL_ENV`].
pub const DEFAULT_SAR_TTL: &str = "60s";

/// How many `SubjectAccessReview` answers to retain. The decision cache is keyed
/// by the whole identity, so without a bound a long-lived UI grows by one entry
/// per (user, groups, extras, verb, resource, namespace, name) ever seen — a
/// slow leak driven by how many people log in, not by how much they do.
pub const SAR_CACHE_SIZE_ENV: &str = "KOPIUR_UI_SAR_CACHE_SIZE";
/// Default for [`SAR_CACHE_SIZE_ENV`]. Generous: an entry is a few hundred bytes
/// and the point of the bound is to be finite, not tight.
pub const DEFAULT_SAR_CACHE_SIZE: usize = 4096;

// --- TLS / CORS -------------------------------------------------------------

/// PEM certificate chain for serving the app port over HTTPS. Optional: the
/// usual deployment terminates TLS at the proxy.
pub const TLS_CERT_ENV: &str = "KOPIUR_UI_TLS_CERT";

/// PEM private key matching [`TLS_CERT_ENV`]. Both or neither.
pub const TLS_KEY_ENV: &str = "KOPIUR_UI_TLS_KEY";

/// Comma-separated origins allowed to call `/api` cross-origin.
///
/// **Development only** — it exists so the Vite dev server on another port can
/// reach a locally-run backend. In production the SPA is same-origin, so leaving
/// this unset is what keeps the CSRF posture (`Sec-Fetch-Site`/`Origin` checks)
/// meaningful.
pub const CORS_ORIGINS_ENV: &str = "KOPIUR_UI_CORS_ORIGINS";

// --- the clap surface -------------------------------------------------------

/// `kopiur-ui`'s command-line/environment surface. Every field is a `--flag`
/// with a `KOPIUR_*` env fallback (flag > env > default); the env names are the
/// chart contract. Values are RAW here — empty-string filtering, parsing and
/// every fail-closed rule live in [`UiArgs::resolve`], because the chart may
/// render an env var as `""` and that has always meant "unset".
#[derive(Debug, Clone, Parser)]
#[command(
    name = "kopiur-ui",
    version,
    about = "Kopiur web UI backend: impersonating read/action API over the Kopiur CRDs"
)]
pub struct UiArgs {
    /// Bind address for the app server (the SPA and /api).
    #[arg(long, env = ADDR_ENV, default_value = DEFAULT_ADDR)]
    pub addr: String,

    /// Bind address for the ops server (/metrics, /healthz, /readyz).
    #[arg(long, env = OPS_ADDR_ENV, default_value = DEFAULT_OPS_ADDR)]
    pub ops_addr: String,

    /// Header the authenticating proxy puts the username in, e.g.
    /// X-Forwarded-User. Setting it selects header mode.
    #[arg(long, env = USER_HEADER_ENV)]
    pub user_header: Option<String>,

    /// Header carrying the caller's groups.
    #[arg(long, env = GROUPS_HEADER_ENV)]
    pub groups_header: Option<String>,

    /// Separator the groups header uses.
    #[arg(long, env = GROUPS_SEPARATOR_ENV, default_value = DEFAULT_GROUPS_SEPARATOR)]
    pub groups_separator: String,

    /// Header carrying the caller's email address (display only).
    #[arg(long, env = EMAIL_HEADER_ENV)]
    pub email_header: Option<String>,

    /// Comma-separated userextras keys the UI may impersonate.
    #[arg(long, env = IMPERSONATE_EXTRA_KEYS_ENV)]
    pub impersonate_extra_keys: Option<String>,

    /// Comma-separated allowlist of impersonatable groups.
    #[arg(long, env = ALLOWED_GROUPS_ENV)]
    pub allowed_groups: Option<String>,

    /// Username for anonymous-only mode (and for the opt-in header-mode fallback).
    #[arg(long, env = ANONYMOUS_USER_ENV)]
    pub anonymous_user: Option<String>,

    /// Comma-separated groups paired with --anonymous-user.
    #[arg(long, env = ANONYMOUS_GROUPS_ENV)]
    pub anonymous_groups: Option<String>,

    /// Opt-in: serve header-mode requests with no identity header as the
    /// anonymous identity instead of 401.
    ///
    /// Not `ArgAction::SetTrue`: that action cannot consume an env value, and
    /// the chart sets `KOPIUR_UI_ANONYMOUS_FALLBACK=true`/`false`.
    /// `num_args = 0..=1` keeps the bare `--anonymous-fallback` form working.
    #[arg(long, env = ANONYMOUS_FALLBACK_ENV, action = ArgAction::Set,
          num_args = 0..=1, default_value_t = false, default_missing_value = "true",
          value_parser = parse_flag_bool)]
    pub anonymous_fallback: bool,

    /// File holding the shared secret the proxy presents in X-Kopiur-Proxy-Token.
    #[arg(long, env = PROXY_SECRET_FILE_ENV)]
    pub proxy_secret_file: Option<String>,

    /// Acknowledge running header mode with no proxy shared secret.
    #[arg(long, env = ACKNOWLEDGE_NO_PROXY_SECRET_ENV, action = ArgAction::Set,
          num_args = 0..=1, default_value_t = false, default_missing_value = "true",
          value_parser = parse_flag_bool)]
    pub acknowledge_no_proxy_secret: bool,

    /// **Debug builds only.** Allow the configured anonymous identity to be a
    /// `system:` principal (including `system:masters`).
    ///
    /// This exists for one purpose: a local smoke run against a throwaway kind
    /// cluster, where the fastest way to see the UI work is to let it impersonate
    /// `kubernetes-admin`/`system:masters` rather than to author RBAC first. It is
    /// compiled out entirely by `#[cfg(debug_assertions)]`, so the released image
    /// has no such flag, no such environment variable, and no code path that reads
    /// one.
    ///
    /// It relaxes exactly one check — [`reject_system_identity`] over the
    /// *operator-chosen* `KOPIUR_UI_ANONYMOUS_*` values. It cannot loosen what a
    /// proxy may assert: header-mode principals go through
    /// [`crate::auth::identity::is_forbidden_principal`], which has no escape
    /// hatch at all.
    #[cfg(debug_assertions)]
    #[arg(long, action = ArgAction::Set, num_args = 0..=1, default_value_t = false,
          default_missing_value = "true", value_parser = parse_flag_bool, hide = true)]
    pub dev_allow_system_groups: bool,

    /// The operator's own namespace (the chart injects it via the downward API).
    #[arg(long = "operator-namespace", env = OPERATOR_NAMESPACE_ENV)]
    pub operator_namespace: Option<String>,

    /// Container image browse-session pods run.
    #[arg(long, env = MOVER_IMAGE_ENV)]
    pub mover_image: Option<String>,

    /// Back reads with watch-fed reflector stores instead of per-request LISTs.
    #[arg(long, env = CACHE_ENV, action = ArgAction::Set,
          num_args = 0..=1, default_value_t = true, default_missing_value = "true",
          value_parser = parse_flag_bool)]
    pub cache: bool,

    /// Idle lifetime of a browse-session pod (e.g. 15m).
    #[arg(long, env = SESSION_TTL_ENV, default_value = DEFAULT_SESSION_TTL)]
    pub session_ttl: String,

    /// How long to wait for a session pod to become ready (e.g. 300s).
    #[arg(long, env = SESSION_READY_TIMEOUT_ENV, default_value = DEFAULT_SESSION_READY_TIMEOUT)]
    pub session_ready_timeout: String,

    /// How long a download may make no progress before it is abandoned (e.g. 60s).
    #[arg(long, env = DOWNLOAD_CHUNK_TIMEOUT_ENV, default_value = DEFAULT_DOWNLOAD_CHUNK_TIMEOUT)]
    pub download_chunk_timeout: String,

    /// Cap on concurrent session-pod creations.
    #[arg(long, env = MAX_SESSION_STARTS_ENV, default_value_t = DEFAULT_MAX_SESSION_STARTS)]
    pub max_session_starts: usize,

    /// Cap on concurrent pods/exec calls per identity.
    #[arg(long, env = MAX_EXEC_PER_IDENTITY_ENV, default_value_t = DEFAULT_MAX_EXEC_PER_IDENTITY)]
    pub max_exec_per_identity: usize,

    /// Process-wide cap on concurrent pods/exec calls.
    #[arg(long, env = MAX_EXEC_GLOBAL_ENV, default_value_t = DEFAULT_MAX_EXEC_GLOBAL)]
    pub max_exec_global: usize,

    /// Largest single file download, in bytes.
    #[arg(long, env = MAX_DOWNLOAD_BYTES_ENV, default_value_t = DEFAULT_MAX_DOWNLOAD_BYTES)]
    pub max_download_bytes: u64,

    /// Largest kopia JSON manifest the UI will buffer, in bytes.
    #[arg(long, env = MAX_MANIFEST_BYTES_ENV, default_value_t = DEFAULT_MAX_MANIFEST_BYTES)]
    pub max_manifest_bytes: u64,

    /// Largest accepted `limit` on the snapshot list endpoint.
    #[arg(long, env = SNAPSHOT_LIST_CAP_ENV, default_value_t = DEFAULT_SNAPSHOT_LIST_CAP)]
    pub snapshot_list_cap: usize,

    /// How many per-identity impersonating kube clients to keep.
    #[arg(long, env = CLIENT_CACHE_SIZE_ENV, default_value_t = DEFAULT_CLIENT_CACHE_SIZE)]
    pub client_cache_size: usize,

    /// Idle lifetime of a cached impersonating client (e.g. 10m).
    #[arg(long, env = CLIENT_CACHE_TTL_ENV, default_value = DEFAULT_CLIENT_CACHE_TTL)]
    pub client_cache_ttl: String,

    /// How long a SubjectAccessReview answer is reused (e.g. 60s).
    #[arg(long, env = SAR_TTL_ENV, default_value = DEFAULT_SAR_TTL)]
    pub sar_ttl: String,

    /// How many SubjectAccessReview answers to retain.
    #[arg(long, env = SAR_CACHE_SIZE_ENV, default_value_t = DEFAULT_SAR_CACHE_SIZE)]
    pub sar_cache_size: usize,

    /// PEM certificate chain for serving the app port over HTTPS.
    #[arg(long, env = TLS_CERT_ENV)]
    pub tls_cert: Option<String>,

    /// PEM private key matching --tls-cert.
    #[arg(long, env = TLS_KEY_ENV)]
    pub tls_key: Option<String>,

    /// Comma-separated cross-origin allowlist for /api. Development only.
    #[arg(long, env = CORS_ORIGINS_ENV)]
    pub cors_origins: Option<String>,
}

// --- the resolved config ----------------------------------------------------

/// Everything `kopiur-ui` needs at runtime, with every value already parsed and
/// every fail-closed rule already enforced. Built once in `main` and shared
/// behind an `Arc`; nothing downstream re-reads the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiConfig {
    /// Bind address for the app server.
    pub addr: SocketAddr,
    /// Bind address for the ops server.
    pub ops_addr: SocketAddr,
    /// How a request's identity is established, and what may be impersonated.
    pub auth: AuthConfig,
    /// The operator's own namespace, when known.
    pub operator_namespace: Option<String>,
    /// Container image browse-session pods run, when overridden.
    pub mover_image: Option<String>,
    /// Whether reads go through watch-fed reflector stores.
    pub cache_enabled: bool,
    /// Browse-session lifetimes and concurrency caps.
    pub session: SessionLimits,
    /// Largest single file download, in bytes.
    pub download_max_bytes: u64,
    /// How long a download may make no progress before it is abandoned.
    pub download_chunk_timeout: Duration,
    /// Largest buffered kopia JSON manifest, in bytes.
    pub manifest_max_bytes: u64,
    /// Largest accepted `limit` on the snapshot list endpoint.
    pub snapshot_list_cap: usize,
    /// Bounds on the per-identity impersonating client cache.
    pub client_cache: CacheLimits,
    /// How long a `SubjectAccessReview` answer is reused.
    pub sar_ttl: Duration,
    /// Maximum `SubjectAccessReview` answers retained across all identities.
    pub sar_cache_size: usize,
    /// Serving certificate, when the UI terminates TLS itself.
    pub tls: Option<TlsPaths>,
    /// Cross-origin allowlist for `/api`. Empty in every production deployment.
    pub cors_origins: Vec<String>,
}

/// How a request's identity is established, and the bounds on what the UI will
/// impersonate once it has one.
///
/// [`fmt::Debug`] is hand-written: `proxy_secret` is a credential, and the whole
/// config is logged at startup.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// Header mode or anonymous-only mode.
    pub mode: AuthMode,
    /// Separator splitting the groups header's value.
    pub groups_separator: String,
    /// Header carrying the caller's email address, when configured.
    pub email_header: Option<HeaderName>,
    /// `userextras` keys the UI may impersonate. Exactly the keys named here
    /// appear as `userextras/<key>` in the UI's ClusterRole.
    pub extra_keys: Vec<String>,
    /// Allowlist of impersonatable groups. `None` means "whatever the proxy
    /// asserts"; `Some` is enforced here AND as `resourceNames` in RBAC.
    pub allowed_groups: Option<BTreeSet<String>>,
    /// Shared secret the proxy must present in `X-Kopiur-Proxy-Token`. `None`
    /// only when the operator acknowledged running without one.
    pub proxy_secret: Option<Vec<u8>>,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("mode", &self.mode)
            .field("groups_separator", &self.groups_separator)
            .field("email_header", &self.email_header)
            .field("extra_keys", &self.extra_keys)
            .field("allowed_groups", &self.allowed_groups)
            // Never the bytes: this struct is inside `UiConfig`, which is logged
            // at startup and could appear in any `?err` chain.
            .field(
                "proxy_secret",
                &self.proxy_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Where a request's identity comes from. An enum, not a bag of `Option`s: every
/// call site `match`es it exhaustively, so "header mode with a fallback" and
/// "anonymous-only" can never be confused for one another, and a future third
/// mode cannot compile until every handler accounts for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// A proxy asserts the identity in headers. A request without the user
    /// header is a 401 unless `anonymous_fallback` is set.
    Headers {
        /// Header carrying the username.
        user: HeaderName,
        /// Header carrying the groups, when configured.
        groups: Option<HeaderName>,
        /// Opt-in identity used when the user header is absent.
        anonymous_fallback: Option<AnonymousIdentity>,
    },
    /// No identity header is configured: every request runs as this one fixed,
    /// explicitly-chosen identity.
    AnonymousOnly(AnonymousIdentity),
}

/// The fixed identity anonymous mode impersonates. Always explicit — the UI
/// never invents one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnonymousIdentity {
    /// Username to impersonate.
    pub user: String,
    /// Groups to impersonate alongside `user`.
    pub groups: Vec<String>,
}

/// Browse-session lifetimes and concurrency caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLimits {
    /// Idle lifetime of a session pod.
    pub ttl: Duration,
    /// How long `POST …/session` waits for the pod to become ready.
    pub ready_timeout: Duration,
    /// Cap on concurrent session-pod creations.
    pub max_starts: usize,
    /// Cap on concurrent `pods/exec` calls per identity.
    pub max_exec_per_identity: usize,
    /// Process-wide cap on concurrent `pods/exec` calls.
    pub max_exec_global: usize,
}

/// Bounds on the per-identity impersonating client cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheLimits {
    /// Maximum entries retained.
    pub size: usize,
    /// Idle lifetime of an entry.
    pub ttl: Duration,
}

/// Paths to the UI's own serving certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPaths {
    /// PEM certificate chain.
    pub cert: PathBuf,
    /// PEM private key.
    pub key: PathBuf,
}

// --- errors -----------------------------------------------------------------

/// Why a configuration was refused. Every message names the environment
/// variable to change, because the operator's next action is always to edit a
/// chart value or an env var — not to read this source.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Neither an identity header nor an explicit anonymous identity.
    #[error(
        "no identity source is configured: neither {USER_HEADER_ENV} nor {ANONYMOUS_USER_ENV} \
         is set. kopiur-ui turns every request into an impersonated Kubernetes request, so \
         with no way to tell callers apart it would have to run all of them as its own \
         ServiceAccount — which is allowed to impersonate anyone. Set {USER_HEADER_ENV} to \
         the header your authenticating proxy injects (e.g. X-Forwarded-User), or set \
         {ANONYMOUS_USER_ENV} to run deliberately as one fixed identity."
    )]
    NoIdentitySource,

    /// A configured identity header name is one the UI reserves.
    #[error(
        "the identity header '{header}' is reserved and cannot be used as an identity input. \
         'impersonate-*' is what kopiur-ui writes to the apiserver itself (accepting one \
         inbound would let a caller pick their own impersonation), and 'authorization' / \
         'cookie' are credentials a browser attaches automatically, so trusting either as an \
         asserted identity would let any origin speak for the user. Point \
         {USER_HEADER_ENV} / {GROUPS_HEADER_ENV} / {EMAIL_HEADER_ENV} at a header your proxy \
         sets explicitly, e.g. X-Forwarded-User."
    )]
    ReservedHeaderName {
        /// The offending header name, lowercased.
        header: String,
    },

    /// A configured header name is not a valid HTTP header name.
    #[error(
        "'{header}' is not a valid HTTP header name, so kopiur-ui cannot look it up on an \
         incoming request. Header names are visible ASCII without spaces or separators \
         (letters, digits, '-' and '_'). Fix the value of {USER_HEADER_ENV} / \
         {GROUPS_HEADER_ENV} / {EMAIL_HEADER_ENV}, e.g. X-Forwarded-User."
    )]
    InvalidHeaderName {
        /// The rejected header name.
        header: String,
        /// The underlying parse failure.
        source: http::header::InvalidHeaderName,
    },

    /// Header mode without a proxy shared secret and without an acknowledgement.
    #[error(
        "header mode is configured ({USER_HEADER_ENV} is set) but no proxy shared secret is: \
         {PROXY_SECRET_FILE_ENV} is empty. Identity headers are only trustworthy if nothing \
         else can set them, and in a cluster any pod — or anyone who can reach the Service, \
         including via the apiserver's services/proxy — can send the same headers directly to \
         kopiur-ui and become any user. Mount a shared secret and point \
         {PROXY_SECRET_FILE_ENV} at it (the chart's ui.auth.proxySecret.existingSecret), and \
         have the proxy send it as X-Kopiur-Proxy-Token. If some other control already \
         prevents direct access, set {ACKNOWLEDGE_NO_PROXY_SECRET_ENV}=true to say so \
         explicitly."
    )]
    ProxySecretRequired,

    /// The proxy secret file could not be read (or was empty).
    #[error(
        "the proxy shared secret at '{path}' could not be read, so kopiur-ui cannot verify \
         that requests really came from your proxy and refuses to start rather than accept \
         unverified identity headers. Check that {PROXY_SECRET_FILE_ENV} points at the \
         mounted Secret's key and that the file is non-empty and readable by the UI's user."
    )]
    ProxySecretUnreadable {
        /// Path that could not be read.
        path: PathBuf,
        /// The underlying IO failure.
        source: std::io::Error,
    },

    /// The anonymous fallback was requested without an anonymous identity.
    #[error(
        "{ANONYMOUS_FALLBACK_ENV} is true but {ANONYMOUS_USER_ENV} is not set, so there is no \
         identity to fall back TO — a request without the identity header would have nobody \
         to run as. Set {ANONYMOUS_USER_ENV} (and optionally {ANONYMOUS_GROUPS_ENV}) to the \
         identity unauthenticated requests should get, or set {ANONYMOUS_FALLBACK_ENV}=false \
         to reject them with 401 instead."
    )]
    AnonymousWithoutUser,

    /// A duration value was not `<integer><s|m|h>`.
    #[error(
        "{name}='{value}' is not a valid duration, so kopiur-ui cannot tell how long the \
         limit it controls should be. Use a whole number followed by a unit: s (seconds), m \
         (minutes) or h (hours) — for example 60s, 15m or 1h. Unset it to use the default."
    )]
    InvalidDuration {
        /// Environment variable the bad value came from.
        name: &'static str,
        /// The rejected value.
        value: String,
    },

    /// A bind address was not `host:port`.
    #[error(
        "{name}='{value}' is not a valid socket address, so kopiur-ui has no port to listen \
         on. Use host:port — [::]:8090 (IPv6/dual-stack, the default) or 0.0.0.0:8090 \
         (IPv4-only, for hosts where IPv6 is disabled). Unset it to use the default."
    )]
    InvalidAddr {
        /// Environment variable the bad value came from.
        name: &'static str,
        /// The rejected value.
        value: String,
        /// The underlying parse failure.
        source: std::net::AddrParseError,
    },

    /// `KOPIUR_UI_ALLOWED_GROUPS` was set but names no group.
    #[error(
        "{ALLOWED_GROUPS_ENV}='{value}' is set but contains no group name — only separators \
         and whitespace. Setting it means \"restrict impersonation to these groups\", and \
         restricting it to nothing would either deny every caller or, if it were read as \
         \"unset\", silently allow every group the proxy asserts; neither is what someone \
         who typed this meant. List the groups you want, comma-separated (e.g. \
         platform,sre), or unset {ALLOWED_GROUPS_ENV} entirely to accept whatever groups the \
         proxy asserts."
    )]
    InvalidAllowedGroups {
        /// The rejected value, as configured.
        value: String,
    },

    /// An anonymous user or group is inside Kubernetes' reserved `system:` space.
    #[error(
        "'{group}' cannot be used as an anonymous identity: it is inside Kubernetes' reserved \
         'system:' namespace (only system:authenticated is allowed there, and system:masters \
         never is). Impersonating it would hand every unauthenticated visitor a built-in \
         cluster identity — system:masters bypasses RBAC entirely. Set {ANONYMOUS_USER_ENV} / \
         {ANONYMOUS_GROUPS_ENV} to an ordinary user and groups, and grant them the \
         kopiur-ui-viewer or kopiur-ui-user role instead."
    )]
    ForbiddenAnonymousGroup {
        /// The rejected user or group name.
        group: String,
    },
}

// --- resolution -------------------------------------------------------------

/// Header names that can never be identity inputs. `impersonate-` is matched as
/// a PREFIX (`Impersonate-User`, `-Group`, `-Extra-*`, `-Uid`) — those are the
/// headers kopiur-ui writes to the apiserver, so honoring an inbound one would
/// let a caller choose their own impersonation.
const RESERVED_HEADER_PREFIXES: &[&str] = &["impersonate-"];

/// Header names that can never be identity inputs, matched exactly. Both are
/// credentials a browser attaches on its own.
const RESERVED_HEADER_NAMES: &[&str] = &["authorization", "cookie"];

/// The one `system:` identity an anonymous mode may use: every authenticated
/// request already carries it, so it grants nothing on its own.
const ALLOWED_SYSTEM_IDENTITY: &str = "system:authenticated";

/// Kubernetes' built-in super-user group, which bypasses RBAC entirely.
const SYSTEM_MASTERS: &str = "system:masters";

impl UiArgs {
    /// Whether the debug-only `--dev-allow-system-groups` hatch is open.
    ///
    /// A release build has no such flag, so this is a compile-time `false` there
    /// and the branch it guards folds away — the escape hatch cannot exist in a
    /// shipped image even as dead code.
    #[cfg(debug_assertions)]
    fn system_anonymous_allowed(&self) -> bool {
        self.dev_allow_system_groups
    }

    /// Release builds have no escape hatch. See the debug-build twin above.
    #[cfg(not(debug_assertions))]
    fn system_anonymous_allowed(&self) -> bool {
        false
    }

    /// Turn the raw flag/env surface into a validated [`UiConfig`], or explain
    /// why it cannot be one.
    ///
    /// With several mistakes present, the first one reported is the one this
    /// order picks, so the order is part of the diagnostics. It is, exactly as
    /// the body runs it:
    ///
    /// 1. Bind addresses → [`ConfigError::InvalidAddr`].
    /// 2. Durations and the allowed-group list → [`ConfigError::InvalidDuration`],
    ///    [`ConfigError::InvalidAllowedGroups`].
    /// 3. The anonymous identity's deny-list →
    ///    [`ConfigError::ForbiddenAnonymousGroup`]. Ahead of the header names
    ///    because it is checked for *any* configured anonymous identity, whether
    ///    or not a header mode ends up using it — see the body.
    /// 4. Header names → [`ConfigError::InvalidHeaderName`],
    ///    [`ConfigError::ReservedHeaderName`].
    /// 5. [`ConfigError::AnonymousWithoutUser`] — a fallback with nothing to fall
    ///    back to. Ahead of `NoIdentitySource` because it names something the
    ///    operator actually typed, rather than reporting the general absence it
    ///    causes.
    /// 6. Mode selection → [`ConfigError::NoIdentitySource`].
    /// 7. The proxy secret → [`ConfigError::ProxySecretUnreadable`],
    ///    [`ConfigError::ProxySecretRequired`]. Last because it only applies once
    ///    header mode has been established in step 6.
    pub fn resolve(self) -> Result<UiConfig, ConfigError> {
        let addr = parse_addr(ADDR_ENV, &self.addr, DEFAULT_ADDR)?;
        let ops_addr = parse_addr(OPS_ADDR_ENV, &self.ops_addr, DEFAULT_OPS_ADDR)?;

        let session = SessionLimits {
            ttl: parse_duration_or_default(
                SESSION_TTL_ENV,
                &self.session_ttl,
                DEFAULT_SESSION_TTL,
            )?,
            ready_timeout: parse_duration_or_default(
                SESSION_READY_TIMEOUT_ENV,
                &self.session_ready_timeout,
                DEFAULT_SESSION_READY_TIMEOUT,
            )?,
            max_starts: self.max_session_starts,
            max_exec_per_identity: self.max_exec_per_identity,
            max_exec_global: self.max_exec_global,
        };
        let client_cache = CacheLimits {
            size: self.client_cache_size,
            ttl: parse_duration_or_default(
                CLIENT_CACHE_TTL_ENV,
                &self.client_cache_ttl,
                DEFAULT_CLIENT_CACHE_TTL,
            )?,
        };
        let sar_ttl = parse_duration_or_default(SAR_TTL_ENV, &self.sar_ttl, DEFAULT_SAR_TTL)?;

        // An empty value is "unset" as everywhere else, but a non-empty value
        // that parses to no groups (`","`, `" , "`) is a typo, and both readings
        // of it are wrong: "allow nothing" locks every caller out, "unset" quietly
        // widens impersonation to every group the proxy asserts. Refuse instead.
        // Read the debug-only dev hatch BEFORE any field is moved out of `self`.
        let system_anonymous_allowed = self.system_anonymous_allowed();
        let allowed_groups = match nonempty(self.allowed_groups) {
            Some(raw) => {
                let groups: BTreeSet<String> = csv(Some(&raw)).into_iter().collect();
                if groups.is_empty() {
                    return Err(ConfigError::InvalidAllowedGroups { value: raw });
                }
                Some(groups)
            }
            None => None,
        };

        // The anonymous identity is validated whenever one is configured at all,
        // even in header mode where it may never be used: a `system:` identity
        // sitting in the environment is a misconfiguration waiting for someone to
        // flip KOPIUR_UI_ANONYMOUS_FALLBACK on, and finding it then is too late.
        let anonymous_user = nonempty(self.anonymous_user);
        let anonymous_groups = csv(self.anonymous_groups.as_deref());
        if system_anonymous_allowed {
            tracing::warn!(
                "--dev-allow-system-groups is set: the anonymous identity may be a system: \
                 principal. This is a debug-build-only escape hatch for a local kind smoke \
                 and must never be used against a cluster you care about."
            );
        } else {
            if let Some(user) = &anonymous_user {
                reject_system_identity(user)?;
            }
            for group in &anonymous_groups {
                reject_system_identity(group)?;
            }
        }
        let anonymous = anonymous_user.map(|user| AnonymousIdentity {
            user,
            groups: anonymous_groups,
        });

        let user_header = header_name(nonempty(self.user_header))?;
        let groups_header = header_name(nonempty(self.groups_header))?;
        let email_header = header_name(nonempty(self.email_header))?;

        // A fallback with nothing to fall back to is a direct contradiction of
        // what the operator typed, so it is reported ahead of the more general
        // "nothing is configured" error.
        if self.anonymous_fallback && anonymous.is_none() {
            return Err(ConfigError::AnonymousWithoutUser);
        }

        let (mode, proxy_secret) = match (user_header, anonymous) {
            (Some(user), anonymous) => {
                let mode = AuthMode::Headers {
                    user,
                    groups: groups_header,
                    anonymous_fallback: self.anonymous_fallback.then_some(anonymous).flatten(),
                };
                (
                    mode,
                    resolve_proxy_secret(
                        nonempty(self.proxy_secret_file),
                        self.acknowledge_no_proxy_secret,
                    )?,
                )
            }
            // No identity header means nothing can forge one, so anonymous-only
            // mode needs no proxy secret: there is no trust boundary to protect.
            (None, Some(anonymous)) => (AuthMode::AnonymousOnly(anonymous), None),
            (None, None) => return Err(ConfigError::NoIdentitySource),
        };

        let groups_separator = if self.groups_separator.is_empty() {
            DEFAULT_GROUPS_SEPARATOR.to_string()
        } else {
            self.groups_separator
        };

        let tls = match (nonempty(self.tls_cert), nonempty(self.tls_key)) {
            (Some(cert), Some(key)) => Some(TlsPaths {
                cert: PathBuf::from(cert),
                key: PathBuf::from(key),
            }),
            // Half a keypair is a typo, not a request for TLS. Serving plain HTTP
            // is the documented default, and the startup log says which is in use.
            _ => None,
        };

        Ok(UiConfig {
            addr,
            ops_addr,
            auth: AuthConfig {
                mode,
                groups_separator,
                email_header,
                extra_keys: csv(self.impersonate_extra_keys.as_deref()),
                allowed_groups,
                proxy_secret,
            },
            operator_namespace: nonempty(self.operator_namespace),
            mover_image: nonempty(self.mover_image),
            cache_enabled: self.cache,
            session,
            download_max_bytes: self.max_download_bytes,
            download_chunk_timeout: parse_duration_or_default(
                DOWNLOAD_CHUNK_TIMEOUT_ENV,
                &self.download_chunk_timeout,
                DEFAULT_DOWNLOAD_CHUNK_TIMEOUT,
            )?,
            manifest_max_bytes: self.max_manifest_bytes,
            snapshot_list_cap: self.snapshot_list_cap,
            client_cache,
            sar_ttl,
            sar_cache_size: self.sar_cache_size,
            tls,
            cors_origins: csv(self.cors_origins.as_deref()),
        })
    }
}

/// An empty string means "unset": a chart routinely renders a nulled value as
/// `""`, and clap consults the environment before this filter can run.
fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

/// Split a comma-separated value, trimming each item and dropping empties, so
/// `" platform , sre ,"` is two groups rather than four with two blanks.
fn csv(value: Option<&str>) -> Vec<String> {
    value
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a bind address, treating an empty value as the default.
fn parse_addr(
    name: &'static str,
    value: &str,
    default: &'static str,
) -> Result<SocketAddr, ConfigError> {
    let value = if value.is_empty() { default } else { value };
    value
        .parse::<SocketAddr>()
        .map_err(|source| ConfigError::InvalidAddr {
            name,
            value: value.to_string(),
            source,
        })
}

/// Parse a configured header name, rejecting the ones the UI reserves.
///
/// The reserved check covers every identity header, not just the user header:
/// `Impersonate-*` is what kopiur-ui writes to the apiserver (so honoring an
/// inbound one would let a caller pick their own impersonation), and
/// `Authorization`/`Cookie` are credentials the browser attaches by itself — none
/// of which becomes safe just because it was configured as the *groups* header.
fn header_name(value: Option<String>) -> Result<Option<HeaderName>, ConfigError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let parsed =
        HeaderName::try_from(raw.as_str()).map_err(|source| ConfigError::InvalidHeaderName {
            header: raw.clone(),
            source,
        })?;
    // `HeaderName` is always lowercase, so this comparison is case-insensitive
    // over whatever the operator typed.
    let lower = parsed.as_str();
    if RESERVED_HEADER_NAMES.contains(&lower)
        || RESERVED_HEADER_PREFIXES
            .iter()
            .any(|p| lower.starts_with(p))
    {
        return Err(ConfigError::ReservedHeaderName {
            header: lower.to_string(),
        });
    }
    Ok(Some(parsed))
}

/// Refuse a `system:` user or group in an anonymous identity.
///
/// `system:authenticated` is the one exception: every authenticated request
/// already carries it, so impersonating it grants nothing on its own.
fn reject_system_identity(value: &str) -> Result<(), ConfigError> {
    let forbidden = value == SYSTEM_MASTERS
        || (value.starts_with("system:") && value != ALLOWED_SYSTEM_IDENTITY);
    if forbidden {
        return Err(ConfigError::ForbiddenAnonymousGroup {
            group: value.to_string(),
        });
    }
    Ok(())
}

/// Read the proxy shared secret, or establish that running without one was
/// deliberate.
///
/// A file that is empty or all whitespace counts as NO secret, not as an empty
/// one: an empty expected value would authenticate any caller that sends an empty
/// `X-Kopiur-Proxy-Token`, which is strictly worse than having no check at all.
/// Surrounding whitespace is trimmed because a Secret written with `echo` carries
/// a trailing newline the proxy will not send.
fn resolve_proxy_secret(
    path: Option<String>,
    acknowledged: bool,
) -> Result<Option<Vec<u8>>, ConfigError> {
    if let Some(path) = path {
        let path = PathBuf::from(path);
        let raw = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::ProxySecretUnreadable { path, source })?;
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.as_bytes().to_vec()));
        }
    }
    if acknowledged {
        return Ok(None);
    }
    Err(ConfigError::ProxySecretRequired)
}

/// Parse a duration, treating an empty value as the (always valid) default.
fn parse_duration_or_default(
    name: &'static str,
    value: &str,
    default: &'static str,
) -> Result<Duration, ConfigError> {
    parse_duration(name, if value.is_empty() { default } else { value })
}

/// Parse `<whole number><unit>` where unit is `s`, `m` or `h`.
///
/// Hand-rolled rather than pulled from a crate because the accepted surface is
/// exactly what the chart documents, and anything else — a float, a bare number,
/// a unit we do not support — must be a startup error naming the env var rather
/// than a silently different timeout.
fn parse_duration(name: &'static str, value: &str) -> Result<Duration, ConfigError> {
    let invalid = || ConfigError::InvalidDuration {
        name,
        value: value.to_string(),
    };

    let mut chars = value.chars();
    let unit = chars.next_back().ok_or_else(invalid)?;
    let digits = chars.as_str();
    let scale = match unit {
        's' => 1u64,
        'm' => 60,
        'h' => 3600,
        _ => return Err(invalid()),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let count: u64 = digits.parse().map_err(|_| invalid())?;
    let secs = count.checked_mul(scale).ok_or_else(invalid)?;
    Ok(Duration::from_secs(secs))
}

/// clap value parser for the boolean flags.
///
/// An empty value maps to `false`, for every flag. That is right for the two
/// opt-in flags (`--anonymous-fallback`, `--acknowledge-no-proxy-secret`), whose
/// default is already `false`.
///
/// It is **wrong for `--cache`**, whose default is `true`: a chart that renders
/// `KOPIUR_UI_CACHE=""` (a nulled Helm value) silently turns the read cache off
/// rather than leaving it at the default. clap consults the environment and runs
/// this parser before [`UiArgs::resolve`] ever sees the value, so `default_value_t`
/// cannot rescue it and the fix has to be here or in the flag's declaration —
/// Task 8 owns it. Until then, set `KOPIUR_UI_CACHE` to `true`/`false` explicitly
/// or leave it out of the environment entirely; do not render it as `""`.
fn parse_flag_bool(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "" => Ok(false),
        "true" | "t" | "yes" | "y" | "on" | "1" => Ok(true),
        "false" | "f" | "no" | "n" | "off" | "0" => Ok(false),
        _ => Err(format!(
            "'{value}' is not a valid boolean; use true/false (also accepted: 1/0, yes/no, \
             on/off); unset it or leave it empty for the default"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Parse a flag vector the way the process would. `#[serial]` because clap
    /// consults the process environment for every `env = …` field on every
    /// parse, and `set_var` in a sibling test is process-global.
    fn parse(args: &[&str]) -> UiArgs {
        let mut argv = vec!["kopiur-ui"];
        argv.extend_from_slice(args);
        UiArgs::try_parse_from(argv).expect("args must parse")
    }

    /// The minimum viable header-mode configuration: a user header plus an
    /// acknowledgement that there is no proxy secret. Tests that are not about
    /// the proxy-secret rule build on this so they fail for their own reason.
    fn header_mode(extra: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec![
            "--user-header".into(),
            "X-Forwarded-User".into(),
            "--acknowledge-no-proxy-secret".into(),
            "true".into(),
        ];
        v.extend(extra.iter().map(|s| (*s).to_string()));
        v
    }

    /// The debug-build-only escape hatch, and the fact that it is the *only*
    /// thing that opens the anonymous `system:` door.
    ///
    /// A release build has no such flag; this test is compiled out with it, and
    /// the `case(…)` table above keeps asserting the refusal in both profiles.
    #[cfg(debug_assertions)]
    #[test]
    #[serial]
    fn the_dev_hatch_is_the_only_way_to_an_anonymous_system_identity() {
        let smoke: Vec<String> = vec![
            "--anonymous-user".into(),
            "kubernetes-admin".into(),
            "--anonymous-groups".into(),
            SYSTEM_MASTERS.into(),
        ];

        let refused = resolve(&smoke).expect_err("without the hatch this must fail closed");
        assert!(
            matches!(refused, ConfigError::ForbiddenAnonymousGroup { .. }),
            "{refused:?}"
        );

        let mut allowed = smoke;
        allowed.push("--dev-allow-system-groups".into());
        allowed.push("true".into());
        let cfg = resolve(&allowed).expect("the hatch lets a local kind smoke through");
        match &cfg.auth.mode {
            AuthMode::AnonymousOnly(anonymous) => {
                assert_eq!(anonymous.user, "kubernetes-admin");
                assert_eq!(anonymous.groups, vec![SYSTEM_MASTERS.to_string()]);
            }
            AuthMode::Headers { .. } => panic!("no user header must select anonymous-only mode"),
        }
    }

    /// The hatch is scoped to the anonymous identity: it must not make header
    /// mode's deny-list negotiable.
    #[cfg(debug_assertions)]
    #[test]
    #[serial]
    fn the_dev_hatch_does_not_touch_header_mode() {
        let cfg = resolve(&header_mode(&["--dev-allow-system-groups", "true"]))
            .expect("header mode still resolves");
        assert!(matches!(cfg.auth.mode, AuthMode::Headers { .. }));
        // Nothing in the resolved config can relax what a proxy may assert:
        // `AuthConfig` carries no such flag, so `extract_identity` cannot read one.
        assert!(crate::auth::identity::is_forbidden_principal(
            SYSTEM_MASTERS
        ));
    }

    fn resolve(args: &[String]) -> Result<UiConfig, ConfigError> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        parse(&refs).resolve()
    }

    /// Write `bytes` to a uniquely-named temp file and return its path. Removed
    /// by [`TempFile`]'s `Drop`.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, bytes: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kopiur-ui-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&path, bytes).expect("temp file must be writable");
            Self(path)
        }
        fn as_str(&self) -> &str {
            self.0.to_str().expect("temp path must be UTF-8")
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // --- the happy paths ---------------------------------------------------

    #[test]
    #[serial]
    fn header_mode_resolves_with_a_proxy_secret() {
        let secret = TempFile::new("secret", "s3cr3t\n");
        let cfg = resolve(&[
            "--user-header".into(),
            "X-Forwarded-User".into(),
            "--groups-header".into(),
            "X-Forwarded-Groups".into(),
            "--email-header".into(),
            "X-Forwarded-Email".into(),
            "--proxy-secret-file".into(),
            secret.as_str().into(),
            "--allowed-groups".into(),
            " platform , sre ,".into(),
            "--impersonate-extra-keys".into(),
            "scopes".into(),
        ])
        .expect("a header-mode config with a proxy secret must resolve");

        match &cfg.auth.mode {
            AuthMode::Headers {
                user,
                groups,
                anonymous_fallback,
            } => {
                assert_eq!(user.as_str(), "x-forwarded-user");
                assert_eq!(
                    groups.as_ref().map(HeaderName::as_str),
                    Some("x-forwarded-groups")
                );
                assert_eq!(*anonymous_fallback, None);
            }
            AuthMode::AnonymousOnly(_) => {
                panic!("a configured user header must select header mode")
            }
        }
        assert_eq!(
            cfg.auth.email_header.as_ref().map(HeaderName::as_str),
            Some("x-forwarded-email")
        );
        // Trailing newline trimmed: a Secret written with `echo` carries one,
        // and the constant-time compare is over exact bytes.
        assert_eq!(cfg.auth.proxy_secret.as_deref(), Some(b"s3cr3t".as_slice()));
        // CSV values are trimmed and empties dropped.
        assert_eq!(
            cfg.auth.allowed_groups,
            Some(
                ["platform".to_string(), "sre".to_string()]
                    .into_iter()
                    .collect()
            )
        );
        assert_eq!(cfg.auth.extra_keys, vec!["scopes".to_string()]);
        assert_eq!(cfg.addr, DEFAULT_ADDR.parse::<SocketAddr>().unwrap());
        assert_eq!(
            cfg.ops_addr,
            DEFAULT_OPS_ADDR.parse::<SocketAddr>().unwrap()
        );
        assert!(cfg.cache_enabled);
        assert_eq!(cfg.session.ttl, Duration::from_secs(15 * 60));
        assert_eq!(cfg.session.ready_timeout, Duration::from_secs(300));
        assert_eq!(cfg.sar_ttl, Duration::from_secs(60));
        assert_eq!(cfg.client_cache.ttl, Duration::from_secs(600));
        assert_eq!(cfg.download_max_bytes, DEFAULT_MAX_DOWNLOAD_BYTES);
        assert_eq!(cfg.download_chunk_timeout, Duration::from_secs(60));
        assert_eq!(cfg.manifest_max_bytes, DEFAULT_MAX_MANIFEST_BYTES);
        assert_eq!(cfg.snapshot_list_cap, DEFAULT_SNAPSHOT_LIST_CAP);
        assert_eq!(cfg.tls, None);
        assert!(cfg.cors_origins.is_empty());
    }

    #[test]
    #[serial]
    fn anonymous_only_mode_resolves_without_a_proxy_secret() {
        // No user header ⇒ no trust boundary to protect, so the proxy-secret
        // rule does not apply.
        let cfg = resolve(&[
            "--anonymous-user".into(),
            "viewer".into(),
            "--anonymous-groups".into(),
            "readers,system:authenticated".into(),
        ])
        .expect("anonymous-only mode must resolve without a proxy secret");

        match &cfg.auth.mode {
            AuthMode::AnonymousOnly(anon) => {
                assert_eq!(anon.user, "viewer");
                assert_eq!(
                    anon.groups,
                    vec!["readers".to_string(), ALLOWED_SYSTEM_IDENTITY.to_string()]
                );
            }
            AuthMode::Headers { .. } => panic!("no user header must select anonymous-only mode"),
        }
        assert_eq!(cfg.auth.proxy_secret, None);
    }

    #[test]
    #[serial]
    fn the_anonymous_fallback_is_opt_in_and_carried_on_the_mode() {
        let cfg = resolve(&header_mode(&[
            "--anonymous-user",
            "viewer",
            "--anonymous-fallback",
            "true",
        ]))
        .expect("header mode with an explicit fallback must resolve");

        match &cfg.auth.mode {
            AuthMode::Headers {
                anonymous_fallback, ..
            } => assert_eq!(
                anonymous_fallback.as_ref().map(|a| a.user.as_str()),
                Some("viewer")
            ),
            AuthMode::AnonymousOnly(_) => panic!("a user header must select header mode"),
        }

        // Without the flag the anonymous identity is configured but inert.
        let cfg = resolve(&header_mode(&["--anonymous-user", "viewer"]))
            .expect("header mode without the fallback flag must resolve");
        match &cfg.auth.mode {
            AuthMode::Headers {
                anonymous_fallback, ..
            } => assert_eq!(*anonymous_fallback, None),
            AuthMode::AnonymousOnly(_) => panic!("a user header must select header mode"),
        }
    }

    // --- every ConfigError variant -----------------------------------------

    #[test]
    #[serial]
    fn every_fail_closed_rule_names_the_env_var_to_change() {
        /// One row of the fail-closed table.
        struct Case {
            /// What the row is checking, for the failure message.
            what: &'static str,
            /// The flags to resolve.
            args: Vec<String>,
            /// Whether the error is the variant this row expects.
            expected: fn(&ConfigError) -> bool,
            /// Substrings the rendered message must carry — always at least the
            /// env var an operator has to change.
            names: Vec<&'static str>,
        }

        /// Build a row. A constructor rather than a struct literal so the table
        /// below stays readable at a glance.
        fn case(
            what: &'static str,
            args: Vec<String>,
            expected: fn(&ConfigError) -> bool,
            names: Vec<&'static str>,
        ) -> Case {
            Case {
                what,
                args,
                expected,
                names,
            }
        }

        let cases: Vec<Case> = vec![
            case(
                "neither a header nor an anonymous user",
                vec![],
                |e| matches!(e, ConfigError::NoIdentitySource),
                vec![USER_HEADER_ENV, ANONYMOUS_USER_ENV],
            ),
            case(
                "the user header is an impersonation header",
                vec![
                    "--user-header".into(),
                    "Impersonate-User".into(),
                    "--acknowledge-no-proxy-secret".into(),
                    "true".into(),
                ],
                |e| matches!(e, ConfigError::ReservedHeaderName { header } if header == "impersonate-user"),
                vec![USER_HEADER_ENV, "impersonate-user"],
            ),
            case(
                "the user header is Authorization",
                vec![
                    "--user-header".into(),
                    "Authorization".into(),
                    "--acknowledge-no-proxy-secret".into(),
                    "true".into(),
                ],
                |e| matches!(e, ConfigError::ReservedHeaderName { header } if header == "authorization"),
                vec![USER_HEADER_ENV, "authorization"],
            ),
            case(
                "the groups header is Cookie",
                header_mode(&["--groups-header", "Cookie"]),
                |e| matches!(e, ConfigError::ReservedHeaderName { header } if header == "cookie"),
                vec![GROUPS_HEADER_ENV, "cookie"],
            ),
            case(
                "the user header is not a legal header name",
                vec!["--user-header".into(), "X Forwarded User".into()],
                |e| matches!(e, ConfigError::InvalidHeaderName { header, .. } if header == "X Forwarded User"),
                vec![USER_HEADER_ENV],
            ),
            case(
                "header mode with no proxy secret and no acknowledgement",
                vec!["--user-header".into(), "X-Forwarded-User".into()],
                |e| matches!(e, ConfigError::ProxySecretRequired),
                vec![PROXY_SECRET_FILE_ENV, ACKNOWLEDGE_NO_PROXY_SECRET_ENV],
            ),
            case(
                "the proxy secret file does not exist",
                vec![
                    "--user-header".into(),
                    "X-Forwarded-User".into(),
                    "--proxy-secret-file".into(),
                    "/nonexistent/kopiur-ui-proxy-secret".into(),
                ],
                |e| matches!(e, ConfigError::ProxySecretUnreadable { .. }),
                vec![PROXY_SECRET_FILE_ENV, "/nonexistent/kopiur-ui-proxy-secret"],
            ),
            case(
                "the anonymous fallback has no identity to fall back to",
                header_mode(&["--anonymous-fallback", "true"]),
                |e| matches!(e, ConfigError::AnonymousWithoutUser),
                vec![ANONYMOUS_FALLBACK_ENV, ANONYMOUS_USER_ENV],
            ),
            case(
                "an anonymous group is system:masters",
                vec![
                    "--anonymous-user".into(),
                    "viewer".into(),
                    "--anonymous-groups".into(),
                    SYSTEM_MASTERS.into(),
                ],
                |e| matches!(e, ConfigError::ForbiddenAnonymousGroup { group } if group == SYSTEM_MASTERS),
                vec![ANONYMOUS_GROUPS_ENV, SYSTEM_MASTERS],
            ),
            case(
                "an anonymous group is another system: group",
                vec![
                    "--anonymous-user".into(),
                    "viewer".into(),
                    "--anonymous-groups".into(),
                    "system:nodes".into(),
                ],
                |e| matches!(e, ConfigError::ForbiddenAnonymousGroup { group } if group == "system:nodes"),
                vec![ANONYMOUS_GROUPS_ENV, "system:nodes"],
            ),
            case(
                "the anonymous user itself is a system: identity",
                vec!["--anonymous-user".into(), "system:anonymous".into()],
                |e| matches!(e, ConfigError::ForbiddenAnonymousGroup { group } if group == "system:anonymous"),
                vec![ANONYMOUS_USER_ENV, "system:anonymous"],
            ),
            case(
                "the bind address is not host:port",
                {
                    let mut v = header_mode(&["--addr", "8090"]);
                    v.push("--groups-separator".into());
                    v.push(",".into());
                    v
                },
                |e| matches!(e, ConfigError::InvalidAddr { name, .. } if *name == ADDR_ENV),
                vec![ADDR_ENV, "8090"],
            ),
            case(
                "the ops address is not host:port",
                header_mode(&["--ops-addr", "not-an-address"]),
                |e| matches!(e, ConfigError::InvalidAddr { name, .. } if *name == OPS_ADDR_ENV),
                vec![OPS_ADDR_ENV, "not-an-address"],
            ),
            case(
                "a duration has no unit",
                header_mode(&["--session-ttl", "15"]),
                |e| matches!(e, ConfigError::InvalidDuration { name, .. } if *name == SESSION_TTL_ENV),
                vec![SESSION_TTL_ENV, "15"],
            ),
            case(
                "a duration uses an unsupported unit",
                header_mode(&["--sar-ttl", "60ms"]),
                |e| matches!(e, ConfigError::InvalidDuration { name, .. } if *name == SAR_TTL_ENV),
                vec![SAR_TTL_ENV, "60ms"],
            ),
            case(
                "the allowed-group list is separators only",
                header_mode(&["--allowed-groups", ","]),
                |e| matches!(e, ConfigError::InvalidAllowedGroups { value } if value == ","),
                vec![ALLOWED_GROUPS_ENV],
            ),
            case(
                "the allowed-group list is whitespace and separators",
                header_mode(&["--allowed-groups", " , , "]),
                |e| matches!(e, ConfigError::InvalidAllowedGroups { value } if value == " , , "),
                vec![ALLOWED_GROUPS_ENV],
            ),
        ];

        for Case {
            what,
            args,
            expected,
            names,
        } in cases
        {
            let err = resolve(&args).expect_err(&format!("{what}: must be refused"));
            assert!(expected(&err), "{what}: wrong variant, got {err:?}");
            let msg = err.to_string();
            for needle in names {
                assert!(
                    msg.contains(needle),
                    "{what}: message must name '{needle}', got: {msg}"
                );
            }
        }
    }

    /// An EMPTY allowed-group list still means "unset" — the repo-wide
    /// convention, because a chart renders a nulled value as `""`. Only a
    /// non-empty value that names no group is the typo worth refusing.
    #[test]
    #[serial]
    fn an_empty_allowed_groups_value_means_unset_not_a_typo() {
        let cfg = resolve(&header_mode(&["--allowed-groups", ""]))
            .expect("an empty allowed-groups value must resolve as unset");
        assert_eq!(cfg.auth.allowed_groups, None);
    }

    #[test]
    #[serial]
    fn an_empty_proxy_secret_file_is_no_secret_at_all() {
        // A zero-byte (or whitespace-only) Secret key would otherwise
        // authenticate any caller sending an empty token header.
        let empty = TempFile::new("empty-secret", "  \n");
        let err = resolve(&[
            "--user-header".into(),
            "X-Forwarded-User".into(),
            "--proxy-secret-file".into(),
            empty.as_str().into(),
        ])
        .expect_err("an empty proxy secret must be refused");
        assert!(
            matches!(err, ConfigError::ProxySecretRequired),
            "got {err:?}"
        );
    }

    #[test]
    #[serial]
    fn the_debug_rendering_never_leaks_the_proxy_secret() {
        let secret = TempFile::new("debug-secret", "hunter2");
        let cfg = resolve(&[
            "--user-header".into(),
            "X-Forwarded-User".into(),
            "--proxy-secret-file".into(),
            secret.as_str().into(),
        ])
        .expect("must resolve");

        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("104, 117, 110"),
            "the startup config log must not carry the proxy secret, got: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "got: {rendered}");
    }

    // --- the duration parser -----------------------------------------------

    #[test]
    fn durations_accept_whole_seconds_minutes_and_hours() {
        for (value, secs) in [
            ("0s", 0),
            ("60s", 60),
            ("300s", 300),
            ("15m", 900),
            ("1h", 3600),
            ("24h", 86_400),
        ] {
            let parsed = parse_duration(SAR_TTL_ENV, value)
                .unwrap_or_else(|e| panic!("'{value}' must parse, got: {e}"));
            assert_eq!(parsed.as_secs(), secs, "{value} must parse to {secs}s");
        }
    }

    #[test]
    fn durations_reject_everything_else() {
        for value in [
            "",
            "15",
            "m",
            "15x",
            "1.5h",
            "-5s",
            "15 m",
            "s15",
            "15sm",
            "1_000s",
            // No overflow surprises: seconds must fit a u64 after scaling.
            "99999999999999999999h",
        ] {
            let err = parse_duration(SESSION_TTL_ENV, value)
                .expect_err(&format!("'{value}' must be rejected"));
            let msg = err.to_string();
            assert!(
                msg.contains(SESSION_TTL_ENV),
                "the message must name the env var, got: {msg}"
            );
        }
    }
}
