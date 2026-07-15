//! Kopia identity resolution (ADR §4.2).
//!
//! Kopia records every snapshot under `username@hostname:sourcePath`. Kopiur makes
//! that identity an explicit, overridable part of the API rather than an accident
//! of `metadata.name`/`metadata.namespace` (ADR §2.2 principle 9). This module is
//! the single place the defaulting + templating rules live. The webhook calls it
//! at admission (so a bad expression/component is rejected on `kubectl apply`) and
//! the controller calls it again on every reconcile, resolving from the LIVE
//! `SnapshotPolicy.spec.identity` and the LIVE referenced repository's
//! `identityDefaults` — **not** a value pinned once and frozen. `status.resolved.identity`
//! mirrors the most recent resolution for observability; it is not the source of
//! truth a later run reads back. What actually keeps an already-snapshotted policy
//! stable is the fork guard (`ValidationError::IdentityWouldFork`/
//! `RepositoryIdentityWouldFork`): an edit that would change the resolved identity
//! on a policy (or repository) with existing history is rejected at admission
//! unless acknowledged with the `allow-identity-change` annotation.
//!
//! ## Defaults (ADR §4.2)
//! - `username` ← `SnapshotPolicy.metadata.name`
//! - `hostname` ← namespace
//! - `sourcePath` ← `/pvc/<pvcName>`
//!
//! ## Repository/ClusterRepository identity expressions (CEL)
//!
//! A [`crate::common::IdentityDefaults`] (`Repository.spec.identityDefaults` or
//! `ClusterRepository.spec.identityDefaults`) supplies `hostnameExpr`/
//! `usernameExpr`, **CEL** expressions ([`cel`]) validated (compiled + trial-evaluated
//! via [`validate_identity_expr`]) at admission, then actually rendered against the
//! LIVE consumer + LIVE repository on every reconcile (ADR-0004 §5) — so editing
//! `identityDefaults` re-renders every consumer that resolves through it on its
//! next backup (guarded by [`ValidationError::RepositoryIdentityWouldFork`], see
//! the module intro above). A consumer's explicit [`Identity`] override **always
//! wins** over the expression.
//!
//! ### CEL environment
//!
//! Each expression returns a **string** and is evaluated against:
//! `namespace` (the consumer's namespace), `policyName` (the `SnapshotPolicy`'s
//! name), `labels` and `annotations` (its metadata maps), and `cluster`
//! ([`IdentityDefaults::cluster`], or `""` when unset — see [`identity_context`]).
//! Examples: `hostnameExpr: "namespace"`,
//! `usernameExpr: "namespace + '-' + policyName"`,
//! `"'team' in labels ? labels['team'] : namespace"`,
//! `"namespace + '.' + cluster"`. CEL is sandboxed (no I/O, no arbitrary code); a
//! syntax error or out-of-scope variable is rejected at `kubectl apply` via
//! [`validate_identity_expr`], and a non-string result is a typed error.
//! Expressions are length-capped ([`MAX_EXPR_LEN`]) as the cost-budget surrogate.
//!
//! ### Multi-cluster hostname default
//!
//! When a repository's [`IdentityDefaults::cluster`] is set, the default
//! (no override, no `hostnameExpr`) kopia identity hostname becomes
//! `<namespace>.<cluster>` instead of bare `<namespace>`, so N clusters sharing one
//! repository never collide on a same-named namespace. [`classify_hostname`]
//! recovers the namespace/cluster split on the read path (retention, discovered
//! `Snapshot` placement).

use std::collections::BTreeMap;

use cel::{Context, Program, Value};

use crate::common::{Identity, IdentityDefaults, ResolvedIdentity};
use crate::error::{ValidationError, ValidationResult};

/// Maximum CEL expression length accepted at admission (the cost-budget surrogate;
/// `cel` 0.13 has no built-in cost API). 1 KiB is far beyond any real identity
/// expression and bounds parse/eval work on adversarial input.
pub const MAX_EXPR_LEN: usize = 1024;

