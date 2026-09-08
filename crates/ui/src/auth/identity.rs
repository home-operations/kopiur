//! Who a request runs as.
//!
//! [`Identity`] is the one value the whole backend keys on: it decides which
//! `Impersonate-*` headers go to the apiserver, which entry of the impersonating
//! client cache is used, which `SubjectAccessReview` answers apply, and which
//! per-identity exec semaphore a browse request takes. It is derived from the
//! trusted proxy headers (or the configured anonymous identity) and is never
//! influenced by an inbound `Impersonate-*` header — those are stripped.
//!
//! [`extract_identity`] is a pure function of the headers and the resolved
//! [`AuthConfig`], on purpose: the entire identity table — including every way a
//! caller might try to escalate — is a unit test with no cluster and no server in
//! it.

use std::collections::BTreeMap;

use http::header::{HeaderMap, HeaderName, HeaderValue};

use kopiur_ui_model::identity::IdentitySource;

use crate::config::{AnonymousIdentity, AuthConfig, AuthMode};

/// The group every authenticated caller carries, and the one `system:` principal
/// that is not forbidden. Kubernetes adds it to every authenticated request and
/// RBAC bindings routinely target it, so an impersonated identity without it is
/// not the subject the apiserver would otherwise have seen.
pub const SYSTEM_AUTHENTICATED: &str = "system:authenticated";

/// The one `userextras` key kopiur-ui can source today: the address from the
/// configured email header. Emitted only when `extra_keys` names it, because the
/// UI's ClusterRole enumerates exactly those keys and the apiserver refuses an
/// `Impersonate-Extra-<key>` it does not grant.
pub const EMAIL_EXTRA_KEY: &str = "email";

/// Largest accepted length, in bytes, of a single principal (the username, one
/// group, or the email). Kubernetes imposes no such limit; this one exists so a
/// proxy bug cannot turn one request into a megabyte of impersonation headers.
pub const MAX_PRINCIPAL_BYTES: usize = 512;

/// Largest number of groups a single identity may assert.
///
/// Every group becomes one `Impersonate-Group` header on every apiserver call the
/// request makes, so the bound is on the outbound request as much as on the
/// identity.
pub const MAX_GROUPS: usize = 64;

/// The authenticated caller, as kopiur-ui will assert them to the apiserver.
///
/// `Eq` + `Hash` are load-bearing, not conveniences: the impersonating client
/// cache and the `SubjectAccessReview` cache are both keyed by the whole
/// identity, so two callers who differ in *any* field — one extra group, a
/// different `userextras` value — must never share a cached client or a cached
/// authorization decision. `BTreeMap` (not `HashMap`) for `extra` so that key
/// order is deterministic wherever the identity is rendered or hashed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    /// Username to impersonate (`Impersonate-User`).
    pub user: String,
    /// Groups to impersonate (`Impersonate-Group`, repeated).
    pub groups: Vec<String>,
    /// Email address the proxy supplied, when it did. Display only: never
    /// impersonated, never used for authorization.
    pub email: Option<String>,
    /// `userextras` entries to impersonate (`Impersonate-Extra-<key>`),
    /// restricted to the keys named by `KOPIUR_UI_IMPERSONATE_EXTRA_KEYS` — the
    /// same keys the UI's ClusterRole enumerates.
    pub extra: BTreeMap<String, Vec<String>>,
    /// Whether the proxy asserted this identity or it is the configured
    /// anonymous one. Reported on `kopiur_ui_requests_total{identity_source}` so
    /// a fallback to anonymous is visible rather than silent.
    pub source: IdentitySource,
}

