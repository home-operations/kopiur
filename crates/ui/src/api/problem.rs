//! Turning failures into `application/problem+json`.
//!
//! Every endpoint answers errors with one shape — [`kopiur_ui_model::problem::Problem`]
//! — carrying kopiur's what/why/fix triple, so the SPA can render a remediation
//! instead of a bare status code.
//!
//! # Why a newtype
//!
//! `Problem` lives in `kopiur-ui-model`, which is deliberately free of `axum` (the
//! SPA's type generator reads it), so `IntoResponse` cannot be implemented on it
//! here. [`ApiError`] is that implementation's home and nothing more; the wire
//! shape is still exactly `Problem`.
//!
//! # Where the text comes from
//!
//! [`kopiur_ops::OpsError`] messages already follow the repo's what/why/fix rule,
//! and they are text-tested in `crates/ops`. Rather than write a second set of
//! sentences that would drift from the CLI's, this module *splits* the existing
//! message into the three wire fields. The status code, however, is never guessed:
//! it comes from an exhaustive `match` over [`OpsErrorKind`], so a new failure
//! class cannot reach a browser without someone choosing what it means over HTTP.
//!
//! Two remediations are rewritten rather than reused, because the CLI's wording
//! names flags a browser has no way to pass.

use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;

use kopiur_ops::error::{OpsError, OpsErrorKind};
use kopiur_ui_model::problem::Problem;

use crate::auth::csrf::CsrfError;
use crate::auth::identity::AuthError;
use crate::auth::impersonate::ClientBuildError;

/// Prefix of every `Problem.type` kopiur-ui emits. A URN rather than an `https:`
/// URL because the SPA switches on it and nothing dereferences it — a URL would
/// promise a page that has to exist forever.
pub const PROBLEM_TYPE_PREFIX: &str = "urn:kopiur:problem:";

/// The `WWW-Authenticate` challenge on a 401.
///
/// Named after the trust boundary rather than a real HTTP auth scheme: kopiur-ui
/// authenticates nobody itself, so the only honest answer to "how do I
/// authenticate" is "come back through the proxy". A bare `Basic` challenge would
/// make browsers pop a credential dialog that can never succeed.
pub const PROXY_CHALLENGE: &str = "Kopiur-Proxy";

/// An error rendered as `application/problem+json`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError(pub Problem);

impl ApiError {
    /// The status this problem will be sent with.
    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// Replace the generated `detail` with a more specific one.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.0.detail = detail.into();
        self
    }

    /// Record the apiserver's `Status.reason`, so the SPA can tell an RBAC denial
    /// from a missing object without parsing prose.
    #[must_use]
    pub fn with_kube_reason(mut self, reason: impl Into<String>) -> Self {
        self.0.kube_reason = Some(reason.into());
        self
    }

    /// Record which request produced this problem.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.0.instance = Some(instance.into());
        self
    }
}

/// Build a problem from its parts.
///
/// `kind` is the kebab-case class name that becomes
/// `urn:kopiur:problem:<kind>`; the human-readable `title` is derived from it so
/// the two can never disagree.
pub fn problem(
    status: u16,
    kind: &str,
    what: impl Into<String>,
    why: impl Into<String>,
    fix: impl Into<String>,
) -> ApiError {
    let what = what.into();
    let why = why.into();
    ApiError(Problem {
        r#type: format!("{PROBLEM_TYPE_PREFIX}{kind}"),
        title: title_for(kind),
        status,
        detail: join_detail(&what, &why),
        what,
        why,
        fix: fix.into(),
        instance: None,
        kube_reason: None,
    })
}

