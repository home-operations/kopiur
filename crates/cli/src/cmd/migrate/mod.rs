//! `kubectl kopiur migrate volsync` — translate VolSync
//! ReplicationSource/ReplicationDestination objects into kopiur
//! SnapshotPolicy/SnapshotSchedule/Restore (+ optionally a Repository and
//! credential Secrets derived from the repository Secret). Two movers are
//! supported, with very different data semantics:
//!
//! * **restic** (upstream VolSync): config translation ONLY. A restic
//!   repository is NOT a kopia repository: no backup data is migrated, and the
//!   new kopiur repository starts empty. Keep VolSync running until kopiur's
//!   retention coverage suffices.
//! * **kopia** (the `perfectra1n/volsync` fork): the repository IS a kopia
//!   repository and is **adopted in place** — same backend, the existing
//!   Secret's `KOPIA_PASSWORD` referenced (never copied), all snapshots
//!   preserved, and the fork's snapshot identity pinned so history continues.

pub mod io;

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams};
use kube::discovery::ApiResource;

// The pure translation core lives in the `kopiur-migrate` crate. The bare
// `kopia::`/`translate::`/`volsync_types::` paths used throughout this file
// resolve through these aliases, so the call sites stay byte-identical.
use kopiur_migrate::{kopia, translate, volsync_types};

use crate::CmdOutput;
use crate::cli::{GlobalArgs, MigrateVolsyncArgs};
use crate::context::{self, KubeCtx};
use crate::error::{CliError, classify_kube};
use io::{RawVolsync, SecretSource, VolsyncKind};
use translate::{FieldNote, PASSWORD_PLACEHOLDER, Translation};
use volsync_types::{
    DestMoverBlock, MoverBlock, ReplicationDestinationSpec, ReplicationSourceSpec,
};

/// Which mover a translated object came from. Drives banner selection and
/// keys the per-Secret repository dedup (a restic- and a kopia-derived
/// Repository must never silently collide).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MoverKind {
    /// Upstream restic mover.
    Restic,
    /// Fork kopia mover.
    Kopia,
}

/// Dedup key for an emitted Repository: `(mover, namespace, Secret name)`. The
/// namespace matters offline, where one run can span several namespaces and the
/// same Secret name in two namespaces is two different repositories.
type RepoKey = (MoverKind, String, String);

/// Translated objects attributed to one ReplicationSource (so `--out-dir` can
/// write one file per source). `restic`/`kopia` record which mover paragraphs
/// the file's banner needs.
struct Group {
    name: String,
    objects: Vec<serde_json::Value>,
    restic: bool,
    kopia: bool,
}

/// Append `objects` to the group named `name` (creating it if new), recording
/// the mover so the per-file banner is accurate.
fn group_push(
    groups: &mut Vec<Group>,
    name: &str,
    mover: MoverKind,
    objects: impl IntoIterator<Item = serde_json::Value>,
) {
    let group = match groups.iter_mut().find(|g| g.name == name) {
        Some(g) => g,
        None => {
            groups.push(Group {
                name: name.to_string(),
                objects: Vec::new(),
                restic: false,
                kopia: false,
            });
            groups.last_mut().expect("just pushed")
        }
    };
    match mover {
        MoverKind::Restic => group.restic = true,
        MoverKind::Kopia => group.kopia = true,
    }
    group.objects.extend(objects);
}

/// Banner header shared by every run.
const BANNER_HEADER: &str = "\
# ============================================================================
# kopiur migrate volsync";

/// The restic paragraph. Loud on purpose: the most dangerous misunderstanding
/// of the restic path is thinking it moves data.
const BANNER_RESTIC: &str = "\
#
# restic-mover sources: CONFIG TRANSLATION ONLY
#
# A VolSync restic repository is NOT a kopia repository. NO backup data is
# migrated; the kopiur repository referenced below starts EMPTY and fills as
# kopiur takes its own snapshots. Keep VolSync (and its repository) until
# kopiur's retention coverage is sufficient for your recovery needs.";

/// The kopia (fork) paragraph: the opposite story — data preserved, Secret
/// referenced in place, identity continuity.
const BANNER_KOPIA: &str = "\
#
# kopia-mover sources: REPOSITORY ADOPTED IN PLACE
#
# The fork's repository IS a kopia repository: the Repository below connects
# to it as-is and ALL existing snapshots are preserved. Translated policies
# pin the fork's snapshot identity (username@hostname:/data), so history
# continues seamlessly. KEEP the referenced VolSync Secret(s) — kopiur reads
# the password (and matching credentials) from them IN PLACE. kopiur takes
# over repository maintenance on its first run; retire the fork's
# KopiaMaintenance objects and ReplicationSources once kopiur is green.";

/// Banner footer shared by every run.
const BANNER_FOOTER: &str = "\
#
# Review the per-field accounting printed on stderr before applying.
# ============================================================================";

/// Compose the banner from the movers actually seen — mixed runs carry both
/// paragraphs. Pure.
fn compose_banner(saw_restic: bool, saw_kopia: bool) -> String {
    let mut banner = String::from(BANNER_HEADER);
    if saw_restic {
        banner.push('\n');
        banner.push_str(BANNER_RESTIC);
    }
    if saw_kopia {
        banner.push('\n');
        banner.push_str(BANNER_KOPIA);
    }
    banner.push('\n');
    banner.push_str(BANNER_FOOTER);
    banner
}

fn volsync_api(ctx: &KubeCtx, kind: &str) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk("volsync.backube", "v1alpha1", kind);
    let resource = ApiResource::from_gvk(&gvk);
    Api::namespaced_with(ctx.client.clone(), &ctx.namespace, &resource)
}

/// One translated VolSync object, for reporting.
struct Translated {
    source: String,
    translation: Translation,
}

