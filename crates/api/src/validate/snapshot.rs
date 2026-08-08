use super::*;
use crate::common::ScheduleDeletePolicy;
use crate::error::{ValidationError, ValidationResult};
use crate::snapshot::{Origin, SnapshotSpec};
use crate::snapshot_policy::{CopyMethod, Hook, HttpRequestHook, SnapshotPolicySpec};
use crate::snapshot_schedule::SnapshotScheduleSpec;

/// Validate a `SnapshotPolicy` spec, accumulating all problems.
pub fn validate_backup_config(spec: &SnapshotPolicySpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_repository_ref(&spec.repository) {
        errs.push(e);
    }
    if spec.sources.is_empty() {
        errs.push(ValidationError::MissingRequiredField {
            field: "spec.sources (at least one source required)".to_string(),
        });
    }
    for source in &spec.sources {
        if let Err(e) = validate_source(source) {
            errs.push(e);
        }
    }
    // A CSI volume CLONE has no group counterpart — there is no
    // "VolumeGroupClone" — so `copyMethod: Clone` can only ever capture each PVC
    // independently. Since `groupBy` server-side-defaults to
    // `VolumeGroupSnapshot`, an untouched selector policy with `Clone` would
    // silently get N unrelated live-volume clones while every member CR still
    // named a VolumeGroupSnapshot that is never created. Refuse instead: a
    // consistency guarantee that is quietly not honored is worse than none.
    if spec.sources.iter().any(|s| s.pvc_selector.is_some())
        && spec.copy_method == crate::snapshot_policy::CopyMethod::Clone
        && spec.group_by != Some(crate::snapshot_policy::GroupBy::None)
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.groupBy".to_string(),
            reason: "`copyMethod: Clone` clones each PVC independently and has no group \
                     equivalent, so it cannot honor `groupBy: VolumeGroupSnapshot` (the \
                     default). Use `copyMethod: Snapshot` for a consistency group, or set \
                     `groupBy: None` to accept independent clones"
                .to_string(),
        });
    }
    // Identity shape (kopia's username@hostname:path contract). The explicit
    // overrides are validated here — client-free, so this runs on every admission
    // even when the webhook has no kube client. CEL-resolved values and the
    // name/namespace defaults are validated where they are resolved
    // (`resolve_identity`), and again defensively at reconcile time.
    if let Some(id) = &spec.identity {
        if let Some(u) = &id.username
            && let Err(e) = validate_identity_component("spec.identity.username", u)
        {
            errs.push(e);
        }
        if let Some(h) = &id.hostname
            && let Err(e) = validate_identity_component("spec.identity.hostname", h)
        {
            errs.push(e);
        }
    }
    for (i, source) in spec.sources.iter().enumerate() {
        if let Some(p) = &source.source_path_override
            && let Err(e) =
                validate_source_path(&format!("spec.sources[{i}].sourcePathOverride"), p)
        {
            errs.push(e);
        }
    }
    // `copyMethod: Direct` + `readOnly: false` is the one combination that reaches the
    // workload's own volume. `readOnly: false` is only ever set to make `fsGroup` apply,
    // and the kubelet applies it by recursively chgrp-ing the mount and adding
    // group-write. Under Snapshot/Clone that walk rewrites a throwaway staged PVC and is
    // free; under Direct it permanently rewrites live production data while the workload
    // runs — and the mover ships `fsGroup: 65532` by DEFAULT, so a user who sets one bool
    // to fix a permissions error would have their data re-grouped with no other signal.
    // Databases that refuse an over-permissive data directory (postgres, redis) fail to
    // restart afterwards. Require the intent to be stated; it is not inferable.
    for (i, source) in spec.sources.iter().enumerate() {
        if crate::snapshot_policy::source_mutates_live_volume(spec.copy_method, source)
            && !source.acknowledge_live_mutation.unwrap_or(false)
        {
            errs.push(ValidationError::InvalidFieldValue {
                field: format!("spec.sources[{i}].readOnly"),
                reason: "copyMethod: Direct with readOnly: false mounts the LIVE source volume \
                         read-write, so the kubelet will recursively chgrp its contents to the \
                         mover's fsGroup (65532 by default) and make them group-writable — \
                         permanently, while the workload is running. Prefer copyMethod: \
                         Snapshot/Clone, which applies fsGroup to a throwaway staged copy and \
                         never touches your data. If you do mean to rewrite the live volume, \
                         set acknowledgeLiveMutation: true on this source"
                    .to_string(),
            });
        }
    }
    // `volumeSnapshotClassName` only applies when a PVC source is CSI-snapshotted/cloned
    // (`copyMethod: Snapshot`/`Clone`). An NFS source has no PVC to snapshot, so pairing
    // it with an explicit class is a configuration mistake — reject it at admission with
    // an actionable message rather than silently ignoring the class. (`copyMethod` itself
    // can't be rejected for NFS: it defaults to `Snapshot` implicitly and an NFS source
    // is simply read directly.)
    if spec.volume_snapshot_class_name.is_some() && spec.sources.iter().any(|s| s.nfs.is_some()) {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.volumeSnapshotClassName".to_string(),
            reason: "an NFS source cannot be CSI-snapshotted, so volumeSnapshotClassName is \
                     meaningless with it; remove volumeSnapshotClassName (NFS is read directly), \
                     or use a PVC source for copyMethod: Snapshot/Clone"
                .to_string(),
        });
    }
    if let Some(m) = &spec.mover {
        // `inheritSecurityContextFrom.snapshot` replays a backup's RECORDED identity;
        // a backup has no recorded identity to replay — it is the run that records one.
        if let Err(e) = forbid_snapshot_inherit(
            m,
            "snapshotPolicy",
            "a backup mover's identity is read from the live workload \
             (pvcConsumer/workloadSelector), not from a snapshot; `snapshot` is restore-only",
        ) {
            errs.push(e);
        }
        if let Err(e) = validate_mover(m, "SnapshotPolicy mover") {
            errs.push(e);
        }
    }
    // `snapshot create --upload-limit-mb` (M4 flag sweep, issue #216): a count
    // knob, must be at least 1 (0 or negative disables the flag's own purpose).
    if let Some(u) = &spec.upload
        && let Some(mb) = u.limit_mb
        && let Some(e) = require_min("SnapshotPolicy spec.upload.limitMb", mb, 1)
    {
        errs.push(e);
    }
    // Data-loss guard: a retention that selects no snapshots prunes EVERY Snapshot the
    // moment it runs. `retention: None` means "don't prune" (safe) and is not flagged;
    // only an explicit but empty/all-zero retention is the trap.
    if let Some(r) = &spec.retention
        && retention_keeps_nothing(r)
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.retention".to_string(),
            reason: "keeps no snapshots — every keep* bucket is unset or 0, so GFS retention \
                     would prune every Snapshot immediately (data loss). Set at least one bucket \
                     (e.g. keepLatest: 1), or omit spec.retention entirely to disable pruning."
                .to_string(),
        });
    }
    // Verification (ADR-0005 §4): override schedules must parse, and the optional
    // `successExpr` (ADR-0005 §15) must compile + trial-evaluate to a bool with no
    // out-of-scope variable — rejected at admission rather than at first verify run.
    if let Some(v) = &spec.verification {
        if let Some(q) = &v.quick {
            // The flat `verification.quick.cron` shape moved under `quick.schedule`
            // (GitHub #174) — a required schedule would break decode of old persisted
            // objects, so `schedule` is Option and this validator is the gate that
            // rejects a re-apply of the old shape with an actionable pointer.
            match &q.schedule {
                None => errs.push(ValidationError::InvalidFieldValue {
                    field: "spec.verification.quick.schedule".to_string(),
                    reason: "the flat `verification.quick.cron` shape moved to \
                             `verification.quick.schedule.cron` (matching `deep.schedule`). \
                             Move your cron/jitter/timezone fields under `schedule:`."
                        .to_string(),
                }),
                Some(s) => {
                    if let Err(e) = validate_cron(&s.cron) {
                        errs.push(e);
                    }
                    if let Err(e) = validate_timezone(s.timezone.as_deref()) {
                        errs.push(e);
                    }
                    if let Err(e) = validate_jitter(
                        "spec.verification.quick.schedule.jitter",
                        s.jitter.as_deref(),
                    ) {
                        errs.push(e);
                    }
                }
            }
            // `kopia snapshot verify` tuning knobs: counts must be at least 1.
            // `maxErrors` is deliberately unconstrained — 0 is a valid, meaningful
            // value (kopia's own default, "stop at the first error").
            if let Some(p) = q.parallel
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.quick.parallel",
                    p.into(),
                    1,
                )
            {
                errs.push(e);
            }
            if let Some(p) = q.file_parallelism
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.quick.fileParallelism",
                    p.into(),
                    1,
                )
            {
                errs.push(e);
            }
            if let Some(p) = q.file_queue_length
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.quick.fileQueueLength",
                    p.into(),
                    1,
                )
            {
                errs.push(e);
            }
        }
        if let Some(d) = &v.deep {
            if let Err(e) = validate_cron(&d.schedule.cron) {
                errs.push(e);
            }
            if let Err(e) = validate_timezone(d.schedule.timezone.as_deref()) {
                errs.push(e);
            }
            if let Err(e) = validate_jitter(
                "spec.verification.deep.schedule.jitter",
                d.schedule.jitter.as_deref(),
            ) {
                errs.push(e);
            }
            // `restore --parallel` under the hood (deep verify IS a restore).
            if let Some(p) = d.parallel
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.deep.parallel",
                    p.into(),
                    1,
                )
            {
                errs.push(e);
            }
        }
        if let Some(expr) = &v.success_expr
            && let Err(e) = crate::success_expr::validate_success_expr(expr)
        {
            errs.push(e);
        }
    }
    errs.extend(validate_staging(spec));
    // Preflight: the timeout must parse, check names must be unique + non-blank, and
    // each check expression must compile + trial-evaluate to a bool with no
    // out-of-scope variable — rejected at admission rather than at the first run.
    if let Some(pf) = &spec.preflight {
        if let Some(t) = &pf.timeout
            && crate::duration::parse_go_duration(t).is_none()
        {
            errs.push(ValidationError::InvalidFieldValue {
                field: "spec.preflight.timeout".to_string(),
                reason: format!(
                    "{t:?} is not a valid duration. Use a Go-style duration like 10m or 1h; omit \
                     for the default (10m), or 0 to hold indefinitely"
                ),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for (i, c) in pf.checks.iter().enumerate() {
            let name = c.name.trim();
            if name.is_empty() {
                errs.push(ValidationError::MissingRequiredField {
                    field: format!("spec.preflight.checks[{i}].name"),
                });
            } else if !seen.insert(name.to_string()) {
                errs.push(ValidationError::InvalidFieldValue {
                    field: format!("spec.preflight.checks[{i}].name"),
                    reason: format!(
                        "duplicate preflight check name {name:?}; names must be unique"
                    ),
                });
            }
            if let Err(e) = crate::preflight::validate_preflight_expr(&c.expr) {
                errs.push(e);
            }
        }
    }
    // Hooks (ADR §4.8): per-hook shape problems are caught at admission rather
    // than at the first backup run (where a quiesce hook failing on a typo would
    // abort the backup).
    if let Some(h) = &spec.hooks {
        for (list, hooks) in [
            ("beforeSnapshot", &h.before_snapshot),
            ("afterSnapshot", &h.after_snapshot),
        ] {
            for (i, hook) in hooks.iter().enumerate() {
                if let Err(e) = validate_hook(list, i, hook) {
                    errs.push(e);
                }
            }
        }
    }
    errs
}

/// Validate `spec.staging` (+ its interplay with `copyMethod` and the sources):
///
///   * `timeout` must parse — rejected at admission rather than silently falling
///     back to the default at the first backup run.
///   * `accessModes` entries must be canonical/unique, and `ReadWriteOncePod`
///     sole ([`validate_access_modes`]).
///   * The staged-PVC override fields (`storageClassName`/`accessModes`) must have
///     a staged PVC to act on — rejected for `copyMethod: Direct` (no staged PVC
///     at all), an NFS source (never staged), and `pvcSelector` sources (staging
///     is skipped for selector expansion). The pre-existing `timeout` and
///     `volumeSnapshotClassName` stay deliberately lenient in those combinations —
///     tightening them now would reject already-persisted objects on re-apply.
fn validate_staging(spec: &SnapshotPolicySpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let Some(st) = &spec.staging else {
        return errs;
    };
    if let Some(t) = &st.timeout
        && crate::duration::parse_go_duration(t).is_none()
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.staging.timeout".to_string(),
            reason: format!(
                "{t:?} is not a valid duration. Use a Go-style duration like 10m or 1h; omit \
                 for the default (10m), or 0 to wait for the VolumeSnapshot indefinitely"
            ),
        });
    }
    errs.extend(validate_access_modes(
        "spec.staging.accessModes",
        &st.access_modes,
    ));
    // A ReadOnlyMany staged PVC cannot be mounted read-write: the kubelet fails the
    // mount and the backup dies at run time with an opaque error. Catch it here.
    if st.access_modes.contains(&PvcAccessMode::ReadOnlyMany)
        && let Some(i) = spec
            .sources
            .iter()
            .position(|s| !crate::snapshot_policy::source_read_only(s))
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: format!("spec.sources[{i}].readOnly"),
            reason: "readOnly: false cannot be honored when spec.staging.accessModes is \
                     [ReadOnlyMany]: the staged PVC is read-only, so mounting it read-write \
                     fails at the kubelet and the backup never starts. Drop ReadOnlyMany (a \
                     read-write staged PVC is what lets the kubelet apply fsGroup), or drop \
                     readOnly: false. The same conflict exists — invisibly here, because the \
                     class is only resolvable in-cluster — with a read-only staged class such \
                     as a rook-ceph CephFS class with backingSnapshot: \"true\""
                .to_string(),
        });
    }
    let overrides: Vec<&str> = [
        (
            "spec.staging.storageClassName",
            st.storage_class_name.is_some(),
        ),
        ("spec.staging.accessModes", !st.access_modes.is_empty()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();
    if overrides.is_empty() {
        return errs;
    }
    let overrides = overrides.join(" / ");
    match spec.copy_method {
        CopyMethod::Direct => errs.push(ValidationError::InvalidFieldValue {
            field: overrides.clone(),
            reason: "copyMethod: Direct mounts the live source PVC — there is no staged PVC \
                     to override. Remove the staged-PVC override(s), or use copyMethod: \
                     Snapshot/Clone."
                .to_string(),
        }),
        CopyMethod::Snapshot | CopyMethod::Clone => {}
    }
    if spec.sources.iter().any(|s| s.nfs.is_some()) {
        errs.push(ValidationError::InvalidFieldValue {
            field: overrides.clone(),
            reason: "an NFS source is read directly and never staged, so a staged-PVC \
                     override is meaningless with it; remove the override(s) or use a PVC \
                     source for copyMethod: Snapshot/Clone"
                .to_string(),
        });
    }
    // NOTE: `pvcSelector` sources used to be rejected here on the grounds that
    // they "are not CSI-staged". That was true only because the selector was
    // never implemented (#346). Each expanded member is now an ordinary
    // single-PVC Snapshot and stages exactly like one, so the override applies
    // to every member and the rejection is gone.
    let _ = overrides;
    errs
}

/// Validate one hook entry — the controller executes these with the SAME parsers
/// (Go-style `timeout`, URL/method for `httpRequest`), so a value admitted here
/// can never fail to parse at run time. Exhaustive over [`Hook`].
fn validate_hook(list: &str, index: usize, hook: &Hook) -> ValidationResult {
    let field = |leaf: &str| format!("spec.hooks.{list}[{index}].{leaf}");
    let check_timeout = |leaf: &str, t: Option<&str>| -> ValidationResult {
        if let Some(t) = t
            && crate::duration::parse_go_duration(t).is_none()
        {
            return Err(ValidationError::InvalidFieldValue {
                field: field(leaf),
                reason: format!(
                    "{t:?} is not a valid Go-style duration; use a positive number with an \
                     s/m/h suffix (e.g. 90s, 2m) — how long the hook may run before it is \
                     treated as failed"
                ),
            });
        }
        Ok(())
    };
    match hook {
        Hook::WorkloadExec(h) => {
            if h.command.is_empty() {
                return Err(ValidationError::MissingRequiredField {
                    field: field("workloadExec.command"),
                });
            }
            check_timeout("workloadExec.timeout", h.timeout.as_deref())
        }
        Hook::RunJob(h) => check_timeout("runJob.timeout", h.timeout.as_deref()),
        Hook::HttpRequest(h) => {
            if !(h.url.starts_with("http://") || h.url.starts_with("https://")) {
                return Err(ValidationError::InvalidFieldValue {
                    field: field("httpRequest.url"),
                    reason: format!(
                        "{:?} must be an absolute http:// or https:// URL the controller can \
                         reach (e.g. http://notifier.tools.svc:8080/fire)",
                        h.url
                    ),
                });
            }
            if let Some(m) = &h.method {
                const METHODS: [&str; 7] =
                    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
                if !METHODS.contains(&m.to_ascii_uppercase().as_str()) {
                    return Err(ValidationError::InvalidFieldValue {
                        field: field("httpRequest.method"),
                        reason: format!(
                            "{m:?} is not an HTTP method; use one of GET, POST (default), PUT, \
                             PATCH, DELETE, HEAD, OPTIONS"
                        ),
                    });
                }
            }
            if let Some(e) = validate_http_hook_headers(h, list, index) {
                return Err(e);
            }
            check_timeout("httpRequest.timeout", h.timeout.as_deref())
        }
    }
}

/// Largest header name `http::HeaderName::from_bytes` will accept: `http` 1.4.2
/// rejects anything longer via its `MAX_HEADER_NAME_LEN = 65535` guard. Mirrored
/// EXACTLY here so a name admitted at the webhook can never fail to parse at run
/// time (the branch's "anything admitted never fails at runtime" guarantee).
const MAX_HEADER_NAME_LEN: usize = 65_535;

/// RFC 7230 token — exactly the character set `http::HeaderName` accepts. This
/// is the token check only; `http` ALSO caps the byte length at
/// [`MAX_HEADER_NAME_LEN`], enforced separately in [`header_name_error`].
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            matches!(b,
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
                | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        })
}

