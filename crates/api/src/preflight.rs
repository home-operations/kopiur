//! Backup preflight: user-declared CEL preconditions a `Snapshot` must satisfy
//! before its mover Job is launched (the "stronger preflight" of
//! `docs/repository-health.md`).
//!
//! This reuses the `cel`/`*Expr` foundation from [`crate::success_expr`] and
//! [`crate::identity`]: compile → execute → require a typed result, length-capped
//! as the cost-budget surrogate, out-of-scope variables rejected at admission. A
//! preflight check returns a **bool** over a live repository + maintenance
//! environment, evaluated by the controller at reconcile (unlike `successExpr`,
//! which the mover evaluates against a finished verify result).
//!
//! ## CEL environment
//!
//! Two maps, always present:
//!
//! - `repository` — `{ phase, ready, backendReachable, snapshotCount, indexBlobCount,
//!   sizeBytes, lastHealthyKnown, lastHealthyAgeSeconds, lastReverifyKnown,
//!   lastReverifyAgeSeconds }`.
//! - `maintenance` — `{ hasRun, lastSuccessAgeSeconds }`.
//!
//! ## Fail-closed sentinels
//!
//! Every `*AgeSeconds` / count is an integer; "never / unknown" is encoded as
//! [`i64::MAX`] (NOT `-1`). A naive freshness guard
//! `maintenance.lastSuccessAgeSeconds < 604800` then reads `i64::MAX < 604800 ==
//! false` and correctly **blocks** the backup when the value is unknown, instead of
//! silently passing (which a `-1` sentinel would do). The companion `*Known` /
//! `hasRun` booleans let an expression branch explicitly when it prefers to.

use cel::{Context, Program, Value};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{ValidationError, ValidationResult};
use crate::identity::MAX_EXPR_LEN;

/// Sentinel for an unknown/never integer input, so a freshness guard fails closed.
pub const UNKNOWN_AGE: i64 = i64::MAX;

/// `SnapshotPolicy.spec.preflight` — named preconditions a backup run must satisfy
/// before the mover Job is created. Opt-in; absent ⇒ no preflight.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreflightSpec {
    /// Checks that must **all** pass (AND) before the backup launches. An empty
    /// list is an inert no-op (symmetric with `verification` absent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 50))]
    pub checks: Vec<PreflightCheck>,
    /// How long to hold a `Snapshot` in `Pending` while a check is unsatisfied
    /// before failing it (Go-style duration like `10m` or `1h`; default `10m`). A
    /// zero duration (`0`/`0s`) holds indefinitely (never fail on preflight).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// One named precondition: a CEL bool predicate over the preflight environment.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreflightCheck {
    /// Stable identifier surfaced in the `Snapshot`'s status when this check blocks
    /// (e.g. `maintenance-fresh`). Unique within the policy.
    #[schemars(length(min = 1, max = 63))]
    pub name: String,
    /// CEL bool predicate; the backup proceeds only when this evaluates `true`.
    pub expr: String,
    /// Optional human message surfaced alongside the check name when it blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The full environment a preflight check evaluates against. A plain value struct
/// (no `kube`/`tokio`) — the controller gathers live repository + maintenance state
/// and fills it; the evaluator is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightInputs {
    /// `repository.phase` — the `Repository`/`ClusterRepository` status phase label.
    pub repository_phase: String,
    /// `repository.ready` — `phase == Ready`.
    pub repository_ready: bool,
    /// `repository.backendReachable` — the `BackendReachable` condition is `True`,
    /// or `true` when the condition is absent (the health probe is disabled, so
    /// there is no evidence the backend is down).
    pub backend_reachable: bool,
    /// `repository.snapshotCount` — `status.storageStats.snapshotCount`, [`UNKNOWN_AGE`] if unobserved.
    pub snapshot_count: i64,
    /// `repository.indexBlobCount` — `status.storageStats.indexBlobCount`, [`UNKNOWN_AGE`] if unobserved.
    pub index_blob_count: i64,
    /// `repository.sizeBytes` — `status.storageStats.totalSizeBytes`, [`UNKNOWN_AGE`] if unobserved.
    pub size_bytes: i64,
    /// `repository.lastHealthyKnown` — a successful health probe has been recorded.
    pub last_healthy_known: bool,
    /// `repository.lastHealthyAgeSeconds` — secs since `status.health.lastHealthyAt`, [`UNKNOWN_AGE`] if never.
    pub last_healthy_age_seconds: i64,
    /// `repository.lastReverifyKnown` — a reverify has been recorded.
    pub last_reverify_known: bool,
    /// `repository.lastReverifyAgeSeconds` — secs since `status.lastReverifyAt`, [`UNKNOWN_AGE`] if never.
    pub last_reverify_age_seconds: i64,
    /// `maintenance.hasRun` — the repo's `Maintenance` has a recorded successful run
    /// (scheduled or manual run-now).
    pub maintenance_has_run: bool,
    /// `maintenance.lastSuccessAgeSeconds` — secs since the most recent successful
    /// maintenance of any mode, [`UNKNOWN_AGE`] if never.
    pub maintenance_last_success_age_seconds: i64,
}

