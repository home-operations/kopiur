//! Driving the `kopiur-ui` web console from an e2e test, as a named user.
//!
//! # Why every request goes through the apiserver's Service proxy
//!
//! The console's app port is a `ClusterIP` Service and the chart never renders an
//! Ingress, so a test on the host has exactly one way in that does not change the
//! deployment under test: `/api/v1/namespaces/<ns>/services/<svc>:<port>/proxy/…`.
//! That path is also the *threat* the proxy shared secret exists to close — anyone
//! with `services/proxy` can reach the console and set identity headers — which
//! makes it the honest transport for this suite: if the console can be driven this
//! way without the token, that is a finding, not a test-harness convenience.
//!
//! # Which headers survive, and which do not
//!
//! The apiserver strips `Authorization` and re-authenticates its own request-header
//! auth names (`X-Remote-*`) before proxying, and it never forwards inbound
//! `Impersonate-*`. It does *not* touch `X-Forwarded-User` / `X-Forwarded-Groups`
//! (it only sets `X-Forwarded-For`/`-Host`/`-Proto`/`-Uri` itself), nor any custom
//! header such as `X-Kopiur-Proxy-Token` or `X-Kopiur-Request`. So the e2e install
//! configures the console on the `X-Forwarded-*` names — the same ones oauth2-proxy
//! emits — and this module sends them. Nothing here weakens the console's own
//! header configuration to make a test pass.
//!
//! # Failure messages
//!
//! Every assertion made through [`UiResponse`] names the subject, the method and
//! route, what was expected, what arrived, and the response body. An e2e failure is
//! read once, hours later, out of a CI log — "assertion failed: left == right" is
//! not a diagnosis.

use anyhow::{Context, Result};
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use kube::Client;
use kube::client::Body;
use serde::de::DeserializeOwned;

use crate::consts::OPERATOR_NS;

/// The console Service the chart renders (`<fullname>-ui`, release `kopiur`).
pub const UI_SERVICE: &str = "kopiur-ui";

/// The console's app port — the SPA and `/api`. Mirrors `ui.port`.
pub const UI_PORT: u16 = 8090;

/// The console's ops port — `/metrics`, `/healthz`, `/readyz`. Mirrors `ui.opsPort`.
pub const UI_OPS_PORT: u16 = 8091;

/// Header the e2e install reads the username from (`ui.auth.userHeader`).
pub const USER_HEADER: &str = "x-forwarded-user";

/// Header the e2e install reads groups from (`ui.auth.groupsHeader`).
pub const GROUPS_HEADER: &str = "x-forwarded-groups";

/// Header carrying the proxy shared secret (`KOPIUR_UI_PROXY_SECRET`).
pub const PROXY_TOKEN_HEADER: &str = "x-kopiur-proxy-token";

/// The SPA's CSRF marker header, required on every mutating request.
pub const REQUEST_HEADER: &str = "x-kopiur-request";

/// Secret the chart mounts the proxy shared token from. Created by the
/// `//crates/e2e:helm` task *before* the install, because the console pod mounts
/// it and would not start otherwise.
pub const PROXY_TOKEN_SECRET: &str = "kopiur-ui-proxy-token";

/// Key within [`PROXY_TOKEN_SECRET`] (`ui.auth.proxySecret.key`).
pub const PROXY_TOKEN_KEY: &str = "token";

/// The shared token the e2e install expects. Keep in lockstep with the
/// `//crates/e2e:helm` task, which creates the Secret holding it.
pub const PROXY_TOKEN: &str = "kopiur-e2e-proxy-shared-token";

/// The permitted subject: bound to `kopiur-ui-user` cluster-wide and to
/// `kopiur-ui-browse` in the operator namespace.
pub const EDITOR_USER: &str = "e2e-editor";

/// The unpermitted subject: authenticated, bound to nothing at all.
pub const NOBODY_USER: &str = "e2e-nobody";

/// Group both test subjects carry, so a binding can name either the user or the
/// group and the test still says which it meant.
pub const E2E_GROUP: &str = "kopiur-e2e-humans";

/// Who a request is made as: the identity the proxy would assert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    /// Username the console will impersonate.
    pub user: String,
    /// Groups impersonated alongside it.
    pub groups: Vec<String>,
}

impl Subject {
    /// A subject with the shared e2e group.
    #[must_use]
    pub fn new(user: &str) -> Self {
        Self {
            user: user.to_string(),
            groups: vec![E2E_GROUP.to_string()],
        }
    }