/// Turn `kind-not-installed` into `Kind not installed`.
fn title_for(kind: &str) -> String {
    let spaced = kind.replace('-', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// `detail` is the whole story in one string, for clients (and log lines) that
/// read only the RFC-standard members.
fn join_detail(what: &str, why: &str) -> String {
    if why.is_empty() {
        what.to_string()
    } else {
        format!("{what} {why}")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response<Body> {
        let status = self.status();
        let body = serde_json::to_vec(&self.0).unwrap_or_else(|e| {
            // A `Problem` is plain owned strings, so this is unreachable; degrade
            // to a valid problem document rather than panicking inside a handler.
            tracing::error!(error = %e, "failed to serialize a Problem response");
            br#"{"type":"urn:kopiur:problem:internal","title":"Internal","status":500,"detail":"the error response could not be serialized","what":"kopiur-ui failed while reporting another failure.","why":"Serializing the problem document itself errored, which should be impossible.","fix":"Report this at https://github.com/home-operations/kopiur/issues with the UI's logs."}"#.to_vec()
        });

        let mut response = Response::new(Body::from(body));
        *response.status_mut() = status;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        // An error is never a cacheable answer: a 403 held in a shared cache
        // outlives the RoleBinding that fixes it.
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if status == StatusCode::UNAUTHORIZED {
            headers.insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(PROXY_CHALLENGE),
            );
        }
        response
    }
}

// --- kopiur_ops -------------------------------------------------------------

/// What one [`OpsErrorKind`] means over HTTP.
struct KindMapping {
    status: u16,
    /// The kebab-case class name, which becomes the `type` URN.
    kind: &'static str,
    /// `why` for a message that carries no second clause of its own.
    why: &'static str,
    /// `fix` for a message with no `Fix:` of its own.
    fix: &'static str,
    /// When set, this remediation replaces whatever the message said — used where
    /// the shared `OpsError` text names a CLI flag a browser cannot send.
    force_fix: bool,
}

/// Map a failure class onto its HTTP meaning.
///
/// Exhaustive by construction: adding an [`OpsErrorKind`] fails to compile until
/// someone decides what status a browser should see.
fn mapping(kind: OpsErrorKind) -> KindMapping {
    match kind {
        OpsErrorKind::Forbidden => KindMapping {
            status: 403,
            kind: "forbidden",
            why: "The apiserver refused the request for the identity kopiur-ui impersonated.",
            // The shared message tells a CLI user to change kubeconfig, which a
            // browser cannot do. The UI's answer is always an RBAC binding.
            fix: "ask a cluster admin to bind kopiur-ui-user, kopiur-ui-editor or \
                  kopiur-ui-viewer to your user or group",
            force_fix: true,
        },
        OpsErrorKind::NotFound => KindMapping {
            status: 404,
            kind: "not-found",
            why: "It does not exist in the scope the request named, or it was deleted after \
                  the page was loaded.",
            fix: "reload the page; if the link came from elsewhere, check the namespace and \
                  name it points at",
            force_fix: false,
        },
        OpsErrorKind::KindNotInstalled => KindMapping {
            status: 503,
            kind: "kind-not-installed",
            why: "The kopiur CRDs are missing from this cluster, or are older than the UI.",
            fix: "install or upgrade kopiur so the CRDs and the UI come from the same release",
            force_fix: false,
        },
        OpsErrorKind::Admission => KindMapping {
            status: 422,
            kind: "admission",
            why: "An admission webhook — kopiur's own, or a cluster policy engine — rejected \
                  the object.",
            fix: "correct the values named in the message and try again",
            force_fix: false,
        },
        OpsErrorKind::Invalid => KindMapping {
            status: 400,
            kind: "invalid",
            why: "The request names something the cluster's current shape cannot satisfy.",
            fix: "correct the request and try again",
            force_fix: false,
        },
        OpsErrorKind::Conflict => KindMapping {
            status: 409,
            kind: "conflict",
            why: "The cluster's shape conflicts with what the request asked for.",
            fix: "resolve the conflict the message describes, then try again",
            force_fix: false,
        },
        OpsErrorKind::Timeout => KindMapping {
            status: 504,
            kind: "timeout",
            why: "A bounded wait expired. The work itself may still be running in the cluster.",
            fix: "reload the page in a moment — waiting stopped, the operation did not",
            force_fix: false,
        },
        OpsErrorKind::Upstream => KindMapping {
            status: 502,
            kind: "upstream",
            why: "A dependency — the apiserver, a session pod, or kopia — failed or was \
                  unreachable.",
            fix: "retry; if it persists, check cluster health and the kopiur controller's logs",
            force_fix: false,
        },
        OpsErrorKind::Internal => KindMapping {
            status: 500,
            kind: "internal",
            why: "This is a kopiur bug, not a problem with the request.",
            fix: "report this at https://github.com/home-operations/kopiur/issues with the \
                  UI's logs",
            force_fix: false,
        },
    }
}

impl From<OpsError> for ApiError {
    fn from(error: OpsError) -> Self {
        let map = mapping(error.kind());
        let detail = error.to_string();
        let (head, message_fix) = split_fix(&detail);
        let (what, why) = split_what_why(head, map.why);

        let fix = match ui_fix(&error) {
            Some(rewritten) => rewritten.to_string(),
            None if map.force_fix => map.fix.to_string(),
            None => message_fix.unwrap_or(map.fix).to_string(),
        };

        let mut api = problem(map.status, map.kind, what, why, fix).with_detail(detail);
        if let Some(reason) = kube_reason(&error) {
            api = api.with_kube_reason(reason);
        }
        api
    }
}

/// Remediations the shared `OpsError` text words for the CLI and the UI must
/// word for a browser.
///
/// Only these two: every other `Fix:` in `kopiur_ops` is already UI-safe, and
/// duplicating them here is how they would drift.
/// Exhaustive over `OpsError` — no `_` arm — so a new variant whose CLI message
/// names a flag must be classified here before it compiles.
fn ui_fix(error: &OpsError) -> Option<&'static str> {
    match error {
        OpsError::UnknownPolicyRepository { .. } => {
            Some("choose one of the policy's repositories in the request")
        }
        OpsError::AmbiguousTarget { .. } => Some("specify the replication kind in the request"),

        OpsError::SelectorMatchedNothing { .. }
        | OpsError::Forbidden { .. }
        | OpsError::KindNotInstalled { .. }
        | OpsError::NotFound { .. }
        | OpsError::Api { .. }
        | OpsError::AdmissionDenied { .. }
        | OpsError::WaitTimeout { .. }
        | OpsError::GoneWhileWaiting { .. }
        | OpsError::RepoOutsideSessionNamespace { .. }
        | OpsError::MoverImageUnresolvable { .. }
        | OpsError::OperatorNamespaceUnresolvable { .. }
        | OpsError::CaBundleUnresolvable { .. }
        | OpsError::NotADirectory { .. }
        | OpsError::Serialization { .. }
        | OpsError::SnapshotNotBrowsable { .. }
        | OpsError::RepositoryUnderivable { .. }
        | OpsError::CredsOutsideSessionNamespace { .. }
        | OpsError::ClusterRepoSecretNamespaceMissing { .. }
        | OpsError::SessionPodFailed { .. }
        | OpsError::SessionNotReady { .. }
        | OpsError::SessionExec { .. }
        | OpsError::InvalidPath { .. }
        | OpsError::PathNotFound { .. }
        | OpsError::IsADirectory { .. }
        | OpsError::NotAFile { .. }
        | OpsError::SnapshotMissingInRepo { .. }
        | OpsError::UnexpectedKopiaOutput { .. }
        | OpsError::StreamIo { .. } => None,
    }
}

/// The apiserver's `Status.reason`, when the failure came from it.
///
/// Exhaustive over `OpsError` for the same reason as [`ui_fix`]: a new variant
/// that wraps a `kube::Error` should surface its reason, and a `_` arm would let
/// it slip through silently.
fn kube_reason(error: &OpsError) -> Option<String> {
    let source = match error {
        OpsError::Forbidden { source, .. }
        | OpsError::KindNotInstalled { source, .. }
        | OpsError::Api { source, .. } => source,

        OpsError::SelectorMatchedNothing { .. }
        | OpsError::UnknownPolicyRepository { .. }
        | OpsError::NotFound { .. }
        | OpsError::AdmissionDenied { .. }
        | OpsError::WaitTimeout { .. }
        | OpsError::GoneWhileWaiting { .. }
        | OpsError::AmbiguousTarget { .. }
        | OpsError::RepoOutsideSessionNamespace { .. }
        | OpsError::MoverImageUnresolvable { .. }
        | OpsError::OperatorNamespaceUnresolvable { .. }
        | OpsError::CaBundleUnresolvable { .. }
        | OpsError::NotADirectory { .. }
        | OpsError::Serialization { .. }
        | OpsError::SnapshotNotBrowsable { .. }
        | OpsError::RepositoryUnderivable { .. }
        | OpsError::CredsOutsideSessionNamespace { .. }
        | OpsError::ClusterRepoSecretNamespaceMissing { .. }
        | OpsError::SessionPodFailed { .. }
        | OpsError::SessionNotReady { .. }
        | OpsError::SessionExec { .. }
        | OpsError::InvalidPath { .. }
        | OpsError::PathNotFound { .. }
        | OpsError::IsADirectory { .. }
        | OpsError::NotAFile { .. }
        | OpsError::SnapshotMissingInRepo { .. }
        | OpsError::UnexpectedKopiaOutput { .. }
        | OpsError::StreamIo { .. } => return None,
    };
    match source.as_ref() {
        kube::Error::Api(status) if !status.reason.is_empty() => Some(status.reason.clone()),
        _other => None,
    }
}

/// Split a message's `Fix: …` tail off the rest.
///
/// The repo's message discipline puts the remediation last and introduces it with
/// exactly `Fix: `, so the first occurrence is the boundary. The trailing `.`/`,`
/// left on the head is trimmed so the two halves read as sentences on their own.
fn split_fix(detail: &str) -> (&str, Option<&str>) {
    match detail.find("Fix: ") {
        Some(at) => (
            detail[..at].trim().trim_end_matches(['.', ',']).trim(),
            Some(detail[at + "Fix: ".len()..].trim()),
        ),
        None => (detail.trim(), None),
    }
}

/// Split the non-remediation part of a message into what happened and why.
///
/// Two shapes cover every `OpsError`: a leading clause and a colon
/// (`forbidden: cannot list repositories …`), or two sentences. Anything else
/// keeps the whole text as `what` and takes `why` from the failure's class — a
/// wrong split is worse than none, because the SPA renders the two separately.
fn split_what_why<'a>(head: &'a str, kind_why: &'a str) -> (&'a str, &'a str) {
    if let Some((what, why)) = head.split_once(": ")
        && reads_as_a_clause(what)
        && !why.trim().is_empty()
    {
        return (what.trim(), why.trim());
    }
    if let Some((what, why)) = head.split_once(". ")
        && !why.trim().is_empty()
    {
        // `split_once` ate the full stop; the sentence keeps it.
        return (head[..what.len() + 1].trim(), why.trim());
    }
    (head, kind_why)
}