/// Inputs to identity resolution. Grouped into a struct so call sites are readable
/// and future inputs slot in without churning the signature.
#[derive(Debug, Clone)]
pub struct IdentityInputs<'a> {
    /// The consumer object's `metadata.name` (default `username`; the `policyName`
    /// CEL variable).
    pub object_name: &'a str,
    /// The consumer object's namespace (default `hostname`; the `namespace` CEL
    /// variable).
    pub namespace: &'a str,
    /// Explicit overrides from `SnapshotPolicy.spec.identity`, if any.
    pub overrides: Option<&'a Identity>,
    /// The referenced repository's `spec.identityDefaults` — `Repository` or
    /// `ClusterRepository`, whichever the consumer targets. Carries the CEL
    /// `*Expr` pair (`hostnameExpr`/`usernameExpr`) AND `cluster`, the
    /// multi-cluster hostname-default suffix (see [`IdentityDefaults::cluster`]
    /// and the module's "Multi-cluster hostname default" section).
    pub defaults: Option<&'a IdentityDefaults>,
    /// The consumer's `metadata.labels`, exposed to CEL as `labels`.
    pub labels: Option<&'a BTreeMap<String, String>>,
    /// The consumer's `metadata.annotations`, exposed to CEL as `annotations`.
    pub annotations: Option<&'a BTreeMap<String, String>>,
    /// The PVC name backing `sourcePath`'s `/pvc/<name>` default. `None` for
    /// surfaces without a single PVC (a non-PVC source like NFS, or a maintenance
    /// identity). When set it takes precedence over [`Self::default_source_path`].
    pub pvc_name: Option<&'a str>,
    /// The `sourcePath` default for a non-PVC source (e.g. an NFS export's path),
    /// used when there is no `pvc_name` and no override. `None` leaves `sourcePath`
    /// unset (kopia's identity-only `username@hostname` form).
    pub default_source_path: Option<&'a str>,
    /// An explicit `sourcePathOverride` (ADR §3.3), which beats every default.
    pub source_path_override: Option<&'a str>,
}

/// Compile a CEL identity expression, enforcing the [`MAX_EXPR_LEN`] budget first.
/// Maps a parse failure to [`ValidationError::IdentityExprCompile`].
fn compile_expr(expr: &str) -> ValidationResult<Program> {
    if expr.len() > MAX_EXPR_LEN {
        return Err(ValidationError::IdentityExprCompile {
            expr: expr.to_string(),
            reason: format!(
                "expression is {} bytes; the maximum is {MAX_EXPR_LEN}",
                expr.len()
            ),
        });
    }
    Program::compile(expr).map_err(|e| ValidationError::IdentityExprCompile {
        expr: expr.to_string(),
        reason: e.to_string(),
    })
}

/// Build the CEL evaluation context for identity expressions: `namespace`,
/// `policyName`, `labels`, `annotations`, `cluster`. `cluster` is **always**
/// declared (so an expression referencing it never hits an undeclared-variable
/// error), taking `inputs.defaults.and_then(|d| d.cluster.as_deref())` or `""`
/// when the consumer's repository has no `identityDefaults.cluster` set (bare
/// single-cluster deployments never see a `cluster` variable value).
fn identity_context<'a>(inputs: &IdentityInputs<'_>) -> Context<'a> {
    let mut ctx = Context::default();
    // `add_variable` only errors if a value cannot serialize; these are
    // `&str`/`BTreeMap<String,String>`, which always serialize.
    let empty = BTreeMap::<String, String>::new();
    let _ = ctx.add_variable("namespace", inputs.namespace);
    let _ = ctx.add_variable("policyName", inputs.object_name);
    let _ = ctx.add_variable("labels", inputs.labels.unwrap_or(&empty));
    let _ = ctx.add_variable("annotations", inputs.annotations.unwrap_or(&empty));
    let cluster = inputs
        .defaults
        .and_then(|d| d.cluster.as_deref())
        .unwrap_or("");
    let _ = ctx.add_variable("cluster", cluster);
    ctx
}

/// Evaluate a compiled identity [`Program`] against `inputs`, requiring a string
/// result. Maps evaluation failure to [`ValidationError::IdentityExprEval`] and a
/// non-string result to [`ValidationError::IdentityExprType`].
fn eval_expr(
    expr: &str,
    program: &Program,
    inputs: &IdentityInputs<'_>,
) -> ValidationResult<String> {
    let ctx = identity_context(inputs);
    match program.execute(&ctx) {
        Ok(Value::String(s)) => Ok(s.to_string()),
        Ok(other) => Err(ValidationError::IdentityExprType {
            expr: expr.to_string(),
            got: other.type_of().to_string(),
        }),
        Err(e) => Err(ValidationError::IdentityExprEval {
            expr: expr.to_string(),
            reason: e.to_string(),
        }),
    }
}

