# Google Cloud Storage

The GCS backend stores the kopia repository in a **Google Cloud Storage** bucket, authenticating with a service-account key. GCS credentials are delivered as a **file**, not as an environment variable. The next section explains why.

## Provider prerequisites

- A **GCS bucket**. Kopiur does not create it.
- A **service account** with object admin on that bucket, meaning `roles/storage.objectAdmin`, scoped to the bucket where possible. You also need a **JSON key** for it.

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

`roles/storage.objectAdmin` is the right role. kopia needs to create, read, list, **and delete** objects, because retention and [maintenance](../maintenance.md) delete expired blobs. The read-only and creator roles both break maintenance.

///

## The Secret shape

GCS is one of the three **file-delivered** backends. kopia's GCS path wants a credentials _file_, and the SDK environment variable `GOOGLE_APPLICATION_CREDENTIALS` holds a _path_, not the JSON.

So Kopiur reads the JSON body from a well-known environment key. The mover writes it to a private file with mode `0600`, then passes `--credentials-file`.

| Secret key              | Required | What it is                                                                                            |
| ----------------------- | -------- | ----------------------------------------------------------------------------------------------------- |
| `KOPIA_GCS_CREDENTIALS` | yes      | The **full service-account key JSON**, exactly as issued. Written to a file, then passed to kopia as `--credentials-file`. |
| `KOPIA_PASSWORD`        | **yes**  | The repository encryption password.                                                                   |

```yaml
stringData:
    KOPIA_GCS_CREDENTIALS: |
        { "type": "service_account", "project_id": "...", ... }
    KOPIA_PASSWORD: "choose-something-long-and-random"
```

/// info | Why KOPIA_GCS_CREDENTIALS and not GOOGLE_APPLICATION_CREDENTIALS

By Google's own convention, `GOOGLE_APPLICATION_CREDENTIALS` is a **path** to a key file. Putting JSON under that name would be misread.