/// Whether the text before a colon is a short opening clause rather than a whole
/// paragraph that merely happens to contain one.
fn reads_as_a_clause(text: &str) -> bool {
    !text.is_empty() && text.len() <= 80 && !text.contains(". ")
}

// --- auth -------------------------------------------------------------------

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        // The two proxy-secret variants deliberately share one detail as well as
        // one problem type: telling a caller *which* of "no token" and "wrong
        // token" it was hands them a probe for whether a secret is configured.
        let detail = match &error {
            AuthError::ProxySecretMissing | AuthError::ProxySecretMismatch => {
                "the request did not come through the trusted proxy".to_string()
            }
            other => other.to_string(),
        };
        let api = match &error {
            AuthError::NoIdentity { expected_header } => problem(
                401,
                "no-identity",
                "kopiur-ui could not tell who you are.",
                "Every Kubernetes call it makes runs as the person making it, and this \
                 request carried no identity it trusts.",
                match expected_header {
                    Some(header) => format!(
                        "reach the UI through the authenticating proxy, which must set the \
                         {header} header on every request"
                    ),
                    None => {
                        "reach the UI through the authenticating proxy in front of it".to_string()
                    }
                },
            ),
            AuthError::InvalidHeaderValue { header, reason } => problem(
                400,
                "invalid-header-value",
                format!("The {header} header is not a usable identity."),
                format!("{reason}. Impersonation headers must be visible ASCII and bounded."),
                format!("correct what the authenticating proxy puts in {header}"),
            ),
            AuthError::ForbiddenPrincipal { principal, why } => problem(
                403,
                "forbidden-principal",
                format!("kopiur-ui refuses to act as {principal}."),
                format!("{why}."),
                "have the proxy assert your real user and groups, and grant access by binding \
                 kopiur-ui-user to them",
            ),
            AuthError::TooManyGroups { count, max } => problem(
                400,
                "too-many-groups",
                format!("The request asserted {count} groups."),
                format!(
                    "kopiur-ui impersonates at most {max}, because every group becomes a \
                     header on every apiserver call the request makes."
                ),
                "narrow what the proxy puts in the groups header, or set \
                 KOPIUR_UI_ALLOWED_GROUPS to the groups that grant kopiur access",
            ),
            AuthError::ProxySecretMissing | AuthError::ProxySecretMismatch => problem(
                401,
                "proxy-secret",
                "This request did not come through the trusted proxy.",
                "Identity headers are only believed from the configured proxy, which proves \
                 itself with a shared secret in X-Kopiur-Proxy-Token — and this request's was \
                 missing or wrong.",
                "use the UI's own address so the proxy handles the request; if you run the \
                 proxy, point it and kopiur-ui at the same Secret",
            ),
            AuthError::NotWired => problem(
                500,
                "not-wired",
                "kopiur-ui was started without wiring its authentication state.",
                "It cannot establish who any caller is, so it refuses to act on anyone's \
                 behalf rather than choosing an identity nobody configured.",
                "this is a build bug in kopiur-ui; report it at \
                 https://github.com/home-operations/kopiur/issues",
            ),
        };
        api.with_detail(detail)
    }
}