/// Validate a single header name against everything `http::HeaderName` enforces
/// — the RFC 7230 token set AND the [`MAX_HEADER_NAME_LEN`] byte cap — returning
/// the first problem as a ready-to-return [`ValidationError`]. A length problem
/// is not a token problem, so each carries its own what/why/fix message. Split
/// out of [`validate_http_hook_headers`] to keep that function's branch count
/// (and cognitive complexity) unchanged.
fn header_name_error(name: &str, list: &str, i: usize, j: usize) -> Option<ValidationError> {
    if !is_valid_header_name(name) {
        return Some(ValidationError::InvalidFieldValue {
            field: format!("spec.hooks.{list}[{i}].httpRequest.headers[{j}].name"),
            reason: format!(
                "{name:?} is not a valid HTTP header name — names are case-insensitive \
                 RFC 7230 tokens (letters, digits, and !#$%&'*+-.^_`|~); remove \
                 spaces and other separators"
            ),
        });
    }
    if name.len() > MAX_HEADER_NAME_LEN {
        return Some(ValidationError::InvalidFieldValue {
            field: format!("spec.hooks.{list}[{i}].httpRequest.headers[{j}].name"),
            reason: format!(
                "header name is {} bytes — HTTP header names are limited to \
                 {MAX_HEADER_NAME_LEN} bytes; use a shorter name",
                name.len()
            ),
        });
    }
    None
}

