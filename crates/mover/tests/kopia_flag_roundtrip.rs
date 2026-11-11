//! Round trip over every `Modeled` row of the kopia flag table for the two
//! commands a `SnapshotPolicy` drives (#708): setting the field puts the flag on
//! the kopia command line; removing it takes the flag off again.
//!
//! "Off again" means two different things:
//!
//! * `snapshot create` is built fresh every run, so an absent field is an
//!   absent flag (kopia's default).
//! * `policy set` writes a STORED policy. The per-source path scope is replaced
//!   every run (`policy delete`, then `policy set`; #681/#700), so the delete is
//!   what removes a dropped setting — asserted below. The identity scope is never
//!   deleted, so its one user knob (`--max-parallel-snapshots`) is written as
//!   `inherit` when absent.
//!
//! The field path comes from the table row itself (`spec_path`), so a row whose
//! path is wrong fails here, and a new `Modeled` row without a sample value below
//! fails [`every_modeled_row_has_a_round_trip_case`].

use std::collections::BTreeMap;

use kopiur_api::kopia_flags::{FlagDisposition, KopiaCommand};
use kopiur_mover::workspec::{PolicyArgsSpec, RecipeCreateFlags, SnapshotOp};
use serde_json::{Value, json};

const SOURCE: &str = "app@ns:/pvc/app";
const IDENTITY: &str = "app@ns";

/// A sample value for each modeled flag, and the argv value it must produce
/// (`None` for a bare boolean flag).
fn sample(cmd: KopiaCommand, flag: &str) -> Option<(Value, Option<&'static str>)> {
    match (cmd, flag) {
        (KopiaCommand::SnapshotCreate, "upload-limit-mb") => Some((json!(250), Some("250"))),
        (KopiaCommand::SnapshotCreate, "description") => {
            Some((json!("pre-upgrade"), Some("pre-upgrade")))
        }
        (KopiaCommand::SnapshotCreate, "fail-fast") => Some((json!(true), None)),
        (KopiaCommand::SnapshotCreate, "force-hash") => Some((json!(100), Some("100"))),
        (KopiaCommand::SnapshotCreate, "tags") => Some((json!({"app": "db"}), Some("app:db"))),
        (KopiaCommand::PolicySet, "compression") => Some((json!("zstd"), Some("zstd"))),
        (KopiaCommand::PolicySet, "add-never-compress") => Some((json!(["*.mp4"]), Some("*.mp4"))),
        (KopiaCommand::PolicySet, "ignore-file-errors")
        | (KopiaCommand::PolicySet, "ignore-dir-errors")
        | (KopiaCommand::PolicySet, "ignore-unknown-types")
        | (KopiaCommand::PolicySet, "ignore-cache-dirs")
        | (KopiaCommand::PolicySet, "ignore-identical-snapshots") => {
            Some((json!(true), Some("true")))
        }
        (KopiaCommand::PolicySet, "add-ignore") => Some((json!(["*.tmp"]), Some("*.tmp"))),
        (KopiaCommand::PolicySet, "max-parallel-file-reads") => Some((json!(4), Some("4"))),
        (KopiaCommand::PolicySet, "max-parallel-snapshots") => Some((json!(3), Some("3"))),
        _ => None,
    }
}

fn modeled_rows(cmd: KopiaCommand) -> Vec<(&'static str, &'static str)> {
    cmd.flag_tables()
        .iter()
        .flat_map(|t| t.iter())
        .filter_map(|f| match f.disposition {
            FlagDisposition::Modeled { spec_path } => Some((f.name, spec_path)),
            FlagDisposition::OwnedByKopiur { .. }
            | FlagDisposition::Global { .. }
            | FlagDisposition::Rejected { .. }
            | FlagDisposition::Unmodeled { .. } => None,
        })
        .collect()
}

fn base_policy() -> Value {
    json!({
        "repository": { "kind": "Repository", "name": "r" },
        "sources": [ { "pvc": { "name": "app" } } ]
    })
}

/// Set `spec.a.b` (from `Kind:spec.a.b`) on a JSON object, creating parents.
fn set_path(obj: &mut Value, spec_path: &str, value: Value) {
    let path = spec_path.split_once(':').expect("Kind:path").1;
    let mut segs: Vec<&str> = path.split('.').collect();
    assert_eq!(segs.remove(0), "spec", "{spec_path}");
    let (last, parents) = segs.split_last().expect("non-empty path");
    let mut node = obj;
    for seg in parents {
        node = node
            .as_object_mut()
            .expect("object")
            .entry(seg.to_string())
            .or_insert_with(|| json!({}));
    }
    node.as_object_mut()
        .expect("object")
        .insert(last.to_string(), value);
}

/// Whether `argv` carries `--flag` (with `value` as `--flag=value` or the next token).
fn has_flag(argv: &[String], flag: &str, value: Option<&str>) -> bool {
    let bare = format!("--{flag}");
    argv.iter().enumerate().any(|(i, a)| match value {
        None => *a == bare,
        Some(v) => {
            *a == format!("{bare}={v}")
                || (*a == bare && argv.get(i + 1).map(String::as_str) == Some(v))
        }
    })
}