impl From<ClientBuildError> for ApiError {
    fn from(error: ClientBuildError) -> Self {
        let detail = error.to_string();
        let api = match &error {
            ClientBuildError::InvalidIdentity(inner) => problem(
                500,
                "client-build",
                "kopiur-ui could not build a Kubernetes client for this caller's identity.",
                format!(
                    "The caller's {} could not be sent as an impersonation header, and a \
                     request without one would run as the UI's own ServiceAccount — so it was \
                     refused instead.",
                    inner.what
                ),
                "this is a bug in kopiur-ui; report it at \
                 https://github.com/home-operations/kopiur/issues",
            ),
            ClientBuildError::Kube(_) => problem(
                500,
                "client-build",
                "kopiur-ui could not build a Kubernetes client from its own configuration.",
                "The base client configuration — TLS material, proxy settings, or the cluster \
                 URL — is not usable.",
                "check the UI's ServiceAccount token mount and any KUBECONFIG or proxy \
                 settings on its Deployment, then restart it",
            ),
        };
        api.with_detail(detail)
    }
}

impl From<CsrfError> for ApiError {
    fn from(error: CsrfError) -> Self {
        let detail = error.to_string();
        // Same discipline as the `OpsError` path: the remediation belongs in
        // `fix`, so it must not be repeated inside `why`.
        let (why, _) = split_fix(&detail);
        problem(
            403,
            "csrf",
            "kopiur-ui refused a change that did not come from its own pages.",
            why,
            match &error {
                CsrfError::MissingRequestHeader => {
                    "make the change from the kopiur UI; if you are scripting against the API, \
                     send X-Kopiur-Request: 1"
                }
                CsrfError::WrongContentType { .. } => "send the request body as application/json",
                CsrfError::CrossSite { .. } | CsrfError::OriginMismatch { .. } => {
                    "make the change from the kopiur UI itself rather than from another site \
                     or an embedded frame"
                }
            },
        )
        .with_detail(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use kube::core::Status;

    use crate::auth::impersonate::{IdentityHeaderKind, InvalidIdentityHeader};

    async fn parse(api: ApiError) -> (StatusCode, axum::http::HeaderMap, Problem) {
        let response = api.into_response();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("problem body");
        (
            status,
            headers,
            serde_json::from_slice(&bytes).expect("problem+json"),
        )
    }

    /// One `OpsError` per `OpsErrorKind`, built through an exhaustive `match` so
    /// a new kind cannot be added without this test being updated.
    fn sample(kind: OpsErrorKind) -> OpsError {
        match kind {
            OpsErrorKind::Forbidden => OpsError::Forbidden {
                verb: "list",
                resource: "snapshots",
                scope: " in namespace prod".to_string(),
                source: Box::new(kube::Error::Api(
                    Status::failure(
                        "snapshots.kopiur.home-operations.com is forbidden: User \"alice\" \
                         cannot list resource",
                        "Forbidden",
                    )
                    .with_code(403)
                    .boxed(),
                )),
            },
            OpsErrorKind::NotFound => OpsError::NotFound {
                kind: "Repository",
                plural: "repositories",
                name: "backups".to_string(),
                scope: " in namespace prod".to_string(),
                scope_flag: " -n prod".to_string(),
            },
            OpsErrorKind::KindNotInstalled => OpsError::KindNotInstalled {
                kind: "Repository",
                source: Box::new(kube::Error::Api(
                    Status::failure("could not find the requested resource", "NotFound")
                        .with_code(404)
                        .boxed(),
                )),
            },
            OpsErrorKind::Admission => OpsError::AdmissionDenied {
                message: "admission webhook \"vsnapshot.kopiur\" denied the request: \
                          policyRef is immutable"
                    .to_string(),
            },
            OpsErrorKind::Invalid => OpsError::InvalidPath {
                path: "../etc/passwd".to_string(),
                reason: "it escapes the snapshot root".to_string(),
            },
            OpsErrorKind::Conflict => OpsError::RepoOutsideSessionNamespace {
                repo: "Repository/store".to_string(),
                repo_namespace: "backups".to_string(),
                session_namespace: "prod".to_string(),
            },
            OpsErrorKind::Timeout => OpsError::WaitTimeout {
                what: "Snapshot prod/nightly-1".to_string(),
                after: "5m".to_string(),
                hint: "watch it with kubectl kopiur snapshots list".to_string(),
            },
            OpsErrorKind::Upstream => OpsError::SessionExec {
                what: "listing the snapshot root".to_string(),
                stderr: "connection reset".to_string(),
            },
            OpsErrorKind::Internal => OpsError::UnexpectedKopiaOutput {
                what: "the directory listing".to_string(),
                detail: "kopia printed no JSON".to_string(),
            },
        }
    }

    const EVERY_KIND: &[OpsErrorKind] = &[
        OpsErrorKind::Forbidden,
        OpsErrorKind::NotFound,
        OpsErrorKind::KindNotInstalled,
        OpsErrorKind::Admission,
        OpsErrorKind::Invalid,
        OpsErrorKind::Conflict,
        OpsErrorKind::Timeout,
        OpsErrorKind::Upstream,
        OpsErrorKind::Internal,
    ];

    #[test]
    fn every_ops_error_kind_maps_to_its_status_and_type() {
        let expected = [
            (OpsErrorKind::Forbidden, 403, "forbidden"),
            (OpsErrorKind::NotFound, 404, "not-found"),
            (OpsErrorKind::KindNotInstalled, 503, "kind-not-installed"),
            (OpsErrorKind::Admission, 422, "admission"),
            (OpsErrorKind::Invalid, 400, "invalid"),
            (OpsErrorKind::Conflict, 409, "conflict"),
            (OpsErrorKind::Timeout, 504, "timeout"),
            (OpsErrorKind::Upstream, 502, "upstream"),
            (OpsErrorKind::Internal, 500, "internal"),
        ];
        assert_eq!(expected.len(), EVERY_KIND.len());

        for (kind, status, slug) in expected {
            let error = sample(kind);
            assert_eq!(
                error.kind(),
                kind,
                "the fixture must have the kind it claims"
            );
            let api = ApiError::from(error);
            assert_eq!(api.0.status, status, "{slug}");
            assert_eq!(api.0.r#type, format!("{PROBLEM_TYPE_PREFIX}{slug}"));
        }
    }

    #[test]
    fn every_kind_produces_a_complete_actionable_problem() {
        for kind in EVERY_KIND {
            let api = ApiError::from(sample(*kind));
            let p = &api.0;
            assert!(!p.title.is_empty(), "{kind:?}");
            assert!(!p.detail.is_empty(), "{kind:?}");
            assert!(!p.what.is_empty(), "{kind:?}");
            assert!(!p.why.is_empty(), "{kind:?} must never render an empty why");
            assert!(!p.fix.is_empty(), "{kind:?} must always say what to do");
            assert!(
                !p.fix.contains("Fix:"),
                "{kind:?} fix must be the remediation, not the label: {}",
                p.fix
            );
        }
    }

    #[test]
    fn a_forbidden_names_the_ui_roles_and_keeps_the_apiserver_text() {
        let api = ApiError::from(sample(OpsErrorKind::Forbidden));

        assert_eq!(
            api.0.fix,
            "ask a cluster admin to bind kopiur-ui-user, kopiur-ui-editor or kopiur-ui-viewer \
             to your user or group"
        );
        assert!(
            !api.0.fix.contains("kubeconfig"),
            "the CLI's remediation is meaningless in a browser: {}",
            api.0.fix
        );
        assert!(
            api.0.why.contains("cannot list resource"),
            "the apiserver's own words must survive into why: {}",
            api.0.why
        );
        assert_eq!(api.0.kube_reason.as_deref(), Some("Forbidden"));
    }

    #[test]
    fn the_two_cli_flag_remediations_are_rewritten_for_the_ui() {
        let api = ApiError::from(OpsError::UnknownPolicyRepository {
            given: "store".to_string(),
            policy: "nightly".to_string(),
            valid: "primary, offsite".to_string(),
        });
        assert_eq!(
            api.0.fix,
            "choose one of the policy's repositories in the request"
        );
        assert!(!api.0.fix.contains("--repository"));

        let api = ApiError::from(OpsError::AmbiguousTarget {
            what: "two replications named nightly".to_string(),
            candidates: "SnapshotReplication/nightly, RepositoryReplication/nightly".to_string(),
        });
        assert_eq!(api.0.fix, "specify the replication kind in the request");
        assert!(!api.0.fix.contains("positional"));
    }

    #[test]
    fn a_messages_own_remediation_is_reused_when_it_is_ui_safe() {
        let api = ApiError::from(sample(OpsErrorKind::NotFound));
        assert!(
            api.0.fix.starts_with("list what exists"),
            "the shared message already says what to do: {}",
            api.0.fix
        );
    }

    #[test]
    fn a_message_with_a_leading_clause_splits_on_the_colon() {
        let api = ApiError::from(sample(OpsErrorKind::Admission));
        assert_eq!(api.0.what, "an admission webhook rejected this object");
        assert!(
            api.0.why.contains("policyRef is immutable"),
            "{}",
            api.0.why
        );
    }

    #[test]
    fn a_message_with_no_colon_splits_on_the_sentence() {
        let api = ApiError::from(sample(OpsErrorKind::Timeout));
        assert_eq!(
            api.0.what,
            "timed out after 5m waiting for Snapshot prod/nightly-1."
        );
        assert!(
            api.0.why.starts_with("The operation is still running"),
            "{}",
            api.0.why
        );
    }

    #[test]
    fn a_single_sentence_message_takes_its_why_from_the_failure_class() {
        let api = ApiError::from(sample(OpsErrorKind::NotFound));
        assert_eq!(
            api.0.what,
            "Repository \"backups\" not found in namespace prod"
        );
        assert_eq!(api.0.why, mapping(OpsErrorKind::NotFound).why);
    }

    #[test]
    fn a_non_apiserver_failure_carries_no_kube_reason() {
        assert_eq!(
            ApiError::from(sample(OpsErrorKind::Timeout)).0.kube_reason,
            None
        );
        assert_eq!(
            ApiError::from(sample(OpsErrorKind::KindNotInstalled))
                .0
                .kube_reason
                .as_deref(),
            Some("NotFound")
        );
    }

    // --- auth and CSRF ------------------------------------------------------

    #[test]
    fn no_identity_is_a_401_naming_the_expected_header() {
        let api = ApiError::from(AuthError::NoIdentity {
            expected_header: Some("x-forwarded-user".to_string()),
        });
        assert_eq!(api.0.status, 401);
        assert_eq!(api.0.r#type, "urn:kopiur:problem:no-identity");
        assert!(api.0.fix.contains("x-forwarded-user"), "{}", api.0.fix);
    }

    #[test]
    fn both_proxy_secret_failures_are_byte_identical_on_the_wire() {
        // Any difference between "no token" and "wrong token" is a probe for
        // whether a shared secret is configured at all.
        let missing = ApiError::from(AuthError::ProxySecretMissing).0;
        let mismatch = ApiError::from(AuthError::ProxySecretMismatch).0;

        assert_eq!(missing.status, 401);
        assert_eq!(missing.r#type, "urn:kopiur:problem:proxy-secret");
        assert_eq!(
            serde_json::to_string(&missing).expect("serializable"),
            serde_json::to_string(&mismatch).expect("serializable"),
            "every field, including detail, must be identical"
        );
    }

    #[test]
    fn the_unwired_placeholder_is_a_500_not_an_identity() {
        let api = ApiError::from(AuthError::NotWired);
        assert_eq!(api.0.status, 500);
        assert_eq!(api.0.r#type, "urn:kopiur:problem:not-wired");
        assert!(api.0.what.contains("without wiring"), "{}", api.0.what);
        assert!(api.0.fix.contains("report it"), "{}", api.0.fix);
    }

    #[test]
    fn a_client_that_cannot_be_built_is_a_500_never_a_request_run_as_the_ui() {
        let api = ApiError::from(ClientBuildError::from(InvalidIdentityHeader {
            what: IdentityHeaderKind::User,
            value: "alice\nbob".to_string(),
        }));

        assert_eq!(api.0.status, 500);
        assert_eq!(api.0.r#type, "urn:kopiur:problem:client-build");
        assert!(api.0.why.contains("user"), "{}", api.0.why);
        assert!(
            api.0.why.contains("ServiceAccount"),
            "the why must say what the alternative would have been: {}",
            api.0.why
        );
    }

    #[test]
    fn the_remaining_auth_failures_map_to_their_statuses() {
        let cases = [
            (
                AuthError::InvalidHeaderValue {
                    header: "x-forwarded-user".to_string(),
                    reason: "the value is empty".to_string(),
                },
                400,
                "urn:kopiur:problem:invalid-header-value",
            ),
            (
                AuthError::ForbiddenPrincipal {
                    principal: "system:masters".to_string(),
                    why: "the system: namespace is reserved",
                },
                403,
                "urn:kopiur:problem:forbidden-principal",
            ),
            (
                AuthError::TooManyGroups { count: 99, max: 64 },
                400,
                "urn:kopiur:problem:too-many-groups",
            ),
        ];
        for (error, status, r#type) in cases {
            let api = ApiError::from(error);
            assert_eq!(api.0.status, status, "{type}");
            assert_eq!(api.0.r#type, r#type);
            assert!(!api.0.fix.is_empty(), "{type}");
        }
    }

    #[test]
    fn every_csrf_refusal_is_one_403_class_with_its_own_remediation() {
        let cases = [
            CsrfError::MissingRequestHeader,
            CsrfError::WrongContentType {
                got: "text/plain".to_string(),
            },
            CsrfError::CrossSite {
                sec_fetch_site: "cross-site".to_string(),
            },
            CsrfError::OriginMismatch {
                origin: "https://evil.example".to_string(),
                host: "kopiur.example".to_string(),
            },
        ];
        let mut fixes = Vec::new();
        for error in cases {
            let api = ApiError::from(error);
            assert_eq!(api.0.status, 403);
            assert_eq!(api.0.r#type, "urn:kopiur:problem:csrf");
            assert!(
                !api.0.why.contains("Fix:"),
                "the remediation belongs in fix, not repeated inside why: {}",
                api.0.why
            );
            assert!(!api.0.why.is_empty());
            fixes.push(api.0.fix);
        }
        assert!(fixes.iter().any(|f| f.contains("X-Kopiur-Request")));
        assert!(fixes.iter().any(|f| f.contains("application/json")));
    }

    // --- the wire response --------------------------------------------------

    #[tokio::test]
    async fn the_response_is_problem_json_and_never_cached() {
        let (status, headers, body) = parse(ApiError::from(sample(OpsErrorKind::NotFound))).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert!(
            headers.get(header::WWW_AUTHENTICATE).is_none(),
            "only a 401 challenges"
        );
        assert_eq!(body.status, 404);
        assert_eq!(body.title, "Not found");
    }

    #[tokio::test]
    async fn a_401_challenges_with_the_proxy_scheme() {
        let (status, headers, body) = parse(ApiError::from(AuthError::ProxySecretMissing)).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers.get(header::WWW_AUTHENTICATE).unwrap(),
            PROXY_CHALLENGE
        );
        assert_eq!(body.status, 401);
    }

    #[test]
    fn the_builders_populate_the_optional_members() {
        let api = problem(400, "invalid", "What.", "Why.", "fix it")
            .with_instance("/api/v1/snapshots")
            .with_kube_reason("Invalid");

        assert_eq!(api.0.detail, "What. Why.");
        assert_eq!(api.0.title, "Invalid");
        assert_eq!(api.0.instance.as_deref(), Some("/api/v1/snapshots"));
        assert_eq!(api.0.kube_reason.as_deref(), Some("Invalid"));
    }
}