/// Everything that can go wrong establishing who a caller is.
///
/// Each variant maps to exactly one status code in [`crate::api::problem`], and
/// each message states what happened, why, and what to do — an operator reading a
/// 401 in the browser's network tab should not have to read this source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No identity was presented, and none is configured as a fallback.
    #[error(
        "no caller identity was presented{}. kopiur-ui runs every Kubernetes call as the \
         person making it, so a request it cannot attribute has no permissions to use. \
         Fix: reach the UI through the authenticating proxy, or set KOPIUR_UI_ANONYMOUS_USER \
         with KOPIUR_UI_ANONYMOUS_FALLBACK=true to choose an explicit identity for \
         unauthenticated requests",
        .expected_header.as_ref().map(|h| format!(" in the {h} header")).unwrap_or_default()
    )]
    NoIdentity {
        /// The header the proxy was expected to set, when header mode is active.
        expected_header: Option<String>,
    },

    /// A configured identity header carried something that cannot be a
    /// Kubernetes principal.
    #[error(
        "the {header} header is not a usable identity: {reason}. Impersonation headers must be \
         visible ASCII and bounded, or a proxy could smuggle a second header — or an \
         unbounded one — into the apiserver request. \
         Fix: correct what the authenticating proxy puts in {header}"
    )]
    InvalidHeaderValue {
        /// The header whose value was refused.
        header: String,
        /// What is wrong with it.
        reason: String,
    },

    /// A user or group the UI refuses to impersonate.
    #[error(
        "refusing to impersonate {principal:?}: {why}. \
         Fix: have the authenticating proxy assert the caller's real user and groups, and \
         grant access by binding kopiur-ui-user to them instead of to a built-in system: \
         principal"
    )]
    ForbiddenPrincipal {
        /// The offending user or group.
        principal: String,
        /// Why it is refused.
        why: &'static str,
    },

    /// More groups than the UI will impersonate on one request.
    #[error(
        "the caller asserted {count} groups, more than the {max} kopiur-ui will impersonate. \
         Every group becomes an Impersonate-Group header on every apiserver call this request \
         makes. \
         Fix: narrow what the proxy puts in the groups header, or set KOPIUR_UI_ALLOWED_GROUPS \
         to the groups that actually grant kopiur access"
    )]
    TooManyGroups {
        /// How many groups were asserted.
        count: usize,
        /// The bound.
        max: usize,
    },

    /// Header mode is configured with a shared secret and the request had none.
    #[error(
        "the request carried no proxy shared secret. Identity headers are only trustworthy \
         coming from the configured proxy, and in a cluster anything that can reach this \
         Service can set them. \
         Fix: have the proxy send the shared secret in X-Kopiur-Proxy-Token — the same value \
         kopiur-ui reads from KOPIUR_UI_PROXY_SECRET_FILE"
    )]
    ProxySecretMissing,

    /// The presented shared secret is not the configured one.
    #[error(
        "the proxy shared secret in X-Kopiur-Proxy-Token does not match the one kopiur-ui was \
         configured with, so this request did not come through the trusted proxy. \
         Fix: point the proxy and kopiur-ui at the same Secret, and restart both after \
         rotating it"
    )]
    ProxySecretMismatch,
}

/// Whether a principal is one kopiur-ui refuses to impersonate.
///
/// Everything in the `system:` namespace is reserved for Kubernetes' own
/// principals and is a privilege escalation to assert — `system:masters` bypasses
/// RBAC entirely, and the rest name components rather than people. The single
/// exception is [`SYSTEM_AUTHENTICATED`], which every authenticated request
/// already carries and which many bindings target.
pub fn is_forbidden_principal(principal: &str) -> bool {
    principal.starts_with("system:") && principal != SYSTEM_AUTHENTICATED
}