/// Field-content bytes `http::HeaderValue::from_str` accepts: HTAB, or any
/// byte >= 0x20 except DEL (0x7F). Blocks CR/LF header injection.
fn is_valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f))
}

/// True when the URL's authority component carries `user[:pass]@` credentials.
fn url_has_userinfo(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}

/// Validate an `httpRequest` hook's headers so a value admitted here can never
/// fail `http::HeaderName`/`HeaderValue` parsing at run time. Extracted from
/// [`validate_hook`] to keep that match arm below the cognitive-complexity
/// ratchet. Returns the first problem found, matching the arm's early-return
/// style; the field paths mirror the CRD shape
/// (`spec.hooks.{list}[{i}].httpRequest.headers[{j}].{name|value}`).
fn validate_http_hook_headers(
    h: &HttpRequestHook,
    list: &str,
    i: usize,
) -> Option<ValidationError> {
    let mut seen: Vec<String> = Vec::new();
    for (j, header) in h.headers.iter().enumerate() {
        if let Some(e) = header_name_error(&header.name, list, i, j) {
            return Some(e);
        }
        if !is_valid_header_value(&header.value) {
            return Some(ValidationError::InvalidFieldValue {
                field: format!("spec.hooks.{list}[{i}].httpRequest.headers[{j}].value"),
                reason: "control characters (including CR/LF) are not allowed in header \
                         values — put multi-line payloads in `body`, not a header"
                    .into(),
            });
        }
        let lower = header.name.to_ascii_lowercase();
        if seen.contains(&lower) {
            return Some(ValidationError::InvalidFieldValue {
                field: format!("spec.hooks.{list}[{i}].httpRequest.headers[{j}].name"),
                reason: format!(
                    "duplicate header {:?} — each header may be set once; combine values \
                     into a single comma-separated header if the endpoint expects repeats",
                    header.name
                ),
            });
        }
        seen.push(lower);
    }
    if url_has_userinfo(&h.url)
        && h.headers
            .iter()
            .any(|hd| hd.name.eq_ignore_ascii_case("authorization"))
    {
        return Some(ValidationError::InvalidFieldValue {
            field: format!("spec.hooks.{list}[{i}].httpRequest.headers"),
            reason: "an explicit Authorization header conflicts with credentials in the \
                     URL (user:pass@…) — use one auth source, not both"
                .into(),
        });
    }
    None
}