So Kopiur takes the JSON **body** under `KOPIA_GCS_CREDENTIALS`, writes it to a file with mode `0600` in the mover, and points kopia at the path. The secret never lands on kopia's command line.

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/gcs.yaml:repository"
```

## Fields reference (`backend.gcs`)

| Field            | Required | Default     | Example                    | What it controls                                                                                                 |
| ---------------- | -------- | ----------- | -------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `bucket`         | yes      | —           | `my-kopia-backups`         | The GCS bucket holding the repository. The bare name, with no `gs://`.                                            |
| `prefix`         | no       | bucket root | `clusters/prod/`           | Object-name prefix so several repos can share one bucket. End it with `/`.                                        |
| `auth.secretRef` | no¹      | —           | `{ name: gcs-repo-creds }` | Names the credential Secret above. Same namespace as the `Repository`; a `ClusterRepository` adds `namespace:`. Mutually exclusive with `workloadIdentity`. |
| `auth.workloadIdentity.serviceAccountName` | no¹ | — | `backup-mover`      | Run the mover Jobs as this ServiceAccount instead of a key file. You create it and bind it to a Google service account. See [Workload identity](#workload-identity-gke). |

¹ Set **exactly one** of `auth.secretRef` or `auth.workloadIdentity`. The webhook enforces this. You may omit `auth` entirely when `KOPIA_GCS_CREDENTIALS` lives in the encryption-password Secret.

## Customization — the values you actually change

- **`bucket` and `prefix`** set where snapshots land.
- **`create.enabled`** initializes the repository if it's missing. The creation-time algorithms are fixed forever. See [creation](../repositories.md#encryption-and-repository-creation).
- **`moverDefaults.cache`** sizes the mover cache. See [movers](../movers.md).

## Workload identity (GKE) { #workload-identity-gke }

On GKE with Workload Identity Federation, you can drop the service-account key JSON entirely.

Set `auth.workloadIdentity.serviceAccountName` and every mover Job runs **as that ServiceAccount**. With no credentials file supplied, kopia falls back to Application Default Credentials, which the GKE metadata server answers with the bound Google service account's identity.

There is no key to mint, rotate, or leak. The only secret left in the cluster is `KOPIA_PASSWORD`.

What you provide:

1. A **Google service account** with `roles/storage.objectAdmin` on the bucket. That is the [setup above](#provider-prerequisites), minus step 4, because there is no key.
2. The **Workload Identity binding**. Grant `roles/iam.workloadIdentityUser` from `PROJECT.svc.id.goog[<namespace>/<sa-name>]` to that Google service account. Then create a Kubernetes ServiceAccount annotated `iam.gke.io/gcp-service-account`, present in every namespace mover Jobs run in.
3. `auth.workloadIdentity.serviceAccountName` on the backend, instead of `auth.secretRef`.

```yaml
--8<-- "deploy/examples/backends/gcs-workload-identity.yaml"
```

## As a `ClusterRepository`

The same `backend.gcs` stanza works on a cluster-scoped [`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository), with two requirements. Every Secret reference must carry an explicit `namespace:`, and the Secret must exist in the namespaces the movers run in. See [Movers](../movers.md).

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny smoke-test: a throwaway PVC, a `SnapshotPolicy`, and a `Snapshot`, all pointed at the `gcs-primary` repository above. It takes you from "applied" to "a snapshot in my bucket" in one go.

/// warning | Fill in the credentials first

The smoke backup only goes green once `KOPIA_GCS_CREDENTIALS` holds a **real** service-account key JSON. With the `REPLACE_ME` placeholders the `Repository` stalls at `Failed`, because kopia can't reach the bucket, and the `Snapshot` stays `Pending`.

///

**1. Apply the bundle.** That is the `backups` namespace, the Secret, the Repository, and the smoke-test objects:

```console
$ kubectl apply -f deploy/examples/backends/gcs.yaml
```

**2. Wait for the repository to be `Ready`.** Everything else waits on this:

```console
$ kubectl -n backups wait --for=condition=Ready repository/gcs-primary --timeout=2m
repository.kopiur.home-operations.com/gcs-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it rather than apply it. The namespace, Secret, Repository, PVC, and policy already exist and report unchanged. The `Snapshot` is the one new object:

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

The output above is illustrative. The `Snapshot` has no fixed `Succeeded` *condition*, so to wait on it in a script, key on the phase:

```console
$ kubectl -n backups wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/smoke-now-abc12 --timeout=5m
```

**5. Prove the data really moved.** `status.stats` shows non-zero `bytesNew` and `filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia snapshot ID in your bucket:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

Both outputs are illustrative; sizes and the ID vary. Non-zero `bytesNew` proves the backup uploaded real content to GCS.

**6. Clean up** the smoke-test when you're done:

```console
$ kubectl -n backups delete snapshot --all       # finalizer also deletes the kopia snapshot
$ kubectl -n backups delete snapshotpolicy smoke
$ kubectl -n backups delete pvc smoke-data
```

/// warning | Deleting a Snapshot deletes its snapshot

A produced `Snapshot` defaults to `deletionPolicy: Delete`, so removing the CR runs `kopia snapshot delete` through a finalizer. Use `Retain` or `Orphan` to keep the data. See [Backups → deletionPolicy](../backups.md#deletionpolicy--what-happens-to-the-snapshot).

///

From here the rest of the lifecycle is the same on every backend. Only the `Repository` differs. Put it on a cron with a `SnapshotSchedule`, described in [Backups & schedules](../backups.md) and [Example 01](../examples.md#example-01--single-pvc-scheduled). Restore by picking a `Snapshot`, described in [Restores](../restores.md) and [Example 03](../examples.md#example-03--restore-by-picking-a-snapshot).

## Troubleshooting

/// warning | Paste the JSON body, not a path

A common mistake is putting a filename or a path under `KOPIA_GCS_CREDENTIALS`. It must be the **entire key JSON**, exactly as issued, including the `BEGIN PRIVATE KEY` block. The mover writes that body to the credentials file for you.

///

- **`403` or permission denied.** The service account lacks object admin on the bucket. Grant `roles/storage.objectAdmin` scoped to the bucket. If the bucket predates uniform bucket-level access, a legacy object ACL can also deny the service account; prefer turning uniform access on.
- **Malformed JSON.** A clipped or re-indented key fails to parse. Copy the file contents unchanged. The `private_key` field must keep its embedded `\n` escapes.
- **Key rejected after rotation.** A disabled or deleted service-account key fails like a wrong key. Mint a new one with `gcloud iam service-accounts keys create` and update the Secret in place. The operator re-verifies when the Secret changes.

## See also

- [Object lock (ransomware protection)](s3.md#object-lock-ransomware-protection): `spec.parameters.blobRetention` works on GCS too. The S3 page documents it in full.
- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, and creation.
- [Movers, RBAC & credentials](../movers.md): where the credential Secret must live.
- Sibling backends: [S3](s3.md) · [Azure](azure.md) · [rclone](rclone.md), which is one way to reach Google Drive.