/// Compile + evaluate an identity expression in one step.
fn render_expr(expr: &str, inputs: &IdentityInputs<'_>) -> ValidationResult<String> {
    let program = compile_expr(expr)?;
    eval_expr(expr, &program, inputs)
}

/// Validate a `Repository`/`ClusterRepository` `identityDefaults` CEL expression
/// at admission (ADR-0004 §5): it must compile, and — because CEL reports an
/// out-of-scope variable only at *evaluation* time — it must also evaluate
/// against a representative context without referencing an undeclared variable.
/// A non-string result is rejected. Missing *map keys* (e.g. `labels['env']`
/// when the trial data lacks `env`) are tolerated: they are data-dependent, not
/// a structural error.
pub fn validate_identity_expr(expr: &str) -> ValidationResult {
    let program = compile_expr(expr)?;
    // Representative trial context: non-empty maps so `'k' in labels`-style guards
    // behave, plus placeholder scalars. `cluster` gets a non-empty placeholder too
    // (rather than the "unset" empty string [`identity_context`] would otherwise
    // supply) so an expression referencing it — e.g. `namespace + '.' + cluster` —
    // trial-evaluates the same way regardless of whether *this particular*
    // repository happens to set `identityDefaults.cluster`.
    let labels = BTreeMap::from([("app".to_string(), "trial".to_string())]);
    let annotations = BTreeMap::from([("note".to_string(), "trial".to_string())]);
    let trial_defaults = IdentityDefaults {
        cluster: Some("trial-cluster".to_string()),
        hostname_expr: None,
        username_expr: None,
    };
    let inputs = IdentityInputs {
        object_name: "policy",
        namespace: "namespace",
        overrides: None,
        defaults: Some(&trial_defaults),
        labels: Some(&labels),
        annotations: Some(&annotations),
        pvc_name: None,
        default_source_path: None,
        source_path_override: None,
    };
    let ctx = identity_context(&inputs);
    match program.execute(&ctx) {
        Ok(Value::String(_)) => Ok(()),
        Ok(other) => Err(ValidationError::IdentityExprType {
            expr: expr.to_string(),
            got: other.type_of().to_string(),
        }),
        // An undeclared-variable reference (a typo / out-of-scope var) is a hard
        // rejection. Other runtime errors (e.g. NoSuchKey on a label the trial data
        // lacks) are data-dependent and tolerated — the real object may supply them.
        Err(cel::ExecutionError::UndeclaredReference(name)) => {
            Err(ValidationError::IdentityExprEval {
                expr: expr.to_string(),
                reason: format!("undeclared reference to '{name}'"),
            })
        }
        Err(_) => Ok(()),
    }
}