/// Validate a `Snapshot` spec for a given origin, accumulating all problems.
///
/// `origin` is supplied by the caller because the canonical value lives in
/// `status.origin` / the `kopiur.home-operations.com/origin` label, not in
/// `spec` (ADR §3.4). `None` means the row carries an origin marker the caller
/// could not parse (`Origin::parse` returned `None`): the origin-GATED rules
/// (deletionPolicy legality, onScheduleDelete) are **skipped, not failed** —
/// refusing would wedge metadata-only writes (finalizer removal above all) on
/// rows written under a newer operator during version skew, while skipping is
/// safe because the controller's conservative resolution of an unparseable
/// origin (forced `Retain`, never scheduled, never run) makes both gated
/// fields inert at reconcile time. The origin-independent rules still run.
pub fn validate_backup(spec: &SnapshotSpec, origin: Option<Origin>) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Some(origin) = origin {
        if let Err(e) = validate_backup_deletion_policy(origin, spec.deletion_policy) {
            errs.push(e);
        }
        if let Err(e) = validate_backup_on_schedule_delete(origin, spec.on_schedule_delete) {
            errs.push(e);
        }
    }
    if let Some(fp) = &spec.failure_policy
        && let Err(e) = validate_failure_policy(fp, "Snapshot")
    {
        errs.push(e);
    }
    errs.extend(validate_snapshot_tags(spec.tags.as_ref()));
    errs
}