/// Resolve the caller's identity from the request headers.
///
/// Two rules here are the security of the whole crate:
///
/// * **Inbound `Impersonate-*` headers are never read.** The identity comes from
///   the configured header names or from the configured anonymous identity, full
///   stop; [`super::impersonate`] then strips any inbound `Impersonate-*` before
///   inserting its own.
/// * **`system:` principals are refused** ([`is_forbidden_principal`]) on
///   everything a proxy asserts. The anonymous identity is *not* re-checked here,
///   because it is validated at startup instead
///   (`reject_system_identity` in [`crate::config`]) — which is exactly what lets
///   the debug-only `--dev-allow-system-groups` escape hatch exist without ever
///   loosening the header path a caller can reach.
pub fn extract_identity(headers: &HeaderMap, cfg: &AuthConfig) -> Result<Identity, AuthError> {
    match &cfg.mode {
        AuthMode::AnonymousOnly(anonymous) => anonymous_identity(anonymous, cfg),
        AuthMode::Headers {
            user,
            groups,
            anonymous_fallback,
        } => match header_str(headers, user)? {
            Some(name) => header_identity(headers, cfg, user, name, groups.as_ref()),
            None => match anonymous_fallback {
                Some(anonymous) => anonymous_identity(anonymous, cfg),
                None => Err(AuthError::NoIdentity {
                    expected_header: Some(user.as_str().to_string()),
                }),
            },
        },
    }
}

/// Build the identity a proxy asserted in the configured headers.
fn header_identity(
    headers: &HeaderMap,
    cfg: &AuthConfig,
    user_header: &HeaderName,
    user: &str,
    groups_header: Option<&HeaderName>,
) -> Result<Identity, AuthError> {
    check_principal(user_header.as_str(), user)?;
    reject_forbidden(user)?;

    let groups = match groups_header {
        Some(header) => asserted_groups(headers, cfg, header)?,
        None => Vec::new(),
    };

    let email = match &cfg.email_header {
        Some(header) => match header_str(headers, header)? {
            Some(value) => {
                check_principal(header.as_str(), value)?;
                Some(value.to_string())
            }
            None => None,
        },
        None => None,
    };

    finish(
        user.to_string(),
        groups,
        email,
        IdentitySource::TrustedHeaders,
        cfg,
    )
}

/// Collect the groups from *every* occurrence of the groups header.
///
/// A proxy is free to send one header with a separator-joined list, several
/// headers with one group each, or any mixture; all three mean the same thing and
/// all three are read.
fn asserted_groups(
    headers: &HeaderMap,
    cfg: &AuthConfig,
    header: &HeaderName,
) -> Result<Vec<String>, AuthError> {
    let mut groups = Vec::new();
    for value in headers.get_all(header) {
        let raw = value_str(header.as_str(), value)?;
        for group in split_groups(raw, &cfg.groups_separator) {
            check_principal(header.as_str(), group)?;
            reject_forbidden(group)?;
            groups.push(group.to_string());
        }
    }
    Ok(groups)
}

/// Build the identity from the operator-chosen anonymous configuration.
///
/// Its principals are not re-validated against the `system:` deny-list: they came
/// from `KOPIUR_UI_ANONYMOUS_*` and were checked — or, in a debug build with
/// `--dev-allow-system-groups`, deliberately waved through — when the config
/// resolved. Nothing a caller sends reaches this path.
fn anonymous_identity(
    anonymous: &AnonymousIdentity,
    cfg: &AuthConfig,
) -> Result<Identity, AuthError> {
    finish(
        anonymous.user.clone(),
        anonymous.groups.clone(),
        None,
        IdentitySource::Anonymous,
        cfg,
    )
}

/// Apply the parts of the pipeline every identity shares: the optional group
/// allowlist, `system:authenticated`, canonical ordering, the group bound, and
/// the `userextras` projection.
fn finish(
    user: String,
    mut groups: Vec<String>,
    email: Option<String>,
    source: IdentitySource,
    cfg: &AuthConfig,
) -> Result<Identity, AuthError> {
    if let Some(allowed) = &cfg.allowed_groups {
        groups.retain(|g| allowed.contains(g));
    }
    // Appended after the allowlist and never filtered by it: a caller whose every
    // group was filtered out is still an authenticated caller, and dropping this
    // would silently change which RoleBindings apply to them.
    if !groups.iter().any(|g| g == SYSTEM_AUTHENTICATED) {
        groups.push(SYSTEM_AUTHENTICATED.to_string());
    }
    // Canonical order, so two callers who differ only in the order their proxy
    // listed their groups share one cache entry and one set of outbound headers.
    groups.sort_unstable();
    groups.dedup();
    check_group_bound(&groups)?;

    let mut extra = BTreeMap::new();
    if let Some(address) = &email
        && cfg.extra_keys.iter().any(|k| k == EMAIL_EXTRA_KEY)
    {
        extra.insert(EMAIL_EXTRA_KEY.to_string(), vec![address.clone()]);
    }

    Ok(Identity {
        user,
        groups,
        email,
        extra,
        source,
    })
}