impl Default for PreflightInputs {
    fn default() -> Self {
        // The fail-closed default: nothing observed yet, so freshness guards block.
        Self {
            repository_phase: String::new(),
            repository_ready: false,
            backend_reachable: false,
            snapshot_count: UNKNOWN_AGE,
            index_blob_count: UNKNOWN_AGE,
            size_bytes: UNKNOWN_AGE,
            last_healthy_known: false,
            last_healthy_age_seconds: UNKNOWN_AGE,
            last_reverify_known: false,
            last_reverify_age_seconds: UNKNOWN_AGE,
            maintenance_has_run: false,
            maintenance_last_success_age_seconds: UNKNOWN_AGE,
        }
    }
}

/// Compile a preflight expression, enforcing the [`MAX_EXPR_LEN`] budget first
/// (shared with identity / `successExpr`). Maps a parse failure to
/// [`ValidationError::PreflightExprCompile`].
fn compile(expr: &str) -> ValidationResult<Program> {
    if expr.len() > MAX_EXPR_LEN {
        return Err(ValidationError::PreflightExprCompile {
            expr: expr.to_string(),
            reason: format!(
                "expression is {} bytes; the maximum is {MAX_EXPR_LEN}",
                expr.len()
            ),
        });
    }
    Program::compile(expr).map_err(|e| ValidationError::PreflightExprCompile {
        expr: expr.to_string(),
        reason: e.to_string(),
    })
}

/// Build the CEL context: the `repository` and `maintenance` maps. Each is a JSON
/// object so int/bool/string values coexist under one variable (mirrors
/// `success_expr::context`'s `restored` object).
fn context<'a>(inputs: &PreflightInputs) -> Context<'a> {
    let mut ctx = Context::default();
    let repository = serde_json::json!({
        "phase": inputs.repository_phase,
        "ready": inputs.repository_ready,
        "backendReachable": inputs.backend_reachable,
        "snapshotCount": inputs.snapshot_count,
        "indexBlobCount": inputs.index_blob_count,
        "sizeBytes": inputs.size_bytes,
        "lastHealthyKnown": inputs.last_healthy_known,
        "lastHealthyAgeSeconds": inputs.last_healthy_age_seconds,
        "lastReverifyKnown": inputs.last_reverify_known,
        "lastReverifyAgeSeconds": inputs.last_reverify_age_seconds,
    });
    let maintenance = serde_json::json!({
        "hasRun": inputs.maintenance_has_run,
        "lastSuccessAgeSeconds": inputs.maintenance_last_success_age_seconds,
    });
    let _ = ctx.add_variable("repository", &repository);
    let _ = ctx.add_variable("maintenance", &maintenance);
    ctx
}

/// Evaluate a preflight expression against `inputs`, requiring a bool result. Maps
/// an evaluation failure to [`ValidationError::PreflightExprEval`] and a non-bool
/// result to [`ValidationError::PreflightExprType`].
pub fn eval_preflight_expr(expr: &str, inputs: &PreflightInputs) -> ValidationResult<bool> {
    let program = compile(expr)?;
    let ctx = context(inputs);
    match program.execute(&ctx) {
        Ok(Value::Bool(b)) => Ok(b),
        Ok(other) => Err(ValidationError::PreflightExprType {
            expr: expr.to_string(),
            got: other.type_of().to_string(),
        }),
        Err(e) => Err(ValidationError::PreflightExprEval {
            expr: expr.to_string(),
            reason: e.to_string(),
        }),
    }
}