/// At most this many user tags per `Snapshot` — unbounded user tags would
/// inflate every kopia manifest AND the catalog result wire.
pub const MAX_SNAPSHOT_TAGS: usize = 10;
/// Longest admissible user tag key, in bytes.
pub const MAX_SNAPSHOT_TAG_KEY_LEN: usize = 63;
/// Longest admissible user tag value, in bytes.
pub const MAX_SNAPSHOT_TAG_VALUE_LEN: usize = 256;

/// Why one `spec.tags` key/value pair is invalid, or `None` when it is clean.
///
/// One predicate, two callers (the shared-validator pattern): the admission
/// validator ([`validate_snapshot_tags`]) rejects NEW objects with these
/// reasons, and the controller's build path defensively SKIPS (warn, never
/// fail) the same keys on already-stored pre-feature objects.
pub fn snapshot_tag_error(key: &str, value: &str) -> Option<String> {
    if key.is_empty() {
        return Some("tag keys must be non-empty; remove the empty key".to_string());
    }
    if key.contains(':') {
        return Some(format!(
            "tag key {key:?} contains a colon — kopia splits each `--tags` argument on the \
             first colon, so this key would be stored mangled (everything after the colon \
             becomes part of the value) and can collide with kopiur's reserved `kopiur:config` \
             tag, which makes kopia fail the snapshot create with a duplicate-tag error. Use a \
             colon-free key."
        ));
    }
    if key.starts_with("kopiur") {
        return Some(format!(
            "tag key {key:?} uses the reserved `kopiur` prefix — kopiur writes its own tags \
             there (`kopiur:config`, `kopiur-meta`) and a user tag under that prefix would \
             collide with or spoof them. Pick a key that does not start with `kopiur`."
        ));
    }
    if key.len() > MAX_SNAPSHOT_TAG_KEY_LEN {
        return Some(format!(
            "tag key is {} bytes; keys are limited to {MAX_SNAPSHOT_TAG_KEY_LEN} bytes — use a \
             shorter key",
            key.len()
        ));
    }
    if value.len() > MAX_SNAPSHOT_TAG_VALUE_LEN {
        return Some(format!(
            "tag value is {} bytes; values are limited to {MAX_SNAPSHOT_TAG_VALUE_LEN} bytes — \
             every tag is stored on the kopia manifest and read back by every catalog scan, so \
             unbounded values inflate the repository and the scan wire. Use a shorter value.",
            value.len()
        ));
    }
    None
}