    /// The permitted subject — see [`EDITOR_USER`].
    #[must_use]
    pub fn editor() -> Self {
        Self::new(EDITOR_USER)
    }

    /// The unpermitted subject — see [`NOBODY_USER`].
    #[must_use]
    pub fn nobody() -> Self {
        Self::new(NOBODY_USER)
    }

    /// A subject with no groups at all, for the deny-list cases where the *user*
    /// is the thing under test.
    #[must_use]
    pub fn bare(user: &str) -> Self {
        Self {
            user: user.to_string(),
            groups: Vec::new(),
        }
    }
}

/// One console response, plus enough about the request to write a useful failure.
#[derive(Debug, Clone)]
pub struct UiResponse {
    /// HTTP status the console answered with.
    pub status: StatusCode,
    /// Response headers, as proxied back by the apiserver.
    pub headers: HeaderMap,
    /// Raw response body.
    pub body: Vec<u8>,
    /// `"<METHOD> <route>"`, for failure messages.
    pub route: String,
    /// How the request identified itself, for failure messages.
    pub subject: String,
}

impl UiResponse {
    /// The body as UTF-8, lossy — only ever used inside a failure message.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// The `(subject, route)` prefix every assertion below opens with.
    fn who(&self) -> String {
        format!("as {} — {}", self.subject, self.route)
    }

    /// Assert the status, naming the subject, the route, both statuses and the body.
    pub fn expect_status(self, want: u16) -> Self {
        assert_eq!(
            self.status.as_u16(),
            want,
            "{}: expected HTTP {want}, got {}. Body: {}",
            self.who(),
            self.status,
            self.text()
        );
        self
    }

    /// Assert one response header's exact value.
    pub fn expect_header(self, name: &str, want: &str) -> Self {
        let got = self.header(name);
        assert_eq!(
            got.as_deref(),
            Some(want),
            "{}: expected header {name}: {want}, got {got:?}",
            self.who()
        );
        self
    }

    /// One response header as a string, if present and printable.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// Deserialize the body as JSON, or panic naming the subject, route and body.
    #[must_use]
    pub fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "{}: response body is not the expected JSON shape ({e}). Body: {}",
                self.who(),
                self.text()
            )
        })
    }

    /// The body as an RFC 7807 problem document, asserting the media type too.
    ///
    /// The media type is part of the contract: the SPA switches on
    /// `application/problem+json` to decide whether it has a `Problem` to render or
    /// an opaque failure to report, so a correct body under the wrong `Content-Type`
    /// is still a bug.
    #[must_use]
    pub fn problem(&self) -> kopiur_ui_model::problem::Problem {
        let ct = self
            .header(header::CONTENT_TYPE.as_str())
            .unwrap_or_default();
        assert!(
            ct.starts_with("application/problem+json"),
            "{}: a refusal must be application/problem+json so the SPA can render it; \
             Content-Type was {ct:?}. Body: {}",
            self.who(),
            self.text()
        );
        self.json()
    }

    /// Assert the refusal's machine-readable `type` URN.
    ///
    /// Returns the parsed document so a caller can go on to assert on its prose,
    /// but discarding it is a complete assertion in itself — hence no `must_use`.
    pub fn expect_problem(self, want_type: &str) -> kopiur_ui_model::problem::Problem {
        let problem = self.problem();
        assert_eq!(
            problem.r#type,
            want_type,
            "{}: expected problem type {want_type}, got {}. detail={:?} why={:?} fix={:?}",
            self.who(),
            problem.r#type,
            problem.detail,
            problem.why,
            problem.fix
        );
        problem
    }
}

/// A console request under construction.
///
/// A builder rather than a pile of `Option` arguments so the *negative* cases read
/// as deliberate subtractions: `.without_identity()`, `.with_token("wrong")`,
/// `.without_csrf()`. A test that forgets the CSRF header by accident and one that
/// omits it on purpose should not look the same.
pub struct UiRequest {
    method: Method,
    route: String,
    subject: Option<Subject>,
    token: Option<String>,
    csrf: bool,
    body: Option<Vec<u8>>,
    port: u16,
}

impl UiRequest {
    /// A `GET` on the app port, as `subject`, with the correct proxy token.
    #[must_use]
    pub fn get(subject: &Subject, route: impl Into<String>) -> Self {
        Self::new(Method::GET, route, Some(subject.clone()))
    }