/// The `snapshot create` argv the mover runs for this policy + per-run Snapshot
/// fields — the controller's `SnapshotOp` assembly, then the kopia builder.
fn create_argv(policy: &Value, snapshot: &Value) -> Vec<String> {
    let spec: kopiur_api::SnapshotPolicySpec =
        serde_json::from_value(policy.clone()).expect("policy decodes");
    let recipe = RecipeCreateFlags::from_policy(&spec);
    let tags: BTreeMap<String, String> = snapshot
        .get("tags")
        .map(|t| serde_json::from_value(t.clone()).expect("tags"))
        .unwrap_or_default();
    let op = SnapshotOp {
        source_path: "/data".into(),
        stdin: None,
        tags,
        policy: PolicyArgsSpec::from_policy(&spec),
        fail_fast: recipe.fail_fast,
        upload_limit_mb: recipe.upload_limit_mb,
        description: snapshot
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        force_hash_percent: recipe.force_hash_percent,
        create_extra_args: recipe.extra_args,
    };
    kopiur_kopia::snapshot_create_argv(&op.source_path, &op.tags, &op.create_options())
}

/// The three argvs the mover runs for this policy's kopia policy:
/// path-scope delete, path-scope set (if any flag), identity-scope set.
fn policy_argvs(policy: &Value) -> (Vec<String>, Option<Vec<String>>, Vec<String>) {
    let spec: kopiur_api::SnapshotPolicySpec =
        serde_json::from_value(policy.clone()).expect("policy decodes");
    let plan = PolicyArgsSpec::from_policy(&spec).scope_plan();
    let (delete, set) = kopiur_kopia::policy_replace_argv(SOURCE, &plan.path);
    let identity = kopiur_kopia::policy_set_argv(IDENTITY, &plan.identity);
    (delete, set, identity)
}

#[test]
fn every_modeled_row_has_a_round_trip_case() {
    let mut missing = Vec::new();
    for cmd in [KopiaCommand::SnapshotCreate, KopiaCommand::PolicySet] {
        for (flag, spec_path) in modeled_rows(cmd) {
            if sample(cmd, flag).is_none() {
                missing.push(format!(
                    "`--{flag}` on `{}` ({spec_path}) has no sample in this test. Fix: add one to \
                     `sample` so the field is proven to reach kopia",
                    cmd.display()
                ));
            }
        }
    }
    assert!(missing.is_empty(), "\n{}", missing.join("\n"));
}

#[test]
fn snapshot_create_modeled_fields_set_and_clear_their_flag() {
    for (flag, spec_path) in modeled_rows(KopiaCommand::SnapshotCreate) {
        let (value, argv_value) = sample(KopiaCommand::SnapshotCreate, flag).expect("sample");
        let mut policy = base_policy();
        let mut snapshot = json!({});
        match spec_path.split_once(':').expect("Kind:path").0 {
            "SnapshotPolicy" => set_path(&mut policy, spec_path, value),
            "Snapshot" => set_path(&mut snapshot, spec_path, value),
            other => panic!("`--{flag}`: unexpected kind {other} for snapshot create"),
        }
        let on = create_argv(&policy, &snapshot);
        assert!(
            has_flag(&on, flag, argv_value),
            "`--{flag}` set from {spec_path}: {on:?}"
        );
        let off = create_argv(&base_policy(), &json!({}));
        assert!(
            !off.iter()
                .any(|a| a == &format!("--{flag}") || a.starts_with(&format!("--{flag}="))),
            "`--{flag}` with {spec_path} absent: {off:?}"
        );
    }
}

#[test]
fn policy_set_modeled_fields_set_and_are_replaced_away() {
    for (flag, spec_path) in modeled_rows(KopiaCommand::PolicySet) {
        let (value, argv_value) = sample(KopiaCommand::PolicySet, flag).expect("sample");
        assert!(spec_path.starts_with("SnapshotPolicy:"), "{spec_path}");
        let mut policy = base_policy();
        set_path(&mut policy, spec_path, value);

        let (_, set, identity) = policy_argvs(&policy);
        let on_path = set
            .as_deref()
            .is_some_and(|a| has_flag(a, flag, argv_value));
        let on_identity = has_flag(&identity, flag, argv_value);
        assert!(
            on_path || on_identity,
            "`--{flag}` set from {spec_path}: path {set:?}, identity {identity:?}"
        );

        let (delete, set, identity) = policy_argvs(&base_policy());
        assert_eq!(
            delete,
            vec!["policy", "delete", SOURCE],
            "the path scope is replaced"
        );
        assert!(
            !set.as_deref()
                .is_some_and(|a| has_flag(a, flag, argv_value)),
            "`--{flag}` stays on the path scope after {spec_path} is removed: {set:?}"
        );
        if on_identity {
            // The identity scope is never deleted: a removed value must be reset.
            assert!(
                has_flag(&identity, flag, Some("inherit")),
                "`--{flag}` must be reset to inherit at the identity scope: {identity:?}"
            );
        } else {
            assert!(
                !has_flag(&identity, flag, argv_value),
                "`--{flag}` leaked onto the identity scope: {identity:?}"
            );
        }
    }
}

/// The escape hatches reach their command, after Kopiur's own flags.
#[test]
fn kopia_args_reach_their_command() {
    let mut policy = base_policy();
    policy["kopiaArgs"] = json!({
        "snapshotCreate": ["--log-level=debug"],
        "policySet": ["--max-file-size=10"]
    });
    let create = create_argv(&policy, &json!({}));
    assert_eq!(create.last().map(String::as_str), Some("--log-level=debug"));
    let (_, set, _) = policy_argvs(&policy);
    assert_eq!(
        set.expect("a flag is set").last().map(String::as_str),
        Some("--max-file-size=10")
    );
}