/// Validate `Snapshot.spec.tags` (admission): every key/value must pass
/// [`snapshot_tag_error`] and the map is bounded to [`MAX_SNAPSHOT_TAGS`]
/// entries. Accumulates every problem, one error per offending tag.
pub fn validate_snapshot_tags(
    tags: Option<&std::collections::BTreeMap<String, String>>,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let Some(tags) = tags else {
        return errs;
    };
    if tags.len() > MAX_SNAPSHOT_TAGS {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.tags".to_string(),
            reason: format!(
                "{} tags; at most {MAX_SNAPSHOT_TAGS} user tags are allowed per Snapshot — \
                 every tag is stored on the kopia manifest and read back by every catalog \
                 scan. Remove tags until at most {MAX_SNAPSHOT_TAGS} remain.",
                tags.len()
            ),
        });
    }
    for (key, value) in tags {
        if let Some(reason) = snapshot_tag_error(key, value) {
            errs.push(ValidationError::InvalidFieldValue {
                field: format!("spec.tags[{key:?}]"),
                reason,
            });
        }
    }
    errs
}

/// `origin: discovered` Snapshots carry an empty spec; a stamped cascade
/// policy on one is meaningless (their owner is a repository, not a schedule)
/// and forbidden, like a non-Retain deletionPolicy. `origin: adopted` is
/// forbidden for the same reason: an adopted row's owner is the
/// `SnapshotPolicy` it was re-attached to, never a `SnapshotSchedule`. So is
/// `origin: replicated`: a copy CR belongs to its `SnapshotReplication`, and
/// no `SnapshotSchedule` ever fires (or cascades onto) one.
pub fn validate_backup_on_schedule_delete(
    origin: Origin,
    value: Option<ScheduleDeletePolicy>,
) -> ValidationResult {
    match origin {
        Origin::Discovered | Origin::Adopted | Origin::Replicated => match value {
            None => Ok(()),
            Some(v) => Err(ValidationError::DiscoveredCannotSetOnScheduleDelete {
                origin: origin.label_value(),
                got: format!("{v:?}"),
            }),
        },
        Origin::Scheduled | Origin::Manual => Ok(()),
    }
}

