# Google Cloud Storage

The GCS backend stores the kopia repository in a **Google Cloud Storage** bucket,
authenticating with a service-account key. GCS credentials are delivered as a
**file**, not an environment variable (see below).

## Provider prerequisites

- A **GCS bucket** (Kopiur does not create it).
- A **service account** with object admin on that bucket
  (`roles/storage.objectAdmin`, scoped to the bucket where possible), and a **JSON
  key** for it.

/// example | The full provider-side setup with gcloud

```console
# 1. The bucket (uniform bucket-level access, so IAM is the only ACL surface):
$ gcloud storage buckets create gs://my-kopia-backups \
    --location europe-west4 --uniform-bucket-level-access

# 2. A dedicated service account for the movers:
$ gcloud iam service-accounts create kopia \
    --display-name "kopia repository access"

# 3. Object admin on THIS bucket only (not project-wide):
$ gcloud storage buckets add-iam-policy-binding gs://my-kopia-backups \
    --member serviceAccount:kopia@PROJECT.iam.gserviceaccount.com \
    --role roles/storage.objectAdmin

# 4. The JSON key — this file's contents go under KOPIA_GCS_CREDENTIALS:
$ gcloud iam service-accounts keys create key.json \
    --iam-account kopia@PROJECT.iam.gserviceaccount.com
```

`roles/storage.objectAdmin` is the right role: kopia needs to create, read,
list, **and delete** objects (retention and [maintenance](../maintenance.md)
delete expired blobs). The read-only and creator roles both break maintenance.

///

## The Secret shape

GCS is one of the three **file-delivered** backends. kopia's GCS path wants a
credentials _file_, and the SDK env var `GOOGLE_APPLICATION_CREDENTIALS` holds a
_path_, not the JSON — so Kopiur reads the JSON body from a well-known env key and
the mover writes it to a private (`0600`) file, then passes `--credentials-file`.

| Secret key              | Required | What it is                                                                                            |
| ----------------------- | -------- | ----------------------------------------------------------------------------------------------------- |
| `KOPIA_GCS_CREDENTIALS` | yes      | The **full service-account key JSON, verbatim**. Materialized to a file → kopia `--credentials-file`. |
| `KOPIA_PASSWORD`        | **yes**  | The repository encryption password.                                                                   |

```yaml
stringData:
    KOPIA_GCS_CREDENTIALS: |
        { "type": "service_account", "project_id": "...", ... }
    KOPIA_PASSWORD: "choose-something-long-and-random"
```

/// info | Why KOPIA_GCS_CREDENTIALS and not GOOGLE_APPLICATION_CREDENTIALS