    /// A `POST` with a JSON body, as `subject`, CSRF header set.
    #[must_use]
    pub fn post(subject: &Subject, route: impl Into<String>, body: &serde_json::Value) -> Self {
        let mut req = Self::new(Method::POST, route, Some(subject.clone()));
        req.body = Some(serde_json::to_vec(body).expect("request body serializes"));
        req
    }

    /// A `DELETE`, as `subject`, CSRF header set.
    #[must_use]
    pub fn delete(subject: &Subject, route: impl Into<String>) -> Self {
        Self::new(Method::DELETE, route, Some(subject.clone()))
    }

    /// A `GET` on the ops port (`/metrics`, `/healthz`, `/readyz`), which carries
    /// no identity by design — probes and Prometheus must reach it while the app
    /// port is restricted to the proxy.
    #[must_use]
    pub fn ops(route: impl Into<String>) -> Self {
        let mut req = Self::new(Method::GET, route, None);
        req.token = None;
        req.port = UI_OPS_PORT;
        req
    }

    fn new(method: Method, route: impl Into<String>, subject: Option<Subject>) -> Self {
        Self {
            method,
            route: route.into(),
            subject,
            token: Some(PROXY_TOKEN.to_string()),
            csrf: true,
            body: None,
            port: UI_PORT,
        }
    }

    /// Send no identity headers at all — the "unauthenticated caller" case.
    #[must_use]
    pub fn without_identity(mut self) -> Self {
        self.subject = None;
        self
    }

    /// Send a specific proxy token (or `None` to send none).
    #[must_use]
    pub fn with_token(mut self, token: Option<&str>) -> Self {
        self.token = token.map(str::to_string);
        self
    }

    /// Omit the SPA's `X-Kopiur-Request` marker, as a cross-origin form post would.
    #[must_use]
    pub fn without_csrf(mut self) -> Self {
        self.csrf = false;
        self
    }

    /// How this request identifies itself, for failure messages.
    fn subject_label(&self) -> String {
        match (&self.subject, &self.token) {
            (Some(s), Some(t)) if t == PROXY_TOKEN => format!("{} (groups {:?})", s.user, s.groups),
            (Some(s), Some(_)) => format!("{} with a WRONG proxy token", s.user),
            (Some(s), None) => format!("{} with NO proxy token", s.user),
            (None, _) => "nobody (no identity headers)".to_string(),
        }
    }

    /// Send it, returning whatever the console answered — including refusals.
    ///
    /// Deliberately not `kube::Client::request_text`: that maps every non-2xx into
    /// `kube::Error::Api` and throws the headers away, and this suite's whole point
    /// is asserting on refusals — their status, their media type and their body.
    pub async fn send(self, ui: &Ui) -> Result<UiResponse> {
        let label = self.subject_label();
        let route_label = format!("{} {}", self.method, self.route);
        let path = format!(
            "/api/v1/namespaces/{OPERATOR_NS}/services/{UI_SERVICE}:{}/proxy{}",
            self.port, self.route
        );

        let mut builder = Request::builder().method(self.method.clone()).uri(&path);
        if let Some(subject) = &self.subject {
            builder = builder.header(USER_HEADER, header_value(&subject.user)?);
            if !subject.groups.is_empty() {
                builder = builder.header(GROUPS_HEADER, header_value(&subject.groups.join(","))?);
            }
        }
        if let Some(token) = &self.token {
            builder = builder.header(PROXY_TOKEN_HEADER, header_value(token)?);
        }
        if self.csrf {
            builder = builder.header(REQUEST_HEADER, HeaderValue::from_static("1"));
        }
        if self.body.is_some() {
            builder = builder.header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }

        let request = builder
            .body(Body::from(self.body.unwrap_or_default()))
            .with_context(|| format!("build console request {route_label}"))?;

        let response =
            ui.client.send(request).await.with_context(|| {
                format!("{route_label} as {label}: apiserver Service proxy call")
            })?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .with_context(|| format!("{route_label} as {label}: read response body"))?
            .to_bytes()
            .to_vec();

        Ok(UiResponse {
            status,
            headers,
            body,
            route: route_label,
            subject: label,
        })
    }
}

/// A `HeaderValue` from test-controlled text, with the failure naming the text.
fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).with_context(|| format!("{value:?} is not a valid header value"))
}

/// Handle for driving the console.
pub struct Ui {
    client: Client,
}

impl Ui {
    /// Wrap the harness's cluster client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Send one request. Sugar for [`UiRequest::send`].
    pub async fn send(&self, request: UiRequest) -> Result<UiResponse> {
        request.send(self).await
    }
}