/// Exactly one of `policyRef` / `policySelector` is set on a `SnapshotSchedule`
/// (ADR-0005 §10). Neither ⇒ `MissingRequiredField`; both ⇒ `MutuallyExclusive`.
/// Pure so the XOR decision is unit-tested directly.
pub fn validate_schedule_policy_target(spec: &SnapshotScheduleSpec) -> ValidationResult {
    match (spec.policy_ref.is_some(), spec.policy_selector.is_some()) {
        (true, true) => Err(ValidationError::MutuallyExclusive {
            a: "policyRef".to_string(),
            b: "policySelector".to_string(),
            context: "SnapshotSchedule".to_string(),
        }),
        (false, false) => Err(ValidationError::MissingRequiredField {
            field: "exactly one of spec.policyRef or spec.policySelector".to_string(),
        }),
        _ => Ok(()),
    }
}

/// Validate a `SnapshotSchedule` spec, accumulating all problems.
pub fn validate_backup_schedule(spec: &SnapshotScheduleSpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_schedule_policy_target(spec) {
        errs.push(e);
    }
    if let Err(e) = validate_cron(&spec.schedule.cron) {
        errs.push(e);
    }
    if let Err(e) = validate_timezone(spec.schedule.timezone.as_deref()) {
        errs.push(e);
    }
    if let Err(e) = validate_jitter("spec.schedule.jitter", spec.schedule.jitter.as_deref()) {
        errs.push(e);
    }
    errs
}