/// Bound the group count once the final set is known.
///
/// Checked on the emitted set rather than on the raw header, so a proxy that
/// repeats a group is not a 400 — what matters is how many `Impersonate-Group`
/// headers the identity turns into.
fn check_group_bound(groups: &[String]) -> Result<(), AuthError> {
    if groups.len() > MAX_GROUPS {
        return Err(AuthError::TooManyGroups {
            count: groups.len(),
            max: MAX_GROUPS,
        });
    }
    Ok(())
}

/// Split one groups-header value on the configured separator, trimming and
/// dropping empties.
///
/// An empty separator means "the whole value is one group": splitting on an empty
/// pattern would otherwise explode the value into single characters.
fn split_groups<'a>(value: &'a str, separator: &str) -> Vec<&'a str> {
    let parts: Vec<&str> = if separator.is_empty() {
        vec![value]
    } else {
        value.split(separator).collect()
    };
    parts
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Read a single-valued header as a trimmed string, or `None` when it is absent
/// or blank.
///
/// Blank is treated as absent because a proxy that did not authenticate anyone
/// commonly forwards the header with an empty value, and "" is not an identity.
fn header_str<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Result<Option<&'a str>, AuthError> {
    match headers.get(name) {
        Some(value) => {
            let text = value_str(name.as_str(), value)?.trim();
            Ok((!text.is_empty()).then_some(text))
        }
        None => Ok(None),
    }
}

/// Decode a header value, refusing anything that is not ASCII text.
fn value_str<'a>(header: &str, value: &'a HeaderValue) -> Result<&'a str, AuthError> {
    value.to_str().map_err(|_| AuthError::InvalidHeaderValue {
        header: header.to_string(),
        reason: "the value is not valid ASCII text".to_string(),
    })
}

/// Validate one principal: visible ASCII, non-empty, within
/// [`MAX_PRINCIPAL_BYTES`].
///
/// Visible ASCII (`!` through `~`) and nothing else. That excludes CR, LF and NUL
/// — the bytes a header-smuggling attempt needs — and also excludes the space,
/// which no Kubernetes principal in this deployment shape uses and whose presence
/// is far more likely to be a proxy concatenating two values than a real name.
fn check_principal(header: &str, value: &str) -> Result<(), AuthError> {
    if value.is_empty() {
        return Err(AuthError::InvalidHeaderValue {
            header: header.to_string(),
            reason: "the value is empty".to_string(),
        });
    }
    if value.len() > MAX_PRINCIPAL_BYTES {
        return Err(AuthError::InvalidHeaderValue {
            header: header.to_string(),
            reason: format!(
                "{} bytes is longer than the {MAX_PRINCIPAL_BYTES}-byte limit",
                value.len()
            ),
        });
    }
    match value.bytes().find(|b| !b.is_ascii_graphic()) {
        Some(bad) => Err(AuthError::InvalidHeaderValue {
            header: header.to_string(),
            reason: format!(
                "it contains the byte 0x{bad:02x}, which is not visible ASCII (only the \
                 characters ! through ~ are accepted)"
            ),
        }),
        None => Ok(()),
    }
}