/// Resolve a [`ResolvedIdentity`] from defaults, an optional `Repository`/
/// `ClusterRepository` identity expression set, and explicit consumer overrides
/// (ADR §4.2 / ADR-0004 §5).
///
/// Precedence per component: **explicit override, then expression, then default**.
/// For `hostname` specifically, the default step itself has two tiers: with
/// [`IdentityDefaults::cluster`] set, the default is `<namespace>.<cluster>`
/// (M1, multi-cluster shared repositories); otherwise it's the bare `namespace`
/// (ADR §4.2). So the full hostname chain, highest precedence first, is:
/// explicit override, `hostnameExpr`, `<namespace>.<cluster>` (cluster set),
/// `namespace`. `username` is unaffected — `cluster` is not part of its default.
/// Returns a
/// [`ValidationError::IdentityExprCompile`]/`IdentityExprEval`/`IdentityExprType` if a
/// supplied expression fails (so the webhook rejects it at admission rather than
/// pinning garbage).
///
/// ```
/// use kopiur_api::{IdentityInputs, resolve_identity, identity_string};
///
/// // Bare defaults: username <- object name, hostname <- namespace,
/// // sourcePath <- /pvc/<pvcName> (ADR §4.2).
/// let inputs = IdentityInputs {
///     object_name: "postgres-data",
///     namespace: "billing",
///     overrides: None,
///     defaults: None,
///     labels: None,
///     annotations: None,
///     pvc_name: Some("postgres-data"),
///     default_source_path: None,
///     source_path_override: None,
/// };
/// let id = resolve_identity(&inputs).unwrap();
/// assert_eq!(id.username, "postgres-data");
/// assert_eq!(id.hostname, "billing");
/// assert_eq!(id.source_path.as_deref(), Some("/pvc/postgres-data"));
/// assert_eq!(identity_string(&id), "postgres-data@billing:/pvc/postgres-data");
/// ```
pub fn resolve_identity(inputs: &IdentityInputs<'_>) -> ValidationResult<ResolvedIdentity> {
    let override_username = inputs.overrides.and_then(|o| o.username.as_deref());
    let override_hostname = inputs.overrides.and_then(|o| o.hostname.as_deref());

    let username = match override_username {
        Some(u) => u.to_string(),
        None => match inputs.defaults.and_then(|t| t.username_expr.as_deref()) {
            Some(expr) => render_expr(expr, inputs)?,
            None => inputs.object_name.to_string(),
        },
    };

    let hostname = match override_hostname {
        Some(h) => h.to_string(),
        None => match inputs.defaults.and_then(|t| t.hostname_expr.as_deref()) {
            Some(expr) => render_expr(expr, inputs)?,
            // No explicit expression: fall back to `<namespace>.<cluster>` when this
            // repository has a cluster identity configured (M1 — distinguishes
            // same-named namespaces across clusters sharing one repository), else the
            // plain namespace (ADR §4.2).
            None => match inputs.defaults.and_then(|d| d.cluster.as_deref()) {
                Some(cluster) => format!("{}.{cluster}", inputs.namespace),
                None => inputs.namespace.to_string(),
            },
        },
    };

    let source_path = match inputs.source_path_override {
        Some(p) => Some(p.to_string()),
        None => inputs
            .pvc_name
            .map(|n| format!("/pvc/{n}"))
            .or_else(|| inputs.default_source_path.map(String::from)),
    };

    // Shape-check the fully-resolved identity, regardless of where each component came
    // from (explicit override, CEL expression result, or the name/namespace/PVC
    // default). kopia parses `username@hostname:path` on the first `@`/`:` with no
    // escaping, so a component carrying a delimiter/whitespace/control char would
    // silently misparse or become un-findable. Rejecting here means the controller
    // never PINS a bad identity into status, and the webhook surfaces it at admission.
    crate::validate::validate_identity_component("resolved username", &username)?;
    crate::validate::validate_identity_component("resolved hostname", &hostname)?;
    if let Some(p) = &source_path {
        crate::validate::validate_source_path("resolved sourcePath", p)?;
    }

    Ok(ResolvedIdentity {
        username,
        hostname,
        source_path,
    })
}

/// Format a kopia identity string. With a source path: `username@hostname:path`;
/// without one: `username@hostname` (kopia's identity-only form, used for catalog
/// queries that aren't pinned to a path).
///
/// ```
/// use kopiur_api::{IdentityInputs, resolve_identity, identity_string};
///
/// // No PVC => no sourcePath => kopia's identity-only `username@hostname` form.
/// let inputs = IdentityInputs {
///     object_name: "cfg",
///     namespace: "ns",
///     overrides: None,
///     defaults: None,
///     labels: None,
///     annotations: None,
///     pvc_name: None,
///     default_source_path: None,
///     source_path_override: None,
/// };
/// let id = resolve_identity(&inputs).unwrap();
/// assert_eq!(id.source_path, None);
/// assert_eq!(identity_string(&id), "cfg@ns");
/// ```
pub fn identity_string(id: &ResolvedIdentity) -> String {
    match &id.source_path {
        Some(p) => format!("{}@{}:{}", id.username, id.hostname, p),
        None => format!("{}@{}", id.username, id.hostname),
    }
}

/// Classification of a snapshot identity hostname relative to THIS cluster.
/// Kubernetes namespace names cannot contain `.`, so the FIRST `.` unambiguously
/// ends the namespace part of a `<namespace>.<cluster>` hostname. Total: with no
/// cluster identity every hostname is `Bare`; an empty namespace part (".east")
/// or empty suffix ("ns.") also classifies `Bare` (never a panic, never an
/// invalid empty namespace). Suffix comparison is case-sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass<'a> {
    /// No dot (or cluster mode off): the legacy `hostname == namespace` form.
    Bare {
        /// The hostname verbatim (there is no separate suffix to strip).
        namespace: &'a str,
    },
    /// `<namespace>.<suffix>` where suffix == this cluster: written by us.
    OwnCluster {
        /// The part of the hostname before the first `.`.
        namespace: &'a str,
    },
    /// `<namespace>.<suffix>` with a different suffix: another cluster's.
    ForeignCluster {
        /// Everything after the first `.` (may itself contain further dots).
        suffix: &'a str,
    },
}