/// Validate a preflight expression at admission: it must compile, and — because
/// CEL reports an out-of-scope variable only at evaluation time — it must
/// trial-evaluate against a representative environment without referencing an
/// undeclared variable, returning a **bool**. Missing *map keys* on a
/// data-dependent index are tolerated, mirroring
/// [`crate::success_expr::validate_success_expr`].
pub fn validate_preflight_expr(expr: &str) -> ValidationResult {
    let program = compile(expr)?;
    // A representative non-trivial environment (observed values, not the sentinel)
    // so guards behave during the trial.
    let inputs = PreflightInputs {
        repository_phase: "Ready".to_string(),
        repository_ready: true,
        backend_reachable: true,
        snapshot_count: 1,
        index_blob_count: 1,
        size_bytes: 1,
        last_healthy_known: true,
        last_healthy_age_seconds: 1,
        last_reverify_known: true,
        last_reverify_age_seconds: 1,
        maintenance_has_run: true,
        maintenance_last_success_age_seconds: 1,
    };
    let ctx = context(&inputs);
    match program.execute(&ctx) {
        Ok(Value::Bool(_)) => Ok(()),
        Ok(other) => Err(ValidationError::PreflightExprType {
            expr: expr.to_string(),
            got: other.type_of().to_string(),
        }),
        // An undeclared-variable reference (typo / out-of-scope) is a hard reject;
        // other runtime errors (NoSuchKey on a data-dependent map index) tolerated.
        Err(cel::ExecutionError::UndeclaredReference(name)) => {
            Err(ValidationError::PreflightExprEval {
                expr: expr.to_string(),
                reason: format!("undeclared reference to '{name}'"),
            })
        }
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_inputs() -> PreflightInputs {
        PreflightInputs {
            repository_phase: "Ready".to_string(),
            repository_ready: true,
            backend_reachable: true,
            snapshot_count: 12,
            index_blob_count: 5,
            size_bytes: 4096,
            last_healthy_known: true,
            last_healthy_age_seconds: 60,
            last_reverify_known: true,
            last_reverify_age_seconds: 120,
            maintenance_has_run: true,
            maintenance_last_success_age_seconds: 3600,
        }
    }

    #[test]
    fn evaluates_true_and_false() {
        let i = ready_inputs();
        assert!(eval_preflight_expr("repository.ready", &i).unwrap());
        assert!(eval_preflight_expr("repository.snapshotCount >= 0", &i).unwrap());
        assert!(!eval_preflight_expr("repository.snapshotCount > 1000000", &i).unwrap());
    }

    #[test]
    fn maintenance_freshness_with_guard() {
        let i = ready_inputs(); // last success 1h ago
        let expr = "maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800";
        assert!(eval_preflight_expr(expr, &i).unwrap());
    }

    #[test]
    fn unknown_age_fails_closed_for_freshness() {
        // The headline fail-closed property: a naive `< 7d` freshness check must
        // BLOCK (eval false) when the value was never observed, not silently pass.
        let mut i = ready_inputs();
        i.maintenance_has_run = false;
        i.maintenance_last_success_age_seconds = UNKNOWN_AGE;
        assert!(
            !eval_preflight_expr("maintenance.lastSuccessAgeSeconds < 604800", &i).unwrap(),
            "i64::MAX sentinel must make a naive freshness check fail closed"
        );
        // The explicit guard reads the same way.
        assert!(
            !eval_preflight_expr(
                "maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800",
                &i
            )
            .unwrap()
        );
    }

    #[test]
    fn known_bools_are_available() {
        let mut i = ready_inputs();
        i.last_healthy_known = false;
        i.last_healthy_age_seconds = UNKNOWN_AGE;
        assert!(!eval_preflight_expr("repository.lastHealthyKnown", &i).unwrap());
        assert!(eval_preflight_expr("repository.backendReachable", &i).unwrap());
    }

    #[test]
    fn non_bool_result_is_an_error() {
        let err = eval_preflight_expr("repository.snapshotCount", &ready_inputs()).unwrap_err();
        assert!(matches!(err, ValidationError::PreflightExprType { .. }));
    }

    #[test]
    fn typo_is_an_error() {
        // An undeclared top-level variable (typo) fails at evaluation.
        let err = eval_preflight_expr("repositoryy.ready", &ready_inputs()).unwrap_err();
        assert!(matches!(err, ValidationError::PreflightExprEval { .. }));
    }

    // --- validate_preflight_expr (admission) ---

    #[test]
    fn validate_accepts_valid_bool_exprs() {
        assert!(validate_preflight_expr("repository.ready").is_ok());
        assert!(
            validate_preflight_expr(
                "maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800"
            )
            .is_ok()
        );
        assert!(validate_preflight_expr("repository.sizeBytes >= 0").is_ok());
        assert!(
            validate_preflight_expr("repository.backendReachable && repository.snapshotCount >= 0")
                .is_ok()
        );
    }

    #[test]
    fn validate_rejects_syntax_error() {
        let err = validate_preflight_expr("repository.ready &&").unwrap_err();
        assert!(matches!(err, ValidationError::PreflightExprCompile { .. }));
    }

    #[test]
    fn validate_rejects_out_of_scope_variable() {
        let err = validate_preflight_expr("bogus > 0").unwrap_err();
        assert!(matches!(err, ValidationError::PreflightExprEval { .. }));
    }

    #[test]
    fn validate_rejects_non_bool_result() {
        // A string-valued expression is not a pass/fail predicate.
        let err = validate_preflight_expr("repository.phase").unwrap_err();
        assert!(matches!(err, ValidationError::PreflightExprType { .. }));
    }

    #[test]
    fn validate_rejects_over_length_expr() {
        let long = format!("repository.snapshotCount == {} ", "1".repeat(MAX_EXPR_LEN));
        let err = validate_preflight_expr(&long).unwrap_err();
        assert!(matches!(err, ValidationError::PreflightExprCompile { .. }));
    }
}