/// Refuse a `system:` principal asserted by a proxy.
fn reject_forbidden(principal: &str) -> Result<(), AuthError> {
    if is_forbidden_principal(principal) {
        return Err(AuthError::ForbiddenPrincipal {
            principal: principal.to_string(),
            why: "the system: namespace is reserved for Kubernetes' own principals, and \
                  system:masters bypasses RBAC entirely",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_GROUPS_SEPARATOR;
    use std::collections::BTreeSet;

    const USER_HEADER: &str = "x-forwarded-user";
    const GROUPS_HEADER: &str = "x-forwarded-groups";
    const EMAIL_HEADER: &str = "x-forwarded-email";

    fn header_cfg() -> AuthConfig {
        AuthConfig {
            mode: AuthMode::Headers {
                user: HeaderName::from_static(USER_HEADER),
                groups: Some(HeaderName::from_static(GROUPS_HEADER)),
                anonymous_fallback: None,
            },
            groups_separator: DEFAULT_GROUPS_SEPARATOR.to_string(),
            email_header: Some(HeaderName::from_static(EMAIL_HEADER)),
            extra_keys: Vec::new(),
            allowed_groups: None,
            proxy_secret: None,
        }
    }

    fn anonymous_cfg(user: &str, groups: &[&str]) -> AuthConfig {
        AuthConfig {
            mode: AuthMode::AnonymousOnly(AnonymousIdentity {
                user: user.to_string(),
                groups: groups.iter().map(|g| (*g).to_string()).collect(),
            }),
            groups_separator: DEFAULT_GROUPS_SEPARATOR.to_string(),
            email_header: None,
            extra_keys: Vec::new(),
            allowed_groups: None,
            proxy_secret: None,
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::try_from(*name).expect("test header name"),
                HeaderValue::from_str(value).expect("test header value"),
            );
        }
        map
    }

    #[test]
    fn header_mode_without_the_user_header_and_without_a_fallback_is_no_identity() {
        let err = extract_identity(&headers(&[]), &header_cfg()).unwrap_err();
        assert_eq!(
            err,
            AuthError::NoIdentity {
                expected_header: Some(USER_HEADER.to_string()),
            }
        );
        // The remediation must name the header the proxy failed to set.
        assert!(err.to_string().contains(USER_HEADER), "{err}");
    }

    #[test]
    fn a_blank_user_header_is_the_same_as_no_header_at_all() {
        let err = extract_identity(&headers(&[(USER_HEADER, "   ")]), &header_cfg()).unwrap_err();
        assert!(matches!(err, AuthError::NoIdentity { .. }), "{err:?}");
    }

    #[test]
    fn header_mode_with_a_configured_fallback_serves_the_anonymous_identity() {
        let mut cfg = header_cfg();
        cfg.mode = AuthMode::Headers {
            user: HeaderName::from_static(USER_HEADER),
            groups: Some(HeaderName::from_static(GROUPS_HEADER)),
            anonymous_fallback: Some(AnonymousIdentity {
                user: "kopiur-ui-anonymous".to_string(),
                groups: vec!["readers".to_string()],
            }),
        };

        let id = extract_identity(&headers(&[]), &cfg).expect("fallback identity");
        assert_eq!(id.user, "kopiur-ui-anonymous");
        assert_eq!(id.groups, vec!["readers", SYSTEM_AUTHENTICATED]);
        assert_eq!(id.source, IdentitySource::Anonymous);
    }

    #[test]
    fn anonymous_only_mode_never_looks_at_the_headers() {
        let cfg = anonymous_cfg("viewer", &["readers"]);
        // Everything an attacker could try, all at once.
        let id = extract_identity(
            &headers(&[
                (USER_HEADER, "root"),
                (GROUPS_HEADER, "system:masters"),
                ("impersonate-user", "system:admin"),
                ("impersonate-group", "system:masters"),
            ]),
            &cfg,
        )
        .expect("anonymous identity");

        assert_eq!(id.user, "viewer");
        assert_eq!(id.groups, vec!["readers", SYSTEM_AUTHENTICATED]);
        assert_eq!(id.source, IdentitySource::Anonymous);
    }

    #[test]
    fn an_inbound_impersonate_header_has_no_effect_on_the_identity() {
        let id = extract_identity(
            &headers(&[
                (USER_HEADER, "alice"),
                ("impersonate-user", "system:admin"),
                ("impersonate-group", "system:masters"),
                ("impersonate-extra-scopes", "everything"),
            ]),
            &header_cfg(),
        )
        .expect("identity");

        assert_eq!(id.user, "alice");
        assert_eq!(id.groups, vec![SYSTEM_AUTHENTICATED]);
        assert!(id.extra.is_empty());
    }

    #[test]
    fn groups_come_from_every_occurrence_trimmed_deduped_and_sorted() {
        let id = extract_identity(
            &headers(&[
                (USER_HEADER, "alice"),
                (GROUPS_HEADER, "ops, dev ,,ops"),
                (GROUPS_HEADER, "platform"),
            ]),
            &header_cfg(),
        )
        .expect("identity");

        assert_eq!(
            id.groups,
            vec!["dev", "ops", "platform", SYSTEM_AUTHENTICATED]
        );
    }

    #[test]
    fn system_authenticated_is_never_duplicated_when_the_proxy_already_sent_it() {
        let id = extract_identity(
            &headers(&[
                (USER_HEADER, "alice"),
                (GROUPS_HEADER, "system:authenticated,ops"),
            ]),
            &header_cfg(),
        )
        .expect("identity");

        assert_eq!(id.groups, vec!["ops", SYSTEM_AUTHENTICATED]);
    }

    #[test]
    fn a_custom_separator_is_honoured() {
        let mut cfg = header_cfg();
        cfg.groups_separator = "|".to_string();
        let id = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, "ops|dev")]),
            &cfg,
        )
        .expect("identity");

        assert_eq!(id.groups, vec!["dev", "ops", SYSTEM_AUTHENTICATED]);
    }

    #[test]
    fn a_system_group_from_the_proxy_is_refused() {
        for group in ["system:masters", "system:serviceaccounts", "system:nodes"] {
            let err = extract_identity(
                &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, group)]),
                &header_cfg(),
            )
            .unwrap_err();
            assert!(
                matches!(err, AuthError::ForbiddenPrincipal { ref principal, .. }
                         if principal == group),
                "{group} must never be impersonated, got {err:?}"
            );
        }
    }

    #[test]
    fn a_system_user_from_the_proxy_is_refused() {
        let err = extract_identity(
            &headers(&[(USER_HEADER, "system:kube-controller-manager")]),
            &header_cfg(),
        )
        .unwrap_err();
        assert!(
            matches!(err, AuthError::ForbiddenPrincipal { ref principal, .. }
                     if principal == "system:kube-controller-manager"),
            "{err:?}"
        );
    }

    #[test]
    fn an_allowlisted_system_group_is_still_refused() {
        // The allowlist narrows what may be impersonated; it can never widen it.
        let mut cfg = header_cfg();
        cfg.allowed_groups = Some(BTreeSet::from(["system:masters".to_string()]));

        let err = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, "system:masters")]),
            &cfg,
        )
        .unwrap_err();
        assert!(
            matches!(err, AuthError::ForbiddenPrincipal { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_allowlist_intersects_and_still_leaves_system_authenticated() {
        let mut cfg = header_cfg();
        cfg.allowed_groups = Some(BTreeSet::from(["ops".to_string()]));

        let kept = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, "ops,dev")]),
            &cfg,
        )
        .expect("identity");
        assert_eq!(kept.groups, vec!["ops", SYSTEM_AUTHENTICATED]);

        let none = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, "dev,marketing")]),
            &cfg,
        )
        .expect("identity");
        assert_eq!(
            none.groups,
            vec![SYSTEM_AUTHENTICATED],
            "a caller with no allowed group is still authenticated"
        );
    }

    #[test]
    fn a_principal_over_the_byte_limit_is_refused() {
        let long = "a".repeat(MAX_PRINCIPAL_BYTES + 1);
        let err = extract_identity(&headers(&[(USER_HEADER, &long)]), &header_cfg()).unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidHeaderValue { ref header, .. } if header == USER_HEADER),
            "{err:?}"
        );
        assert!(err.to_string().contains("512-byte limit"), "{err}");
    }

    #[test]
    fn a_principal_that_is_not_visible_ascii_is_refused() {
        // A tab is a legal `HeaderValue` byte, so only this check catches it.
        let err =
            extract_identity(&headers(&[(USER_HEADER, "alice\tbob")]), &header_cfg()).unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidHeaderValue { .. }),
            "{err:?}"
        );

        let err =
            extract_identity(&headers(&[(USER_HEADER, "alice bob")]), &header_cfg()).unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidHeaderValue { .. }),
            "a space is not a visible-ASCII principal: {err:?}"
        );
    }

    #[test]
    fn more_groups_than_the_bound_is_refused() {
        let many: Vec<String> = (0..=MAX_GROUPS).map(|i| format!("g{i}")).collect();
        let err = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, &many.join(","))]),
            &header_cfg(),
        )
        .unwrap_err();

        // `system:authenticated` is appended, so 65 asserted groups become 66.
        assert_eq!(
            err,
            AuthError::TooManyGroups {
                count: MAX_GROUPS + 2,
                max: MAX_GROUPS,
            }
        );
    }

    #[test]
    fn exactly_the_bound_is_accepted() {
        let many: Vec<String> = (0..MAX_GROUPS - 1).map(|i| format!("g{i}")).collect();
        let id = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, &many.join(","))]),
            &header_cfg(),
        )
        .expect("identity");
        assert_eq!(id.groups.len(), MAX_GROUPS);
    }

    #[test]
    fn email_is_display_only_unless_it_is_a_configured_extra_key() {
        let sent = &[(USER_HEADER, "alice"), (EMAIL_HEADER, "alice@example.com")];

        let display_only = extract_identity(&headers(sent), &header_cfg()).expect("identity");
        assert_eq!(display_only.email.as_deref(), Some("alice@example.com"));
        assert!(
            display_only.extra.is_empty(),
            "an extra the ClusterRole does not name must never be emitted"
        );

        let mut cfg = header_cfg();
        cfg.extra_keys = vec![EMAIL_EXTRA_KEY.to_string()];
        let impersonated = extract_identity(&headers(sent), &cfg).expect("identity");
        assert_eq!(
            impersonated.extra.get(EMAIL_EXTRA_KEY),
            Some(&vec!["alice@example.com".to_string()])
        );
    }

    #[test]
    fn the_email_header_is_validated_like_any_other_principal() {
        let err = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (EMAIL_HEADER, "a b@example.com")]),
            &header_cfg(),
        )
        .unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidHeaderValue { ref header, .. } if header == EMAIL_HEADER),
            "{err:?}"
        );
    }

    #[test]
    fn header_mode_without_a_groups_header_yields_only_system_authenticated() {
        let mut cfg = header_cfg();
        cfg.mode = AuthMode::Headers {
            user: HeaderName::from_static(USER_HEADER),
            groups: None,
            anonymous_fallback: None,
        };
        let id = extract_identity(
            &headers(&[(USER_HEADER, "alice"), (GROUPS_HEADER, "system:masters")]),
            &cfg,
        )
        .expect("identity");

        assert_eq!(id.user, "alice");
        assert_eq!(
            id.groups,
            vec![SYSTEM_AUTHENTICATED],
            "an unconfigured groups header is not read, so its content cannot escalate"
        );
    }

    #[test]
    fn only_system_authenticated_escapes_the_deny_list() {
        assert!(!is_forbidden_principal(SYSTEM_AUTHENTICATED));
        assert!(!is_forbidden_principal("alice"));
        assert!(!is_forbidden_principal("systemd"));
        assert!(is_forbidden_principal("system:masters"));
        assert!(is_forbidden_principal("system:anonymous"));
        assert!(is_forbidden_principal("system:"));
    }
}