`GOOGLE_APPLICATION_CREDENTIALS` is, by Google's own convention, a **path** to a
key file — putting JSON under that name would be misread. Kopiur takes the JSON
**body** under `KOPIA_GCS_CREDENTIALS`, writes it to a `0600` file in the mover,
and points kopia at the path. The secret never lands on kopia's argv.

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/gcs.yaml:repository"
```

## Fields reference (`backend.gcs`)

| Field            | Required | Default     | Example                    | What it controls                                                                                                 |
| ---------------- | -------- | ----------- | -------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `bucket`         | yes      | —           | `my-kopia-backups`         | The GCS bucket holding the repository. The bare name — no `gs://`.                                                |
| `prefix`         | no       | bucket root | `clusters/prod/`           | Object-name prefix so several repos can share one bucket. End it with `/`.                                        |
| `auth.secretRef` | no¹      | —           | `{ name: gcs-repo-creds }` | Names the credential Secret above. Same namespace as the `Repository`; a `ClusterRepository` adds `namespace:`. Mutually exclusive with `workloadIdentity`. |
| `auth.workloadIdentity.serviceAccountName` | no¹ | — | `backup-mover`      | Run the mover Jobs as this (user-created, GSA-bound) ServiceAccount instead of a key file — see [Workload identity](#workload-identity-gke). |

¹ Set **exactly one** of `auth.secretRef` or `auth.workloadIdentity`
(webhook-enforced). `auth` itself may be omitted when `KOPIA_GCS_CREDENTIALS`
rides the encryption-password Secret.

## Customization — the values you actually change

- **`bucket` / `prefix`** — where snapshots land.
- **`create.enabled`** — initialize the repository if missing. Creation-time
  algorithms are fixed forever — see [creation](../repositories.md#encryption-and-repository-creation).
- **`moverDefaults.cache`** — mover cache sizing ([movers](../movers.md)).

## Workload identity (GKE) { #workload-identity-gke }

On GKE with Workload Identity Federation, you can drop the service-account key
JSON entirely: set `auth.workloadIdentity.serviceAccountName` and every mover
Job runs **as that ServiceAccount**. With no credentials file supplied, kopia
falls back to Application Default Credentials — which the GKE metadata server
answers with the bound Google service account's identity. No key to mint,
rotate, or leak; the only secret left in the cluster is `KOPIA_PASSWORD`.

What you provide:

1. A **Google service account** with `roles/storage.objectAdmin` on the bucket
   (the [setup above](#provider-prerequisites), minus step 4 — no key).
2. The **Workload Identity binding**: `roles/iam.workloadIdentityUser` from
   `PROJECT.svc.id.goog[<namespace>/<sa-name>]` to that GSA, and a Kubernetes
   ServiceAccount annotated `iam.gke.io/gcp-service-account`, present in every
   namespace mover Jobs run in.
3. `auth.workloadIdentity.serviceAccountName` on the backend, instead of
   `auth.secretRef`.

```yaml
--8<-- "deploy/examples/backends/gcs-workload-identity.yaml"
```

## As a `ClusterRepository`

The same `backend.gcs` stanza works on a cluster-scoped
[`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository); every
Secret reference must carry an explicit `namespace:` and the credential Secret
must exist where the movers run — see [Movers](../movers.md).

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny
smoke-test (a throwaway PVC + a `SnapshotPolicy` + a `Snapshot`) that targets the
`gcs-primary` repository above, so you can go from "applied" to "a snapshot in my
bucket" in one arc.

/// warning | Fill in the credentials first

The smoke backup only goes green once `KOPIA_GCS_CREDENTIALS` holds a **real**
service-account key JSON. With the `REPLACE_ME` placeholders the `Repository`
stalls at `Failed` (kopia can't reach the bucket) and the `Snapshot` stays
`Pending`.

///

**1. Apply the bundle** (namespace `backups`, Secret, Repository, and the
smoke-test objects):

```console
$ kubectl apply -f deploy/examples/backends/gcs.yaml
```

**2. Wait for the repository to be `Ready`** — the gate everything else waits on:

```console
$ kubectl -n backups wait --for=condition=Ready repository/gcs-primary --timeout=2m
repository.kopiur.home-operations.com/gcs-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it
(the namespace, Secret, Repository, PVC, and policy already exist and report
unchanged — the `Snapshot` is the one new object):

```console
$ kubectl create -f deploy/examples/backends/gcs.yaml
snapshot.kopiur.home-operations.com/smoke-now-abc12 created
```

**4. Watch it succeed:**

```console
$ kubectl -n backups get snapshots -w
NAME              PHASE       ORIGIN   SNAPSHOT     AGE
smoke-now-abc12   Pending     manual                2s
smoke-now-abc12   Running     manual                7s
smoke-now-abc12   Succeeded   manual   k1f1ec0a8    38s
```

(Output illustrative.) The `Snapshot` has no fixed `Succeeded` *condition*; to
wait on it in a script, key on the phase:

```console
$ kubectl -n backups wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/smoke-now-abc12 --timeout=5m
```

**5. Deep proof — the data really moved.** `status.stats` shows non-zero
`bytesNew`/`filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia
snapshot ID in your bucket:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

(Both outputs illustrative — sizes and the ID vary.) Non-zero `bytesNew` is the
proof the backup uploaded real content to GCS.

**6. Clean up** the smoke-test when you're done:

```console
$ kubectl -n backups delete snapshot --all       # finalizer also deletes the kopia snapshot
$ kubectl -n backups delete snapshotpolicy smoke
$ kubectl -n backups delete pvc smoke-data
```

/// warning | Deleting a Snapshot deletes its snapshot

A produced `Snapshot` defaults to `deletionPolicy: Delete`, so removing the CR
runs `kopia snapshot delete` via a finalizer. Use `Retain` (or `Orphan`) to keep
the data — see [Backups → deletionPolicy](../backups.md#deletionpolicy--what-happens-to-the-snapshot).

///

From here the full lifecycle is backend-independent — only the `Repository`
differs. Put it on a cron with a `SnapshotSchedule`
([Backups & schedules](../backups.md),
[Example 01](../examples.md#example-01--single-pvc-scheduled)) and restore by
picking a `Snapshot` ([Restores](../restores.md),
[Example 03](../examples.md#example-03--restore-by-picking-a-snapshot)).

## Troubleshooting

/// warning | Paste the JSON body, not a path

A common mistake is putting a filename or path under `KOPIA_GCS_CREDENTIALS`. It
must be the **entire key JSON**, verbatim (including the `BEGIN PRIVATE KEY`
block). The mover writes that body to the credentials file for you.

///

- **`403` / permission denied** — the service account lacks object admin on the
  bucket. Grant `roles/storage.objectAdmin` scoped to the bucket. If the bucket
  predates uniform bucket-level access, a legacy object ACL can also deny the SA —
  prefer turning uniform access on.
- **Malformed JSON** — a clipped or re-indented key fails to parse; copy the file
  contents unchanged. The `private_key` field must keep its embedded `\n` escapes.
- **Key rejected after rotation** — a disabled or deleted service-account key
  fails like a wrong key. Mint a new one (`gcloud iam service-accounts keys
  create`) and update the Secret in place; the operator re-verifies on the
  Secret change.

## See also

- [Repositories & backends](../repositories.md) — concepts: scope, encryption, creation.
- [Movers, RBAC & credentials](../movers.md) — where the credential Secret must live.
- Sibling backends: [S3](s3.md) · [Azure](azure.md) · [rclone](rclone.md) (for Google Drive).
