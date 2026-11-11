//! #708: `spec.upload.forceHashPercent` bounds, the per-command `spec.kopiaArgs`
//! escape hatch, and the admission-only check on the legacy `spec.extraArgs`.

use super::*;
use crate::snapshot_policy::SnapshotPolicySpec;
use crate::testutil::from_yaml;

fn policy(extra: &str) -> SnapshotPolicySpec {
    from_yaml(&format!(
        "repository: {{ kind: Repository, name: r }}\n\
         sources: [ {{ pvc: {{ name: data }} }} ]\n\
         {extra}"
    ))
}

fn joined(errs: &[ValidationError]) -> String {
    errs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn force_hash_percent_accepts_zero_through_one_hundred() {
    for n in [0, 1, 50, 100] {
        let errs = validate_backup_config(&policy(&format!("upload: {{ forceHashPercent: {n} }}")));
        assert!(errs.is_empty(), "{n}: {}", joined(&errs));
    }
}

#[test]
fn force_hash_percent_above_one_hundred_is_refused_with_the_field() {
    let errs = validate_backup_config(&policy("upload: { forceHashPercent: 101 }"));
    let msg = joined(&errs);
    assert!(msg.contains("spec.upload.forceHashPercent"), "{msg}");
    assert!(msg.contains("101"), "{msg}");
}

#[test]
fn absent_force_hash_percent_is_valid() {
    assert!(validate_backup_config(&policy("upload: { limitMb: 5 }")).is_empty());
}

#[test]
fn kopia_args_snapshot_create_refuses_a_modeled_flag_naming_the_field() {
    let errs = validate_backup_config(&policy(
        "kopiaArgs: { snapshotCreate: ['--force-hash=100'] }",
    ));
    let msg = joined(&errs);
    assert_eq!(errs.len(), 1, "{msg}");
    assert!(msg.contains("spec.kopiaArgs.snapshotCreate"), "{msg}");
    assert!(msg.contains("--force-hash=100"), "{msg}");
    assert!(
        msg.contains("SnapshotPolicy spec.upload.forceHashPercent"),
        "{msg}"
    );
}

#[test]
fn kopia_args_refuse_owned_and_rejected_flags_in_both_value_forms() {
    let errs = validate_backup_config(&policy(
        "kopiaArgs:\n\
         \x20 snapshotCreate: ['--json', '--override-source', 'u@h:/p']\n\
         \x20 policySet: ['--keep-daily', '7', '--global']\n",
    ));
    let msg = joined(&errs);
    assert_eq!(errs.len(), 4, "{msg}");
    assert!(msg.contains("--override-source u@h:/p"), "{msg}");
    assert!(msg.contains("--keep-daily 7"), "{msg}");
    assert!(msg.contains("spec.kopiaArgs.policySet"), "{msg}");
}

#[test]
fn kopia_args_accept_unmodeled_and_global_flags() {
    let errs = validate_backup_config(&policy(
        "kopiaArgs:\n\
         \x20 snapshotCreate: ['--log-level=debug', '--send-snapshot-report']\n\
         \x20 policySet: ['--one-file-system=true', '--max-file-size', '1000000']\n",
    ));
    assert!(errs.is_empty(), "{}", joined(&errs));
}

#[test]
fn legacy_extra_args_and_kopia_args_policy_set_are_one_or_the_other() {
    let errs = validate_backup_config(&policy(
        "extraArgs: ['--one-file-system=true']\n\
         kopiaArgs: { policySet: ['--max-file-size=10'] }\n",
    ));
    let msg = joined(&errs);
    assert!(msg.contains("spec.extraArgs"), "{msg}");
    assert!(msg.contains("spec.kopiaArgs.policySet"), "{msg}");
}

/// The legacy field predates the flag table, so stored objects may carry a flag
/// it now refuses: the shared aggregate (which the controller re-runs as a hard
/// stop) must NOT refuse them, or those policies would stop backing up.
#[test]
fn legacy_extra_args_are_not_checked_by_the_shared_aggregate() {
    let spec = policy("extraArgs: ['--keep-latest=3']");
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn legacy_extra_args_admission_refuses_a_new_apply_carrying_a_refused_flag() {
    let spec = policy("extraArgs: ['--keep-latest=3', '--one-file-system=true']");
    let errs = validate_legacy_extra_args(&spec, None);
    let msg = joined(&errs);
    assert_eq!(errs.len(), 1, "{msg}");
    assert!(msg.contains("spec.extraArgs"), "{msg}");
    assert!(msg.contains("--keep-latest=3"), "{msg}");
    assert!(msg.contains("SnapshotPolicy spec.retention"), "{msg}");
}

#[test]
fn legacy_extra_args_admission_tolerates_an_unchanged_stored_list() {
    let spec = policy("extraArgs: ['--keep-latest=3']");
    assert!(validate_legacy_extra_args(&spec, Some(&spec)).is_empty());
    // ...but editing the list re-checks all of it.
    let edited = policy("extraArgs: ['--keep-latest=3', '--max-file-size=10']");
    assert_eq!(validate_legacy_extra_args(&edited, Some(&spec)).len(), 1);
}

#[test]
fn legacy_extra_args_findings_name_each_refused_arg() {
    let spec = policy("extraArgs: ['--compression=zstd', '--max-file-size=10']");
    let found = legacy_extra_args_findings(&spec);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].arg, "--compression=zstd");
    assert!(found[0].message.contains("spec.compression.compressor"));
}