/// Build the Repository + Secrets from a restic repository Secret
/// (`--resolve-secrets`). Returns `(repository_ref, objects)`.
fn emit_repository_objects(
    namespace: &str,
    restic_secret_name: &str,
    restic_secret: &Secret,
) -> Result<(serde_json::Value, Vec<serde_json::Value>, Vec<FieldNote>), CliError> {
    let data = restic_secret.data.clone().unwrap_or_default();
    let get = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(|v| String::from_utf8(v.0.clone()).ok())
    };
    let url = get("RESTIC_REPOSITORY").ok_or_else(|| CliError::MigrationInput {
        what: format!(
            "restic Secret {namespace}/{restic_secret_name} has no RESTIC_REPOSITORY key"
        ),
        fix: "point --repository at an existing kopiur Repository instead, or fix the Secret"
            .into(),
    })?;

    let creds_name = format!("{restic_secret_name}-kopiur-creds");
    let password_name = format!("{restic_secret_name}-kopiur-password");
    let repo_name = format!("{restic_secret_name}-kopiur");

    let (mut backend, scheme) = translate::backend_from_restic_repository(&url, &creds_name)
        .map_err(|reason| CliError::MigrationInput {
            what: reason,
            fix: "author the kopiur Repository by hand and pass it via --repository".into(),
        })?;

    let mut notes = vec![FieldNote {
        field: format!("secret/{restic_secret_name}.RESTIC_REPOSITORY"),
        disposition: translate::Disposition::Mapped {
            to: format!("Repository/{repo_name}.spec.backend"),
        },
    }];

    let mut objects = Vec::new();
    // Carry credentials the way kopiur's MOVER reads them (kopia's env names
    // and backend fields — not restic's names; see translate::cred_plan).
    let mut creds_data = BTreeMap::new();
    for carry in translate::cred_plan(scheme) {
        match carry {
            translate::CredCarry::Env { from, to } => {
                if let Some(value) = get(from) {
                    creds_data.insert(to.to_string(), value);
                    notes.push(FieldNote {
                        field: format!("secret/{restic_secret_name}.{from}"),
                        disposition: translate::Disposition::Mapped {
                            to: format!("Secret/{creds_name}.{to}"),
                        },
                    });
                }
            }
            translate::CredCarry::BackendField { from, pointer } => {
                if let Some(value) = get(from) {
                    if let Some(slot) = backend.pointer_mut(pointer) {
                        *slot = serde_json::json!(value);
                    } else {
                        // Parent object exists (the scheme just built it);
                        // insert the leaf key.
                        let (parent, key) = pointer.rsplit_once('/').expect("pointer has a key");
                        if let Some(obj) =
                            backend.pointer_mut(parent).and_then(|v| v.as_object_mut())
                        {
                            obj.insert(key.to_string(), serde_json::json!(value));
                        }
                    }
                    notes.push(FieldNote {
                        field: format!("secret/{restic_secret_name}.{from}"),
                        disposition: translate::Disposition::Mapped {
                            to: format!("Repository/{repo_name}.spec.backend{pointer}"),
                        },
                    });
                }
            }
        }
    }
    if matches!(scheme, translate::BackendScheme::Gcs) {
        notes.push(FieldNote {
            field: format!("secret/{restic_secret_name}.GOOGLE_APPLICATION_CREDENTIALS"),
            disposition: translate::Disposition::Unmappable {
                reason: format!(
                    "restic stores a file PATH; kopiur needs the service-account JSON CONTENT \
                     under {creds_name}.KOPIA_GCS_CREDENTIALS — add it yourself before applying"
                ),
            },
        });
    }
    if !creds_data.is_empty() {
        objects.push(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": creds_name, "namespace": namespace },
            "stringData": creds_data,
        }));
    }
    // The kopia password CANNOT be the restic password's hash-of-use — and
    // reusing the same passphrase across tools is its own hazard. Emit an
    // explicitly-invalid placeholder; --apply refuses it.
    notes.push(FieldNote {
        field: format!("secret/{restic_secret_name}.RESTIC_PASSWORD"),
        disposition: translate::Disposition::Unmappable {
            reason: format!(
                "a kopia repository needs its OWN new password; {password_name} carries a \
                 REPLACE_ME placeholder you must set before applying"
            ),
        },
    });
    objects.push(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": password_name, "namespace": namespace },
        "stringData": { "KOPIA_PASSWORD": PASSWORD_PLACEHOLDER },
    }));
    objects.push(serde_json::json!({
        "apiVersion": kopiur_api::consts::API_VERSION,
        "kind": "Repository",
        "metadata": { "name": repo_name, "namespace": namespace },
        "spec": {
            "backend": backend,
            "encryption": { "passwordSecretRef": { "name": password_name, "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    }));
    let repo_ref = serde_json::json!({ "kind": "Repository", "name": repo_name });
    Ok((repo_ref, objects, notes))
}

/// Build the Repository (+ optional derived rename-Secret) that ADOPTS a fork
/// kopia repository in place. The existing VolSync Secret is referenced — its
/// `KOPIA_PASSWORD` becomes `encryption.passwordSecretRef` and (where kopia's
/// env names already match) its backend keys serve as `auth.secretRef` — and
/// NO `create` block is emitted: the repository must already exist, so a
/// mis-parsed backend can never silently initialize a fresh empty repo.
/// Returns `(repository_ref, objects, notes)`.
fn emit_kopia_repository_objects(
    namespace: &str,
    secret_name: &str,
    secret: &Secret,
    mover_volumes: Option<&[volsync_types::MoverVolume]>,
) -> Result<(serde_json::Value, Vec<serde_json::Value>, Vec<FieldNote>), CliError> {
    let data: BTreeMap<String, String> = secret
        .data
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| String::from_utf8(v.0).ok().map(|v| (k, v)))
        .collect();
    let url = data
        .get("KOPIA_REPOSITORY")
        .ok_or_else(|| CliError::MigrationInput {
            what: format!("kopia Secret {namespace}/{secret_name} has no KOPIA_REPOSITORY key"),
            fix: "point --repository at an existing kopiur Repository instead, or fix the Secret"
                .into(),
        })?;
    if !data.contains_key("KOPIA_PASSWORD") {
        return Err(CliError::MigrationInput {
            what: format!(
                "kopia Secret {namespace}/{secret_name} has no KOPIA_PASSWORD key; the fork \
                 requires it and kopiur references it in place"
            ),
            fix: "fix the Secret, or pass --repository to skip secret resolution".into(),
        });
    }

    let repo_name = format!("{secret_name}-kopiur");
    let plan = kopia::backend_from_kopia_repository(&kopia::KopiaBackendInput {
        url,
        secret_name,
        repo_name: &repo_name,
        data: &data,
        mover_volumes,
    })
    .map_err(|reason| CliError::MigrationInput {
        what: reason,
        fix: "author the kopiur Repository by hand and pass it via --repository".into(),
    })?;

    let mut notes = vec![
        FieldNote {
            field: format!("secret/{secret_name}.KOPIA_REPOSITORY"),
            disposition: translate::Disposition::Mapped {
                to: format!(
                    "Repository/{repo_name}.spec.backend (adopted in place; no create \
                             block — the repository must already exist)"
                ),
            },
        },
        FieldNote {
            field: format!("secret/{secret_name}.KOPIA_PASSWORD"),
            disposition: translate::Disposition::Mapped {
                to: format!(
                    "Repository/{repo_name}.spec.encryption.passwordSecretRef — referenced IN \
                     PLACE; KEEP Secret {secret_name:?} when decommissioning VolSync"
                ),
            },
        },
    ];
    notes.extend(plan.notes);

    let mut objects = Vec::new();
    if let Some((derived_name, string_data)) = plan.derived_creds {
        objects.push(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": derived_name, "namespace": namespace },
            "stringData": string_data,
        }));
    }
    objects.push(serde_json::json!({
        "apiVersion": kopiur_api::consts::API_VERSION,
        "kind": "Repository",
        "metadata": { "name": repo_name, "namespace": namespace },
        "spec": {
            "backend": plan.backend,
            "encryption": {
                "passwordSecretRef": { "name": secret_name, "key": "KOPIA_PASSWORD" }
            }
        }
    }));
    let repo_ref = serde_json::json!({ "kind": "Repository", "name": repo_name });
    Ok((repo_ref, objects, notes))
}

/// Refuse a Secret referenced by BOTH movers under `--resolve-secrets`: their
/// derived Repositories would share the `<secret>-kopiur` name, and the second
/// server-side apply would silently overwrite the first.
fn check_cross_mover_collision(
    emitted_repos: &BTreeMap<RepoKey, serde_json::Value>,
    mover_kind: MoverKind,
    namespace: &str,
    secret_name: &str,
) -> Result<(), CliError> {
    let other_mover = match mover_kind {
        MoverKind::Restic => MoverKind::Kopia,
        MoverKind::Kopia => MoverKind::Restic,
    };
    if emitted_repos.contains_key(&(other_mover, namespace.to_string(), secret_name.to_string())) {
        return Err(CliError::MigrationInput {
            what: format!(
                "Secret {secret_name:?} is referenced by BOTH a restic and a kopia VolSync \
                 object; their derived Repositories would both be named {secret_name}-kopiur"
            ),
            fix: "migrate the two movers in separate runs using --name, or pass --repository"
                .into(),
        });
    }
    Ok(())
}

/// Render the accounting for stderr. Pure.
fn render_notes(translated: &[Translated]) -> String {
    let mut out = String::new();
    for t in translated {
        out.push_str(&format!("{}:\n", t.source));
        for note in &t.translation.notes {
            let line = match &note.disposition {
                translate::Disposition::Mapped { to } => {
                    format!("  mapped      {} -> {to}\n", note.field)
                }
                translate::Disposition::Unmappable { reason } => {
                    format!("  UNMAPPABLE  {}: {reason}\n", note.field)
                }
                translate::Disposition::Ignored { reason } => {
                    format!("  ignored     {}: {reason}\n", note.field)
                }
            };
            out.push_str(&line);
        }
    }
    out
}

/// Does any emitted object still carry a REPLACE_ME marker? Pure.
fn has_placeholder(objects: &[serde_json::Value]) -> bool {
    objects
        .iter()
        .any(|o| serde_json::to_string(o).is_ok_and(|s| s.contains("REPLACE_ME")))
}

/// Render the emitted objects as a multi-doc YAML stream headed by the banner.
fn render_yaml(objects: &[serde_json::Value], banner: &str) -> Result<String, CliError> {
    let mut out = String::from(banner);
    out.push('\n');
    for obj in objects {
        out.push_str("---\n");
        out.push_str(
            &serde_yaml::to_string(obj).map_err(|e| CliError::Serialization {
                what: "translated manifest",
                source: e.into(),
            })?,
        );
    }
    Ok(out)
}

/// List one VolSync kind from the cluster as namespace-tagged [`RawVolsync`].
async fn list_raw(ctx: &KubeCtx, kind: VolsyncKind) -> Result<Vec<RawVolsync>, CliError> {
    let (kind_str, plural) = match kind {
        VolsyncKind::Source => ("ReplicationSource", "replicationsources"),
        VolsyncKind::Destination => ("ReplicationDestination", "replicationdestinations"),
    };
    let listed = volsync_api(ctx, kind_str)
        .list(&ListParams::default())
        .await
        .map_err(|e| match &e {
            kube::Error::Api(ae) if ae.code == 404 && kind == VolsyncKind::Source => {
                CliError::MigrationInput {
                    what: "the cluster has no volsync.backube/v1alpha1 ReplicationSource resource"
                        .into(),
                    fix:
                        "is VolSync installed on this cluster? Nothing to migrate without its CRDs"
                            .into(),
                }
            }
            _ => classify_kube("list", kind_str, plural, Some(&ctx.namespace), None, e),
        })?;
    Ok(listed
        .items
        .into_iter()
        .map(|o| RawVolsync {
            kind,
            namespace: ctx.namespace.clone(),
            name: o.metadata.name.clone().unwrap_or_default(),
            spec: o
                .data
                .get("spec")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
        .collect())
}

/// Validate flag combinations clap can't express declaratively. Runs before any
/// I/O so a misuse fails fast with an actionable message.
fn validate_args(global: &GlobalArgs, args: &MigrateVolsyncArgs) -> Result<(), CliError> {
    if global.all_namespaces {
        return Err(CliError::AllNamespacesNotApplicable {
            command: "migrate volsync",
        });
    }
    let offline = !args.filename.is_empty();
    let secrets_files = !args.secrets.is_empty();
    // R1: can't apply what you read from disk.
    if offline && args.apply {
        return Err(CliError::MigrationInput {
            what: "--apply cannot be combined with offline input (-f/--filename)".into(),
            fix: "drop --apply and `kubectl apply -f` the emitted manifests yourself, or run \
                  against the cluster"
                .into(),
        });
    }
    // R2: --out-dir writes files for review; it does not apply.
    if args.out_dir.is_some() && args.apply {
        return Err(CliError::MigrationInput {
            what: "--apply cannot be combined with --out-dir".into(),
            fix: "--out-dir writes manifests to disk for review; drop one of the two".into(),
        });
    }
    // R3: offline --resolve-secrets needs a Secret source.
    if offline && args.resolve_secrets && !secrets_files && !args.from_cluster_secrets {
        return Err(CliError::MigrationInput {
            what: "offline --resolve-secrets has no Secret source".into(),
            fix: "pass --secrets <file/dir> for plaintext Secrets, --from-cluster-secrets to \
                  fetch them live, or --repository to skip secret resolution"
                .into(),
        });
    }
    // R4: secret-source flags are meaningless without offline input.
    if !offline && (secrets_files || args.from_cluster_secrets) {
        return Err(CliError::MigrationInput {
            what: "--secrets/--from-cluster-secrets only apply to offline input".into(),
            fix: "add -f/--filename to read VolSync objects from disk, or drop these flags \
                  (a cluster --resolve-secrets run already reads Secrets live)"
                .into(),
        });
    }
    // R6: stdin can only be read once.
    let uses_stdin = |v: &[std::path::PathBuf]| {
        v.iter()
            .any(|p| p.as_os_str() == crate::consts::STDIN_TOKEN)
    };
    if uses_stdin(&args.filename) && uses_stdin(&args.secrets) {
        return Err(CliError::MigrationInput {
            what: "stdin (-) was given for both -f and --secrets".into(),
            fix: "stdin can only be read once; put one of the two in a file".into(),
        });
    }
    Ok(())
}

/// Run `migrate volsync`.
pub async fn run(global: &GlobalArgs, args: &MigrateVolsyncArgs) -> Result<CmdOutput, CliError> {
    validate_args(global, args)?;
    let offline = !args.filename.is_empty();

    // Connect only when a cluster is genuinely needed: cluster input, or the
    // offline hybrid that reads Secrets live. (Offline --apply is rejected, so
    // it never forces a connection.)
    let needs_client = !offline || (args.resolve_secrets && args.from_cluster_secrets);
    let ctx = if needs_client {
        Some(context::connect(global).await?)
    } else {
        None
    };

    // Namespace for objects that don't carry their own (file input) and for
    // cluster scoping.
    let fallback_ns = match (&ctx, offline) {
        (Some(c), false) => c.namespace.clone(),
        _ => global
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
    };

    // --- gather VolSync objects (cluster or files), in a deterministic order ---
    let mut all: Vec<RawVolsync> = if offline {
        io::parse_volsync(&args.filename, &fallback_ns)?
    } else {
        let ctx = ctx.as_ref().expect("client present for cluster input");
        let mut v = list_raw(ctx, VolsyncKind::Source).await?;
        if args.include_destinations {
            v.extend(list_raw(ctx, VolsyncKind::Destination).await?);
        }
        v
    };
    all.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    let mut sources: Vec<RawVolsync> = all
        .iter()
        .filter(|o| o.kind == VolsyncKind::Source)
        .cloned()
        .collect();
    if let Some(name) = &args.name {
        sources.retain(|o| o.name == *name);
        if sources.is_empty() {
            return Err(CliError::NotFound {
                kind: "ReplicationSource",
                plural: "replicationsources",
                name: name.clone(),
                scope: crate::error::scope_suffix(Some(&fallback_ns)),
                scope_flag: if offline {
                    String::new()
                } else {
                    format!(" -n {fallback_ns}")
                },
            });
        }
    }
    if sources.is_empty() {
        let location = if offline {
            "the supplied files".to_string()
        } else {
            format!("namespace {fallback_ns}")
        };
        return Ok(CmdOutput::ok(format!(
            "no ReplicationSources found in {location}; nothing to migrate\n"
        )));
    }
    let destinations: Vec<RawVolsync> = if args.include_destinations {
        all.iter()
            .filter(|o| o.kind == VolsyncKind::Destination)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    // --- how repository Secrets resolve ---
    let secret_source = if args.repository.is_some() {
        SecretSource::None
    } else if args.from_cluster_secrets || (!offline && args.resolve_secrets) {
        SecretSource::Cluster {
            client: ctx
                .as_ref()
                .expect("client present for cluster secrets")
                .client
                .clone(),
        }
    } else if offline && args.resolve_secrets {
        SecretSource::Files(io::parse_secrets(&args.secrets, &fallback_ns)?)
    } else {
        // Unreachable: the repo_source group requires --repository or
        // --resolve-secrets, both handled above.
        SecretSource::None
    };

    // Shared (deduped) Repository/Secret objects vs per-source policy/schedule.
    let mut shared: Vec<serde_json::Value> = Vec::new();
    let (mut shared_restic, mut shared_kopia) = (false, false);
    let mut groups: Vec<Group> = Vec::new();
    let mut translated: Vec<Translated> = Vec::new();
    let mut emitted_repos: BTreeMap<RepoKey, serde_json::Value> = BTreeMap::new();
    // policy names per (mover, namespace, Secret), for destination pairing.
    let mut policies_by_secret: BTreeMap<RepoKey, Vec<String>> = BTreeMap::new();
    let (mut saw_restic, mut saw_kopia) = (false, false);

    for source in &sources {
        let name = source.name.clone();
        let ns = source.namespace.clone();
        let spec: ReplicationSourceSpec =
            serde_json::from_value(source.spec.clone()).map_err(|e| CliError::MigrationInput {
                what: format!("ReplicationSource {ns}/{name}: cannot decode spec: {e}"),
                fix: "check the object; only volsync.backube/v1alpha1 shapes are supported".into(),
            })?;

        let mover = spec.mover().map_err(|what| CliError::MigrationInput {
            what: format!("ReplicationSource {ns}/{name} {what}"),
            fix: "only restic- and (fork) kopia-mover ReplicationSources are translatable".into(),
        })?;
        let (mover_kind, secret_name) = match &mover {
            MoverBlock::Restic(r) => (MoverKind::Restic, r.repository.clone().unwrap_or_default()),
            MoverBlock::Kopia(k) => (MoverKind::Kopia, k.repository.clone().unwrap_or_default()),
        };
        // Empty mover repository under --resolve-secrets would later try to GET a
        // Secret with an empty name; name the offending source instead.
        if args.repository.is_none() && secret_name.is_empty() {
            let field = match mover_kind {
                MoverKind::Restic => "spec.restic.repository",
                MoverKind::Kopia => "spec.kopia.repository",
            };
            return Err(CliError::MigrationInput {
                what: format!(
                    "ReplicationSource {ns}/{name} uses --resolve-secrets but its {field} field \
                     is empty/missing"
                ),
                fix: "set the mover's repository to the repository Secret name, or pass \
                      --repository to point at an existing kopiur Repository"
                    .into(),
            });
        }
        match mover_kind {
            MoverKind::Restic => saw_restic = true,
            MoverKind::Kopia => saw_kopia = true,
        }

        // Which kopiur repository the policy points at.
        let repo_ref = if let Some(existing) = &args.repository {
            let kind: kopiur_api::common::RepositoryKind = args.repository_kind.into();
            serde_json::json!({ "kind": format!("{kind:?}"), "name": existing })
        } else {
            // --resolve-secrets: derive (and emit, once per Secret).
            let key: RepoKey = (mover_kind, ns.clone(), secret_name.clone());
            match emitted_repos.get(&key) {
                Some(r) => r.clone(),
                None => {
                    check_cross_mover_collision(&emitted_repos, mover_kind, &ns, &secret_name)?;
                    let secret = secret_source.get(&ns, &secret_name).await?.ok_or_else(|| {
                        CliError::MigrationInput {
                            what: format!(
                                "ReplicationSource {ns}/{name} references repository Secret \
                                 {secret_name:?}, which does not exist"
                            ),
                            fix: if offline && !args.from_cluster_secrets {
                                "supply it via --secrets, or pass --repository to skip secret \
                                 resolution"
                                    .into()
                            } else {
                                "fix the VolSync object or pass --repository to skip secret \
                                 resolution"
                                    .into()
                            },
                        }
                    })?;
                    let (repo_ref, repo_objects, notes, has_unmappable) = match &mover {
                        MoverBlock::Restic(_) => {
                            let (r, o, n) = emit_repository_objects(&ns, &secret_name, &secret)?;
                            shared_restic = true;
                            (r, o, n, true) // the password placeholder
                        }
                        MoverBlock::Kopia(k) => {
                            let (r, o, n) = emit_kopia_repository_objects(
                                &ns,
                                &secret_name,
                                &secret,
                                k.mover_volumes.as_deref(),
                            )?;
                            shared_kopia = true;
                            let unmappable = n.iter().any(|note| {
                                matches!(
                                    note.disposition,
                                    translate::Disposition::Unmappable { .. }
                                )
                            });
                            (r, o, n, unmappable)
                        }
                    };
                    shared.extend(repo_objects);
                    translated.push(Translated {
                        source: format!("secret/{secret_name}"),
                        translation: Translation {
                            objects: Vec::new(),
                            notes,
                            has_unmappable,
                        },
                    });
                    emitted_repos.insert(key, repo_ref.clone());
                    repo_ref
                }
            }
        };

        let translation =
            match &mover {
                MoverBlock::Restic(_) => translate::translate_source(&name, &ns, &spec, &repo_ref)
                    .map_err(|what| CliError::MigrationInput {
                        what,
                        fix: "only restic-mover ReplicationSources are translatable".into(),
                    })?,
                MoverBlock::Kopia(_) => kopia::translate_source(&name, &ns, &spec, &repo_ref)
                    .map_err(|what| CliError::MigrationInput {
                        what,
                        fix: "check the ReplicationSource's spec.kopia block".into(),
                    })?,
            };
        group_push(
            &mut groups,
            &name,
            mover_kind,
            translation.objects.iter().cloned(),
        );
        policies_by_secret
            .entry((mover_kind, ns.clone(), secret_name))
            .or_default()
            .push(name.clone());
        translated.push(Translated {
            source: format!("replicationsource/{name}"),
            translation,
        });
    }

    // --- destinations (opt-in) ---
    for dest in &destinations {
        let name = dest.name.clone();
        let ns = dest.namespace.clone();
        let spec: ReplicationDestinationSpec =
            serde_json::from_value(dest.spec.clone()).map_err(|e| CliError::MigrationInput {
                what: format!("ReplicationDestination {ns}/{name}: cannot decode spec: {e}"),
                fix: "check the object".into(),
            })?;
        let mover = spec.mover().map_err(|what| CliError::MigrationInput {
            what: format!("ReplicationDestination {ns}/{name} {what}"),
            fix: "only restic- and (fork) kopia-mover ReplicationDestinations are translatable"
                .into(),
        })?;
        let (mover_kind, dest_secret) = match &mover {
            DestMoverBlock::Restic(r) => {
                (MoverKind::Restic, r.repository.clone().unwrap_or_default())
            }
            DestMoverBlock::Kopia(k) => {
                (MoverKind::Kopia, k.repository.clone().unwrap_or_default())
            }
        };
        match mover_kind {
            MoverKind::Restic => saw_restic = true,
            MoverKind::Kopia => saw_kopia = true,
        }
        // Pair with a source policy via the shared Secret (the fromPolicy
        // fallback; a kopia destination with an explicit identity does not
        // use it).
        let policy = match policies_by_secret
            .get(&(mover_kind, ns.clone(), dest_secret.clone()))
            .map(Vec::as_slice)
        {
            Some([only]) => only.clone(),
            _ => "REPLACE_ME-policy".to_string(),
        };
        let mut translation = match &mover {
            DestMoverBlock::Restic(_) => {
                translate::translate_destination(&name, &ns, &spec, &policy).map_err(|what| {
                    CliError::MigrationInput {
                        what,
                        fix: "only restic-mover ReplicationDestinations are translatable".into(),
                    }
                })?
            }
            DestMoverBlock::Kopia(k) => {
                // An identity-source Restore needs spec.repository; resolve it
                // the same way sources do (emitting the Repository when this
                // Secret was not seen via a source).
                let repo_ref = if let Some(existing) = &args.repository {
                    let kind: kopiur_api::common::RepositoryKind = args.repository_kind.into();
                    serde_json::json!({ "kind": format!("{kind:?}"), "name": existing })
                } else {
                    if dest_secret.is_empty() {
                        return Err(CliError::MigrationInput {
                            what: format!(
                                "ReplicationDestination {ns}/{name} uses --resolve-secrets but its \
                                 spec.kopia.repository field is empty/missing"
                            ),
                            fix: "set spec.kopia.repository, or pass --repository".into(),
                        });
                    }
                    let key: RepoKey = (MoverKind::Kopia, ns.clone(), dest_secret.clone());
                    match emitted_repos.get(&key) {
                        Some(r) => r.clone(),
                        None => {
                            check_cross_mover_collision(
                                &emitted_repos,
                                MoverKind::Kopia,
                                &ns,
                                &dest_secret,
                            )?;
                            let secret = secret_source.get(&ns, &dest_secret).await?.ok_or_else(
                                || CliError::MigrationInput {
                                    what: format!(
                                        "ReplicationDestination {ns}/{name} references repository \
                                         Secret {dest_secret:?}, which does not exist"
                                    ),
                                    fix: "fix the VolSync object or pass --repository".into(),
                                },
                            )?;
                            let (repo_ref, repo_objects, notes) = emit_kopia_repository_objects(
                                &ns,
                                &dest_secret,
                                &secret,
                                k.mover_volumes.as_deref(),
                            )?;
                            shared_kopia = true;
                            let has_unmappable = notes.iter().any(|note| {
                                matches!(
                                    note.disposition,
                                    translate::Disposition::Unmappable { .. }
                                )
                            });
                            shared.extend(repo_objects);
                            translated.push(Translated {
                                source: format!("secret/{dest_secret}"),
                                translation: Translation {
                                    objects: Vec::new(),
                                    notes,
                                    has_unmappable,
                                },
                            });
                            emitted_repos.insert(key, repo_ref.clone());
                            repo_ref
                        }
                    }
                };
                kopia::translate_destination(&name, &ns, &spec, &repo_ref, &policy).map_err(
                    |what| CliError::MigrationInput {
                        what,
                        fix: "check the ReplicationDestination's spec.kopia block".into(),
                    },
                )?
            }
        };
        // The fromPolicy fallback may have used the placeholder; surface it.
        if policy == "REPLACE_ME-policy" && has_placeholder(&translation.objects) {
            translation.has_unmappable = true;
            let field = match mover_kind {
                MoverKind::Restic => "spec.restic.repository",
                MoverKind::Kopia => "spec.kopia.repository",
            };
            translation.notes.push(FieldNote {
                field: field.into(),
                disposition: translate::Disposition::Unmappable {
                    reason: format!(
                        "could not pair Secret {dest_secret:?} with exactly one translated source \
                         policy; edit fromPolicy.name before applying"
                    ),
                },
            });
        }
        // Group the Restore with its paired source's file, or its own file when
        // unpaired.
        let group_name = if policy == "REPLACE_ME-policy" {
            format!("destination-{name}")
        } else {
            policy.clone()
        };
        group_push(
            &mut groups,
            &group_name,
            mover_kind,
            translation.objects.iter().cloned(),
        );
        translated.push(Translated {
            source: format!("replicationdestination/{name}"),
            translation,
        });
    }

    // --- report on stderr ---
    if saw_restic {
        eprintln!(
            "restic sources: CONFIG TRANSLATION ONLY — no backup data is migrated; the kopiur \
             repository starts empty. Keep VolSync until kopiur's retention coverage suffices."
        );
    }
    if saw_kopia {
        eprintln!(
            "kopia sources: REPOSITORY ADOPTED IN PLACE — all existing snapshots are preserved \
             and the snapshot identity is pinned so history continues. KEEP the referenced \
             VolSync Secret(s); retire the fork's KopiaMaintenance objects."
        );
    }
    eprintln!();
    eprint!("{}", render_notes(&translated));

    let any_unmappable = translated.iter().any(|t| t.translation.has_unmappable);
    if args.strict && any_unmappable {
        eprintln!("\n--strict: unmappable fields present (see above); not emitting");
        return Ok(CmdOutput {
            text: String::new(),
            exit: 1,
        });
    }

    // Flat view (shared first, then each source's objects), for --apply and the
    // stdout stream.
    let mut flat: Vec<serde_json::Value> = shared.clone();
    for g in &groups {
        flat.extend(g.objects.iter().cloned());
    }

    if args.apply {
        let ctx = ctx.as_ref().expect("client present for --apply");
        if has_placeholder(&flat) {
            return Err(CliError::MigrationInput {
                what: "the translated manifests still contain REPLACE_ME placeholders \
                       (kopia password and/or an unpaired restore policy)"
                    .into(),
                fix: "run without --apply, edit the placeholders in the emitted YAML, then \
                      `kubectl apply -f` it yourself"
                    .into(),
            });
        }
        let params = PatchParams::apply(crate::consts::FIELD_MANAGER);
        for obj in &flat {
            let kind = obj["kind"].as_str().unwrap_or("?");
            let name = obj["metadata"]["name"].as_str().unwrap_or("?");
            let obj_ns = obj["metadata"]["namespace"]
                .as_str()
                .unwrap_or(&ctx.namespace);
            let gvk = match obj["apiVersion"].as_str().unwrap_or_default() {
                "v1" => GroupVersionKind::gvk("", "v1", kind),
                _ => GroupVersionKind::gvk(kopiur_api::GROUP, kopiur_api::VERSION, kind),
            };
            let resource = ApiResource::from_gvk(&gvk);
            let api: Api<DynamicObject> =
                Api::namespaced_with(ctx.client.clone(), obj_ns, &resource);
            api.patch(name, &params, &Patch::Apply(obj.clone()))
                .await
                .map_err(|e| {
                    classify_kube(
                        "patch",
                        "translated object",
                        "objects",
                        Some(obj_ns),
                        Some(name),
                        e,
                    )
                })?;
            eprintln!("applied {kind}/{name}");
        }
    }

    // --- emit: one file per source (+ _shared.yaml), or the stdout stream ---
    if let Some(dir) = &args.out_dir {
        let mut files: Vec<(String, String)> = Vec::new();
        if !shared.is_empty() {
            files.push((
                crate::consts::MIGRATE_SHARED_FILE.to_string(),
                render_yaml(&shared, &compose_banner(shared_restic, shared_kopia))?,
            ));
        }
        for g in &groups {
            if g.objects.is_empty() {
                continue;
            }
            files.push((
                format!("{}.yaml", g.name),
                render_yaml(&g.objects, &compose_banner(g.restic, g.kopia))?,
            ));
        }
        io::write_files(dir, &files, args.force).await?;
        let mut summary = format!("wrote {} file(s) to {}:\n", files.len(), dir.display());
        for (name, _) in &files {
            summary.push_str(&format!("  {name}\n"));
        }
        return Ok(CmdOutput::ok(summary));
    }

    Ok(CmdOutput::ok(render_yaml(
        &flat,
        &compose_banner(saw_restic, saw_kopia),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_scan_catches_password_and_policy_markers() {
        let clean = vec![serde_json::json!({ "kind": "Secret", "stringData": { "k": "v" } })];
        assert!(!has_placeholder(&clean));
        let with_password = vec![serde_json::json!({
            "stringData": { "KOPIA_PASSWORD": PASSWORD_PLACEHOLDER }
        })];
        assert!(has_placeholder(&with_password));
        let with_policy = vec![serde_json::json!({
            "spec": { "source": { "fromPolicy": { "name": "REPLACE_ME-policy" } } }
        })];
        assert!(has_placeholder(&with_policy));
    }

    #[test]
    fn restic_banner_is_loud_about_config_only() {
        let yaml = render_yaml(
            &[serde_json::json!({ "kind": "SnapshotPolicy" })],
            &compose_banner(true, false),
        )
        .unwrap();
        assert!(yaml.starts_with("# ===="), "{yaml}");
        assert!(yaml.contains("CONFIG TRANSLATION ONLY"), "{yaml}");
        assert!(yaml.contains("NO backup data is"), "{yaml}");
        assert!(!yaml.contains("ADOPTED IN PLACE"), "{yaml}");
        assert!(yaml.contains("---\nkind: SnapshotPolicy"), "{yaml}");
    }

    #[test]
    fn kopia_banner_says_adopted_not_empty() {
        let banner = compose_banner(false, true);
        assert!(banner.contains("REPOSITORY ADOPTED IN PLACE"), "{banner}");
        assert!(
            banner.contains("KEEP the referenced VolSync Secret"),
            "{banner}"
        );
        assert!(banner.contains("KopiaMaintenance"), "{banner}");
        assert!(!banner.contains("starts EMPTY"), "{banner}");
    }

    #[test]
    fn mixed_banner_carries_both_paragraphs() {
        let banner = compose_banner(true, true);
        assert!(banner.contains("CONFIG TRANSLATION ONLY"), "{banner}");
        assert!(banner.contains("REPOSITORY ADOPTED IN PLACE"), "{banner}");
    }

    #[test]
    fn emit_repository_objects_builds_repo_and_secrets_with_placeholder_password() {
        let secret: Secret = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "restic-cfg", "namespace": "media" },
            "data": {
                "RESTIC_REPOSITORY": base64("s3:https://minio.local:9000/bucket/app"),
                "RESTIC_PASSWORD": base64("old-restic-pass"),
                "AWS_ACCESS_KEY_ID": base64("AK"),
                "AWS_SECRET_ACCESS_KEY": base64("SK")
            }
        }))
        .unwrap();
        let (repo_ref, objects, notes) =
            emit_repository_objects("media", "restic-cfg", &secret).unwrap();
        assert_eq!(repo_ref["name"], "restic-cfg-kopiur");
        // creds secret carries AWS keys but NEVER the restic password.
        let creds = objects
            .iter()
            .find(|o| o["metadata"]["name"] == "restic-cfg-kopiur-creds")
            .expect("creds secret");
        assert_eq!(creds["stringData"]["AWS_ACCESS_KEY_ID"], "AK");
        assert!(creds["stringData"].get("RESTIC_PASSWORD").is_none());
        // password secret is an explicit placeholder.
        let password = objects
            .iter()
            .find(|o| o["metadata"]["name"] == "restic-cfg-kopiur-password")
            .expect("password secret");
        assert_eq!(
            password["stringData"]["KOPIA_PASSWORD"],
            PASSWORD_PLACEHOLDER
        );
        // repository points at the parsed backend.
        let repo = objects
            .iter()
            .find(|o| o["kind"] == "Repository")
            .expect("repository");
        assert_eq!(repo["spec"]["backend"]["s3"]["bucket"], "bucket");
        assert_eq!(repo["spec"]["backend"]["s3"]["prefix"], "app/");
        // …and the accounting flags the password as unmappable.
        assert!(notes.iter().any(|n| matches!(
            &n.disposition,
            translate::Disposition::Unmappable { reason } if reason.contains("REPLACE_ME")
        )));
        // The emitted Repository spec parses as the REAL kopiur type.
        let _typed: kopiur_api::RepositorySpec =
            serde_json::from_value(repo["spec"].clone()).expect("valid RepositorySpec");
    }

    #[test]
    fn emit_kopia_repository_objects_adopts_in_place_with_no_create_and_no_placeholder() {
        let secret: Secret = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "vs-kopia", "namespace": "media" },
            "data": {
                "KOPIA_REPOSITORY": base64("s3://bucket/app"),
                "KOPIA_PASSWORD": base64("the-real-password"),
                "AWS_ACCESS_KEY_ID": base64("AK"),
                "AWS_SECRET_ACCESS_KEY": base64("SK")
            }
        }))
        .unwrap();
        let (repo_ref, objects, notes) =
            emit_kopia_repository_objects("media", "vs-kopia", &secret, None).unwrap();
        assert_eq!(repo_ref["name"], "vs-kopia-kopiur");

        // S3 references the EXISTING Secret in place: exactly one object (the
        // Repository), no derived Secret, and the password VALUE never leaves
        // the cluster — only a reference to it.
        assert_eq!(objects.len(), 1, "{objects:?}");
        let repo = &objects[0];
        assert_eq!(repo["kind"], "Repository");
        assert_eq!(
            repo["spec"]["encryption"]["passwordSecretRef"]["name"],
            "vs-kopia"
        );
        assert_eq!(
            repo["spec"]["encryption"]["passwordSecretRef"]["key"],
            "KOPIA_PASSWORD"
        );
        assert!(
            !serde_json::to_string(repo)
                .unwrap()
                .contains("the-real-password"),
            "the password value must never be copied into emitted YAML"
        );
        // Adoption semantics: NO create block — the repository must already
        // exist, so a mis-parsed backend can never initialize a fresh repo.
        assert!(repo["spec"].get("create").is_none());
        assert_eq!(
            repo["spec"]["backend"]["s3"]["auth"]["secretRef"]["name"],
            "vs-kopia"
        );
        // Unlike the restic path there is NO password placeholder: a pure-kopia
        // run is --apply-able immediately.
        assert!(!has_placeholder(&objects));
        // The accounting tells the user to KEEP the referenced Secret.
        assert!(notes.iter().any(|n| matches!(
            &n.disposition,
            translate::Disposition::Mapped { to } if to.contains("KEEP Secret")
        )));
        // The emitted Repository spec parses as the REAL kopiur type.
        let typed: kopiur_api::RepositorySpec =
            serde_json::from_value(repo["spec"].clone()).expect("valid RepositorySpec");
        assert!(typed.create.is_none());
    }

    #[test]
    fn emit_kopia_repository_objects_emits_the_rename_secret_for_b2() {
        let secret: Secret = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "vs-b2", "namespace": "media" },
            "data": {
                "KOPIA_REPOSITORY": base64("b2://bkt"),
                "KOPIA_PASSWORD": base64("pw"),
                "B2_ACCOUNT_ID": base64("id"),
                "B2_APPLICATION_KEY": base64("key")
            }
        }))
        .unwrap();
        let (_, objects, _) =
            emit_kopia_repository_objects("media", "vs-b2", &secret, None).unwrap();
        // kopia reads B2_KEY_ID/B2_KEY — a derived rename-Secret carries them.
        let creds = objects
            .iter()
            .find(|o| o["metadata"]["name"] == "vs-b2-kopiur-creds")
            .expect("derived rename secret");
        assert_eq!(creds["stringData"]["B2_KEY_ID"], "id");
        assert_eq!(creds["stringData"]["B2_KEY"], "key");
        let repo = objects
            .iter()
            .find(|o| o["kind"] == "Repository")
            .expect("repo");
        assert_eq!(
            repo["spec"]["backend"]["b2"]["auth"]["secretRef"]["name"],
            "vs-b2-kopiur-creds"
        );
        // The password still references the ORIGINAL Secret.
        assert_eq!(
            repo["spec"]["encryption"]["passwordSecretRef"]["name"],
            "vs-b2"
        );
    }

    #[test]
    fn cross_mover_secret_collision_is_refused_for_sources_and_destinations() {
        // One Secret claimed by the OTHER mover ⇒ the derived `<secret>-kopiur`
        // Repository names would collide; both the source loop and the kopia
        // destination loop must refuse via this shared check.
        let mut emitted: BTreeMap<RepoKey, serde_json::Value> = BTreeMap::new();
        emitted.insert(
            (MoverKind::Restic, "media".to_string(), "shared".to_string()),
            serde_json::json!({}),
        );
        let err =
            check_cross_mover_collision(&emitted, MoverKind::Kopia, "media", "shared").unwrap_err();
        assert!(
            err.to_string().contains("BOTH a restic and a kopia"),
            "{err}"
        );
        // Same mover re-using the Secret is fine (that is the dedup hit path).
        assert!(
            check_cross_mover_collision(&emitted, MoverKind::Restic, "media", "shared").is_ok()
        );
        assert!(check_cross_mover_collision(&emitted, MoverKind::Kopia, "media", "other").is_ok());
        // A same-named Secret in a DIFFERENT namespace is a different repo.
        assert!(check_cross_mover_collision(&emitted, MoverKind::Kopia, "db", "shared").is_ok());
    }

    #[test]
    fn emit_kopia_repository_objects_requires_both_wellknown_keys() {
        let no_url: Secret = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "s", "namespace": "ns" },
            "data": { "KOPIA_PASSWORD": base64("pw") }
        }))
        .unwrap();
        let err = emit_kopia_repository_objects("ns", "s", &no_url, None).unwrap_err();
        assert!(err.to_string().contains("KOPIA_REPOSITORY"), "{err}");

        let no_password: Secret = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "s", "namespace": "ns" },
            "data": { "KOPIA_REPOSITORY": base64("s3://b") }
        }))
        .unwrap();
        let err = emit_kopia_repository_objects("ns", "s", &no_password, None).unwrap_err();
        assert!(err.to_string().contains("KOPIA_PASSWORD"), "{err}");
    }

    // --- offline / GitOps mode (end-to-end through run(), no cluster) ---

    /// Parse a full CLI invocation into the global + migrate args run() takes.
    fn parse(argv: &[&str]) -> (GlobalArgs, MigrateVolsyncArgs) {
        use clap::Parser;
        let cli = crate::Cli::parse_from(argv);
        match cli.command {
            crate::cli::Command::Migrate(crate::cli::MigrateCommand::Volsync(a)) => (cli.global, a),
            _ => panic!("expected migrate volsync"),
        }
    }

    /// A throwaway directory unique to `label`, recreated empty.
    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kopiur-migrate-test-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const KOPIA_SOURCE: &str = "\
apiVersion: volsync.backube/v1alpha1
kind: ReplicationSource
metadata: { name: app, namespace: media }
spec:
  sourcePVC: app-data
  trigger: { schedule: \"0 * * * *\" }
  kopia:
    repository: vs-kopia
";
    const KOPIA_SECRET: &str = "\
apiVersion: v1
kind: Secret
metadata: { name: vs-kopia, namespace: media }
stringData:
  KOPIA_REPOSITORY: s3://bucket/app
  KOPIA_PASSWORD: the-real-password
  AWS_ACCESS_KEY_ID: AK
  AWS_SECRET_ACCESS_KEY: SK
";
    const RESTIC_SOURCE: &str = "\
apiVersion: volsync.backube/v1alpha1
kind: ReplicationSource
metadata: { name: app, namespace: media }
spec:
  sourcePVC: app-data
  trigger: { schedule: \"0 * * * *\" }
  restic:
    repository: vs-restic
";
    const RESTIC_SECRET: &str = "\
apiVersion: v1
kind: Secret
metadata: { name: vs-restic, namespace: media }
stringData:
  RESTIC_REPOSITORY: s3:https://minio.local:9000/bucket/app
  RESTIC_PASSWORD: old-restic-pass
  AWS_ACCESS_KEY_ID: AK
  AWS_SECRET_ACCESS_KEY: SK
";

    fn write(dir: &std::path::Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    #[tokio::test]
    async fn offline_mode_2a_repository_points_policy_at_existing_repo_with_no_secrets() {
        let dir = scratch("2a");
        let src = write(&dir, "src.yaml", KOPIA_SOURCE);
        let (global, args) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            &src,
            "--repository",
            "nas",
        ]);
        let out = run(&global, &args).await.unwrap();
        // No Repository object emitted (its tell is passwordSecretRef); the
        // policy points at the named repo.
        assert!(!out.text.contains("passwordSecretRef"), "{}", out.text);
        assert!(out.text.contains("kind: SnapshotPolicy"), "{}", out.text);
        assert!(out.text.contains("name: nas"), "{}", out.text);
    }

    #[tokio::test]
    async fn offline_mode_2b_restic_resolves_secret_files_with_placeholder_password() {
        let dir = scratch("2b-restic");
        let src = write(&dir, "src.yaml", RESTIC_SOURCE);
        let sec = write(&dir, "sec.yaml", RESTIC_SECRET);
        let out_dir = dir.join("out");
        let (global, args) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            &src,
            "--resolve-secrets",
            "--secrets",
            &sec,
            "--out-dir",
            out_dir.to_str().unwrap(),
        ]);
        let out = run(&global, &args).await.unwrap();
        assert!(out.text.contains("wrote"), "{}", out.text);
        // _shared.yaml carries the Repository + creds + password placeholder.
        let shared = std::fs::read_to_string(out_dir.join("_shared.yaml")).unwrap();
        assert!(shared.contains("kind: Repository"), "{shared}");
        assert!(shared.contains("REPLACE_ME"), "{shared}");
        assert!(shared.contains("AWS_ACCESS_KEY_ID"), "{shared}");
        // app.yaml carries the policy/schedule, not the Repository (whose tell
        // is passwordSecretRef).
        let app = std::fs::read_to_string(out_dir.join("app.yaml")).unwrap();
        assert!(app.contains("kind: SnapshotPolicy"), "{app}");
        assert!(!app.contains("passwordSecretRef"), "{app}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offline_mode_2b_kopia_adopts_in_place_no_placeholder() {
        let dir = scratch("2b-kopia");
        let src = write(&dir, "src.yaml", KOPIA_SOURCE);
        let sec = write(&dir, "sec.yaml", KOPIA_SECRET);
        let (global, args) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            &src,
            "--resolve-secrets",
            "--secrets",
            &sec,
        ]);
        let out = run(&global, &args).await.unwrap();
        // Adopted in place: password referenced, never copied; no placeholder.
        assert!(out.text.contains("kind: Repository"), "{}", out.text);
        assert!(!out.text.contains("REPLACE_ME"), "{}", out.text);
        assert!(!out.text.contains("the-real-password"), "{}", out.text);
        assert!(out.text.contains("passwordSecretRef"), "{}", out.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offline_shared_repository_is_deduped_across_two_sources() {
        let dir = scratch("dedup");
        // Two sources sharing one Secret.
        let two = "\
apiVersion: volsync.backube/v1alpha1
kind: ReplicationSource
metadata: { name: app-a, namespace: media }
spec:
  sourcePVC: a-data
  kopia: { repository: vs-kopia }
---
apiVersion: volsync.backube/v1alpha1
kind: ReplicationSource
metadata: { name: app-b, namespace: media }
spec:
  sourcePVC: b-data
  kopia: { repository: vs-kopia }
";
        let src = write(&dir, "src.yaml", two);
        let sec = write(&dir, "sec.yaml", KOPIA_SECRET);
        let out_dir = dir.join("out");
        let (global, args) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            &src,
            "--resolve-secrets",
            "--secrets",
            &sec,
            "--out-dir",
            out_dir.to_str().unwrap(),
        ]);
        run(&global, &args).await.unwrap();
        // Exactly one Repository, in _shared.yaml; one file per source.
        let shared = std::fs::read_to_string(out_dir.join("_shared.yaml")).unwrap();
        assert_eq!(shared.matches("kind: Repository").count(), 1, "{shared}");
        assert!(out_dir.join("app-a.yaml").exists());
        assert!(out_dir.join("app-b.yaml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_repository_field_names_the_offending_source() {
        let dir = scratch("emptyname");
        let bad = "\
apiVersion: volsync.backube/v1alpha1
kind: ReplicationSource
metadata: { name: app, namespace: media }
spec:
  sourcePVC: app-data
  kopia: {}
";
        let src = write(&dir, "src.yaml", bad);
        let sec = write(&dir, "sec.yaml", KOPIA_SECRET);
        let (global, args) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            &src,
            "--resolve-secrets",
            "--secrets",
            &sec,
        ]);
        let err = run(&global, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("media/app"), "{msg}");
        assert!(msg.contains("spec.kopia.repository"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_args_rejects_invalid_offline_combinations() {
        // R1: --apply with offline input.
        let (g, a) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            "x.yaml",
            "--repository",
            "r",
            "--apply",
        ]);
        assert!(
            validate_args(&g, &a)
                .unwrap_err()
                .to_string()
                .contains("--apply")
        );

        // R3: offline --resolve-secrets with no Secret source.
        let (g, a) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            "x.yaml",
            "--resolve-secrets",
        ]);
        assert!(
            validate_args(&g, &a)
                .unwrap_err()
                .to_string()
                .contains("no Secret source")
        );

        // R4: secret flags without offline input.
        let (g, a) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "--resolve-secrets",
            "--secrets",
            "s.yaml",
        ]);
        assert!(
            validate_args(&g, &a)
                .unwrap_err()
                .to_string()
                .contains("only apply to offline input")
        );

        // A clean offline combo validates.
        let (g, a) = parse(&[
            "kubectl-kopiur",
            "migrate",
            "volsync",
            "-f",
            "x.yaml",
            "--repository",
            "r",
        ]);
        assert!(validate_args(&g, &a).is_ok());
    }

    fn base64(s: &str) -> String {
        use std::fmt::Write;
        // tiny local base64 (test-only; avoids a dev-dependency)
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = s.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
            let _ = write!(
                out,
                "{}{}",
                TABLE[(n >> 18) as usize & 63] as char,
                TABLE[(n >> 12) as usize & 63] as char
            );
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }
}