/// Classify a kopia identity `hostname` against `my_cluster`
/// ([`IdentityDefaults::cluster`], resolved from the consuming repository), so a
/// reader (retention pruning, discovered-`Snapshot` placement) can tell whether a
/// hostname it sees was written by this cluster, another cluster sharing the same
/// repository, or predates cluster identity entirely.
///
/// Total and pure — never panics, never fabricates an empty namespace:
/// - `my_cluster` is `None` or `""` (no cluster identity configured): always
///   [`HostClass::Bare`].
/// - No `.` in `hostname`: [`HostClass::Bare`].
/// - The part before the first `.` is empty (e.g. `".east"`), or the part after
///   is empty (e.g. `"ns."`): [`HostClass::Bare`] with the **whole** hostname as
///   `namespace` — these aren't well-formed `<namespace>.<cluster>` hostnames, so
///   classification declines to guess which part is which.
/// - The suffix (everything after the first `.`) exactly equals `my_cluster`:
///   [`HostClass::OwnCluster`].
/// - Otherwise: [`HostClass::ForeignCluster`] — note `"ns.east.x"` under cluster
///   `"east"` is `ForeignCluster { suffix: "east.x" }`, not `OwnCluster`; only an
///   **exact** suffix match is ours.
///
/// ```
/// use kopiur_api::identity::{HostClass, classify_hostname};
///
/// assert_eq!(
///     classify_hostname("billing.east", Some("east")),
///     HostClass::OwnCluster { namespace: "billing" }
/// );
/// assert_eq!(
///     classify_hostname("billing.west", Some("east")),
///     HostClass::ForeignCluster { suffix: "west" }
/// );
/// // No cluster identity configured => every hostname reads as legacy/bare.
/// assert_eq!(
///     classify_hostname("billing.east", None),
///     HostClass::Bare { namespace: "billing.east" }
/// );
/// ```
pub fn classify_hostname<'a>(hostname: &'a str, my_cluster: Option<&str>) -> HostClass<'a> {
    let my_cluster = match my_cluster {
        Some(c) if !c.is_empty() => c,
        _ => {
            return HostClass::Bare {
                namespace: hostname,
            };
        }
    };
    match hostname.split_once('.') {
        None => HostClass::Bare {
            namespace: hostname,
        },
        Some((namespace, suffix)) if namespace.is_empty() || suffix.is_empty() => HostClass::Bare {
            namespace: hostname,
        },
        Some((namespace, suffix)) if suffix == my_cluster => HostClass::OwnCluster { namespace },
        Some((_, suffix)) => HostClass::ForeignCluster { suffix },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        name: &'a str,
        ns: &'a str,
        overrides: Option<&'a Identity>,
        defaults: Option<&'a IdentityDefaults>,
        pvc: Option<&'a str>,
    ) -> IdentityInputs<'a> {
        IdentityInputs {
            object_name: name,
            namespace: ns,
            overrides,
            defaults,
            labels: None,
            annotations: None,
            pvc_name: pvc,
            default_source_path: None,
            source_path_override: None,
        }
    }

    fn defaults(host: Option<&str>, user: Option<&str>) -> IdentityDefaults {
        IdentityDefaults {
            cluster: None,
            hostname_expr: host.map(String::from),
            username_expr: user.map(String::from),
        }
    }

    /// Like [`defaults`] but also sets `identityDefaults.cluster` (M1 precedence tests).
    fn defaults_with_cluster(
        host: Option<&str>,
        user: Option<&str>,
        cluster: Option<&str>,
    ) -> IdentityDefaults {
        IdentityDefaults {
            cluster: cluster.map(String::from),
            hostname_expr: host.map(String::from),
            username_expr: user.map(String::from),
        }
    }

    #[test]
    fn nfs_source_uses_default_source_path() {
        // No PVC, but an NFS export supplies the sourcePath default.
        let mut i = inputs("media", "default", None, None, None);
        i.default_source_path = Some("/mnt/eros/Media");
        let r = resolve_identity(&i).unwrap();
        assert_eq!(r.source_path.as_deref(), Some("/mnt/eros/Media"));
        assert_eq!(identity_string(&r), "media@default:/mnt/eros/Media");
    }

    #[test]
    fn override_beats_default_source_path() {
        let mut i = inputs("media", "default", None, None, None);
        i.default_source_path = Some("/mnt/eros/Media");
        i.source_path_override = Some("/data");
        let r = resolve_identity(&i).unwrap();
        assert_eq!(r.source_path.as_deref(), Some("/data"));
    }

    #[test]
    fn defaults_use_name_namespace_and_pvc_path() {
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            None,
            None,
            Some("postgres-data"),
        ))
        .unwrap();
        assert_eq!(r.username, "postgres-data");
        assert_eq!(r.hostname, "billing");
        assert_eq!(r.source_path.as_deref(), Some("/pvc/postgres-data"));
    }

    // --- M1: identityDefaults.cluster hostname precedence -------------------
    // explicit override > hostnameExpr > `<namespace>.<cluster>` (cluster set) > namespace.

    #[test]
    fn cluster_default_is_used_when_no_override_or_expr() {
        let d = defaults_with_cluster(None, None, Some("east"));
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            None,
            Some(&d),
            Some("data"),
        ))
        .unwrap();
        assert_eq!(r.hostname, "billing.east");
        // username is unaffected — cluster is not part of its default.
        assert_eq!(r.username, "postgres-data");
    }

    #[test]
    fn no_cluster_no_expr_no_override_falls_back_to_bare_namespace() {
        // The `defaults_use_name_namespace_and_pvc_path` test above already covers
        // `defaults: None`; this covers `defaults: Some(..)` with `cluster: None`.
        let d = defaults_with_cluster(None, None, None);
        let r = resolve_identity(&inputs("cfg", "billing", None, Some(&d), Some("data"))).unwrap();
        assert_eq!(r.hostname, "billing");
    }

    #[test]
    fn explicit_hostname_override_beats_cluster_default() {
        let d = defaults_with_cluster(None, None, Some("east"));
        let ovr = Identity {
            username: None,
            hostname: Some("pinned-host".to_string()),
        };
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            Some(&ovr),
            Some(&d),
            Some("data"),
        ))
        .unwrap();
        assert_eq!(r.hostname, "pinned-host");
    }

    #[test]
    fn hostname_expr_beats_cluster_default() {
        // Both a hostnameExpr AND a cluster default are set; the expr wins (the
        // cluster tier only applies when there is no expression at all).
        let d = defaults_with_cluster(Some("namespace"), None, Some("east"));
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            None,
            Some(&d),
            Some("data"),
        ))
        .unwrap();
        assert_eq!(r.hostname, "billing"); // NOT "billing.east"
    }

    #[test]
    fn hostname_expr_can_reference_cluster_variable() {
        // hostnameExpr explicitly renders using `cluster` — the brief's example.
        let d = defaults_with_cluster(Some("namespace + '.' + cluster"), None, Some("east"));
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            None,
            Some(&d),
            Some("data"),
        ))
        .unwrap();
        assert_eq!(r.hostname, "billing.east");
    }

    #[test]
    fn cluster_variable_is_empty_string_when_defaults_lack_cluster() {
        // Same expression, but `identityDefaults.cluster` isn't set: `cluster`
        // evaluates to "" (identity_context's documented "unset" value).
        let d = defaults_with_cluster(Some("namespace + '.' + cluster"), None, None);
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            None,
            Some(&d),
            Some("data"),
        ))
        .unwrap();
        assert_eq!(r.hostname, "billing.");
    }

    #[test]
    fn validate_identity_expr_admits_cluster_reference_even_without_a_real_cluster_default() {
        // The *admission-time trial* always declares `cluster` (a placeholder),
        // independent of whether the real ClusterRepository sets
        // identityDefaults.cluster — so this must admit even though no `defaults`
        // is threaded through validate_identity_expr's caller in this test.
        assert!(validate_identity_expr("namespace + '.' + cluster").is_ok());
        assert!(validate_identity_expr("cluster").is_ok());
    }

    #[test]
    fn adr_cluster_repository_cel_example() {
        // ADR-0004 §5 example:
        //   hostnameExpr: "namespace"
        //   usernameExpr: "namespace + '-' + policyName"
        // For namespace `billing`, policy `postgres-data`, must evaluate to
        //   username = billing-postgres-data, hostname = billing.
        let d = defaults(Some("namespace"), Some("namespace + '-' + policyName"));
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            None,
            Some(&d),
            Some("data"),
        ))
        .unwrap();
        assert_eq!(r.username, "billing-postgres-data");
        assert_eq!(r.hostname, "billing");
    }

    #[test]
    fn cel_conditional_on_labels_resolves() {
        // ADR-0004 §5 conditional: 'team' in labels ? labels['team'] : namespace.
        let d = defaults(
            Some("'team' in labels ? labels['team'] : namespace"),
            Some("namespace + '-' + policyName"),
        );
        let labels = BTreeMap::from([("team".to_string(), "payments".to_string())]);
        let mut i = inputs("atuin", "default", None, Some(&d), Some("data"));
        i.labels = Some(&labels);
        let r = resolve_identity(&i).unwrap();
        assert_eq!(r.hostname, "payments"); // label present → label value
        assert_eq!(r.username, "default-atuin");

        // No `team` label → falls back to namespace.
        let mut i2 = inputs("atuin", "default", None, Some(&d), Some("data"));
        i2.labels = None;
        assert_eq!(resolve_identity(&i2).unwrap().hostname, "default");
    }

    #[test]
    fn override_beats_expr() {
        let d = defaults(Some("namespace"), Some("namespace + '-' + policyName"));
        let ovr = Identity {
            username: Some("custom-user".to_string()),
            hostname: Some("custom-host".to_string()),
        };
        let r = resolve_identity(&inputs("cfg", "ns", Some(&ovr), Some(&d), Some("p"))).unwrap();
        assert_eq!(r.username, "custom-user");
        assert_eq!(r.hostname, "custom-host");
    }

    #[test]
    fn partial_override_falls_through_to_expr_for_the_other_field() {
        let d = defaults(Some("namespace"), Some("namespace + '-' + policyName"));
        // Only hostname overridden; username still comes from the expression.
        let ovr = Identity {
            username: None,
            hostname: Some("pinned-host".to_string()),
        };
        let r = resolve_identity(&inputs(
            "postgres-data",
            "billing",
            Some(&ovr),
            Some(&d),
            Some("d"),
        ))
        .unwrap();
        assert_eq!(r.hostname, "pinned-host");
        assert_eq!(r.username, "billing-postgres-data");
    }

    #[test]
    fn source_path_override_beats_default() {
        let mut i = inputs("cfg", "ns", None, None, Some("vol"));
        i.source_path_override = Some("/data");
        let r = resolve_identity(&i).unwrap();
        assert_eq!(r.source_path.as_deref(), Some("/data"));
    }

    #[test]
    fn no_pvc_yields_no_source_path() {
        let r = resolve_identity(&inputs("cfg", "ns", None, None, None)).unwrap();
        assert_eq!(r.source_path, None);
    }

    #[test]
    fn identity_string_with_and_without_path() {
        let with = ResolvedIdentity {
            username: "postgres-data".into(),
            hostname: "billing".into(),
            source_path: Some("/data".into()),
        };
        assert_eq!(identity_string(&with), "postgres-data@billing:/data");
        let without = ResolvedIdentity {
            source_path: None,
            ..with
        };
        assert_eq!(identity_string(&without), "postgres-data@billing");
    }

    #[test]
    fn resolve_rejects_override_with_kopia_delimiter() {
        // An explicit override carrying kopia's '@' delimiter would misparse — rejected
        // at resolution so the controller never pins it.
        let ovr = Identity {
            username: Some("bad@user".to_string()),
            hostname: None,
        };
        let err = resolve_identity(&inputs("c", "n", Some(&ovr), None, Some("p"))).unwrap_err();
        assert!(matches!(
            err,
            ValidationError::IdentityComponentInvalid { .. }
        ));
    }

    #[test]
    fn resolve_rejects_cel_result_with_delimiter() {
        // A CEL expression that resolves to a string with ':' is caught at resolution
        // (the static override validator can't see CEL results).
        let d = defaults(None, Some("'a:b'"));
        let err = resolve_identity(&inputs("c", "n", None, Some(&d), Some("p"))).unwrap_err();
        assert!(matches!(
            err,
            ValidationError::IdentityComponentInvalid { .. }
        ));
    }

    #[test]
    fn malformed_expr_is_rejected_at_resolve() {
        let d = defaults(Some("namespace +"), None); // syntax error
        let err = resolve_identity(&inputs("c", "n", None, Some(&d), Some("p"))).unwrap_err();
        assert!(matches!(err, ValidationError::IdentityExprCompile { .. }));
    }

    // --- validate_identity_expr (admission-time check, ADR-0004 §5) ---

    #[test]
    fn validate_accepts_valid_string_exprs() {
        assert!(validate_identity_expr("namespace").is_ok());
        assert!(validate_identity_expr("namespace + '-' + policyName").is_ok());
        assert!(validate_identity_expr("'team' in labels ? labels['team'] : namespace").is_ok());
        // Data-dependent map index is tolerated (the real object may supply the key).
        assert!(
            validate_identity_expr("namespace + (labels['env'] == 'prod' ? '-prod' : '')").is_ok()
        );
    }

    #[test]
    fn validate_rejects_syntax_error() {
        let err = validate_identity_expr("namespace +").unwrap_err();
        assert!(matches!(err, ValidationError::IdentityExprCompile { .. }));
    }

    #[test]
    fn validate_rejects_out_of_scope_variable() {
        // `namspace` is a typo — an undeclared reference, caught at trial-eval.
        let err = validate_identity_expr("namspace").unwrap_err();
        assert!(matches!(err, ValidationError::IdentityExprEval { .. }));
    }

    #[test]
    fn validate_rejects_non_string_result() {
        // A bool/int result is not a valid hostname/username.
        let err = validate_identity_expr("1 + 1").unwrap_err();
        assert!(matches!(err, ValidationError::IdentityExprType { .. }));
    }

    #[test]
    fn validate_rejects_over_length_expr() {
        let long = format!("'{}'", "a".repeat(MAX_EXPR_LEN));
        let err = validate_identity_expr(&long).unwrap_err();
        assert!(matches!(err, ValidationError::IdentityExprCompile { .. }));
    }

    // --- classify_hostname (M1) ---

    #[test]
    fn classify_hostname_table() {
        let cases: &[(&str, Option<&str>, HostClass<'_>)] = &[
            // No cluster identity configured at all: always Bare, dot or no dot.
            (
                "billing",
                None,
                HostClass::Bare {
                    namespace: "billing",
                },
            ),
            (
                "billing.east",
                None,
                HostClass::Bare {
                    namespace: "billing.east",
                },
            ),
            // Empty cluster string behaves like None (defensive; shouldn't occur post
            // admission-validation, but classify_hostname is total).
            (
                "billing",
                Some(""),
                HostClass::Bare {
                    namespace: "billing",
                },
            ),
            // No dot in the hostname at all.
            (
                "billing",
                Some("east"),
                HostClass::Bare {
                    namespace: "billing",
                },
            ),
            // Exact suffix match => ours.
            (
                "ns.east",
                Some("east"),
                HostClass::OwnCluster { namespace: "ns" },
            ),
            // Different suffix => another cluster's.
            (
                "ns.west",
                Some("east"),
                HostClass::ForeignCluster { suffix: "west" },
            ),
            // Only an EXACT suffix match is "own" — "east.x" != "east".
            (
                "ns.east.x",
                Some("east"),
                HostClass::ForeignCluster { suffix: "east.x" },
            ),
            // Empty namespace part or empty suffix: Bare, whole hostname verbatim.
            (
                ".east",
                Some("east"),
                HostClass::Bare { namespace: ".east" },
            ),
            ("ns.", Some("east"), HostClass::Bare { namespace: "ns." }),
            // Case-sensitive suffix comparison.
            (
                "ns.East",
                Some("east"),
                HostClass::ForeignCluster { suffix: "East" },
            ),
            // Hostname exactly equal to the cluster name, no dot => Bare, never
            // OwnCluster (there is no namespace/cluster split to make).
            ("east", Some("east"), HostClass::Bare { namespace: "east" }),
        ];
        for (hostname, cluster, expected) in cases {
            assert_eq!(
                classify_hostname(hostname, *cluster),
                *expected,
                "classify_hostname({hostname:?}, {cluster:?})"
            );
        }
    }

    #[test]
    fn classify_hostname_round_trips_the_rendered_default() {
        // For any valid namespace + cluster, classifying the hostname
        // resolve_identity's own default would render must come back OwnCluster
        // with the original namespace — the write path and the read path agree.
        for (ns, cluster) in [
            ("billing", "east"),
            ("a", "b"),
            ("kube-system", "prod-1"),
            ("default", "us-west-2"),
        ] {
            let rendered = format!("{ns}.{cluster}");
            assert_eq!(
                classify_hostname(&rendered, Some(cluster)),
                HostClass::OwnCluster { namespace: ns },
                "rendered={rendered:?} cluster={cluster:?}"
            );
        }
    }
}
