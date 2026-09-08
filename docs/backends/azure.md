# Azure Blob Storage

The Azure backend stores the kopia repository in an **Azure Blob Storage** container. Reach for it when your storage is Azure. For an S3-compatible store, use [S3](s3.md) instead.

## Provider prerequisites

- A **storage account** and a **blob container** within it. Kopiur does not create them.
- A credential for that container, **exactly one** of:
    - the storage-account **access key**, under `AZURE_STORAGE_KEY`; or
    - a **SAS token**, under `AZURE_STORAGE_SAS_TOKEN`, scoped to the container. This is the least-privilege option.

/// example | Creating the container and credential with the Azure CLI

```console
$ az storage container create \
    --account-name mystorageacct --name kopia-backups

# Option A — the account access key (full-account access):
$ az storage account keys list \
    --account-name mystorageacct --query "[0].value" -o tsv

# Option B — a container-scoped SAS token (least privilege; note the expiry):
$ az storage container generate-sas \
    --account-name mystorageacct --name kopia-backups \
    --permissions racwdl --expiry 2027-06-12 -o tsv
```

The SAS `--permissions` must include **r**ead, **a**dd, **c**reate, **w**rite, **d**elete, and **l**ist, which is `racwdl`. kopia lists and deletes blobs during retention and [maintenance](../maintenance.md), not just at backup time.

Paste the CLI output exactly as it comes out. It has no leading `?`.

///

## The Secret shape

The mover loads this Secret with `envFrom`, so the keys reach kopia as environment variables.

| Secret key                | Required | What it is                                            |
| ------------------------- | -------- | ----------------------------------------------------- |
| `AZURE_STORAGE_KEY`       | one of¹  | The storage-account access key.                       |
| `AZURE_STORAGE_SAS_TOKEN` | one of¹  | A SAS token scoped to the container (no leading `?`). |
| `KOPIA_PASSWORD`          | **yes**  | The repository encryption password.                   |

¹ Provide **exactly one** of the key or the SAS token. kopia uses whichever one is set.

```yaml
stringData:
    AZURE_STORAGE_KEY: "REPLACE_ME" # OR AZURE_STORAGE_SAS_TOKEN, not both
    KOPIA_PASSWORD: "choose-something-long-and-random"
```

/// warning | Lose the password, lose the backups

`KOPIA_PASSWORD` encrypts the repository, and it cannot be recovered if you lose it. Store it outside the cluster and back up the Secret. See [Encryption](../repositories.md#encryption-and-repository-creation).

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/azure.yaml:repository"
```

## Fields reference (`backend.azure`)

| Field            | Required | Default        | Example                      | What it controls                                                                                                |
| ---------------- | -------- | -------------- | ---------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `container`      | yes      | —              | `kopia-backups`              | The blob container holding the repository. The container name only: not a URL, not `account/container`.         |
| `prefix`         | no       | container root | `prod/`                      | Blob-name prefix so several repos can share one container. End it with `/`.                                     |
| `storageAccount` | no       | inferred       | `mystorageacct`              | Account name. Set it always in practice. SAS tokens don't carry it, and being explicit costs nothing with a key. |
| `auth.secretRef` | no¹      | —              | `{ name: azure-repo-creds }` | Names the credential Secret above. Same namespace as the `Repository`; a `ClusterRepository` adds `namespace:`. Mutually exclusive with `workloadIdentity`. |
| `auth.workloadIdentity.serviceAccountName` | no¹ | — | `backup-mover`        | Run the mover Jobs as this ServiceAccount instead of a key or SAS token. You create it and federate it with Entra. See [Workload identity](#workload-identity-aks). Requires `storageAccount`. |

¹ Set **exactly one** of `auth.secretRef` or `auth.workloadIdentity`. The webhook enforces this. You may omit `auth` entirely when the `AZURE_*` key lives in the encryption-password Secret.

## Customization — the values you actually change

- **`container` and `prefix`** set where snapshots land.
- **`storageAccount`** is usually required, because SAS tokens don't encode the account.
- **Key vs. SAS.** You switch between them by which Secret key you set. See the SAS variant below.
- **`create.enabled`** initializes the repository if it's missing. The creation-time `encryption`, `splitter`, and `hash` values are fixed forever. See [creation](../repositories.md#encryption-and-repository-creation).

### SAS-token auth (least privilege)

A SAS token scoped to the container is time-limited, and it avoids handing the mover the full account key:

```yaml
--8<-- "deploy/examples/backends/azure-sas.yaml"
```

## Workload identity (AKS) { #workload-identity-aks }

On AKS with the workload-identity add-on, or any cluster running the azure-workload-identity webhook, you can drop the storage key and SAS token entirely.

Set `auth.workloadIdentity.serviceAccountName` and every mover Job runs **as that ServiceAccount**. Kopiur stamps the mover pods with the `azure.workload.identity/use: "true"` label. The Azure webhook then injects `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, and `AZURE_FEDERATED_TOKEN_FILE`, and kopia authenticates with the federated token. The only secret left in the cluster is `KOPIA_PASSWORD`.

What you provide:

1. A **managed identity**, or an app registration, with `Storage Blob Data Contributor` on the container, plus a **federated credential** for `system:serviceaccount:<namespace>:<sa-name>`.
2. A **ServiceAccount** annotated `azure.workload.identity/client-id: <id>`, present in every namespace mover Jobs run in.
3. `auth.workloadIdentity.serviceAccountName` on the backend. Setting it makes **`storageAccount` required**, and the webhook enforces that. The identity webhook injects the tenant, client id, and token, but not the account name.

```yaml
--8<-- "deploy/examples/backends/azure-workload-identity.yaml"
```

/// warning | Replication: don't mix static and workload-identity Azure pairs

A `RepositoryReplication` between two Azure backends must use the same auth style on both sides. If both use `workloadIdentity`, they must name the same ServiceAccount.

A mixed pair is rejected at admission. The replication pod's environment would carry the static side's `AZURE_*` credentials, and the federated side's environment-driven flags would pick them up.

///

## As a `ClusterRepository`

The same `backend.azure` stanza works on a cluster-scoped
[`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository); every
Secret reference must carry an explicit `namespace:` and the Secret must be
present where the movers run. See [Movers](../movers.md).

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny smoke-test: a throwaway PVC, a `SnapshotPolicy`, and a `Snapshot`, all pointed at the `azure-primary` repository above. It takes you from "applied" to "a snapshot in my container" in one go.

/// warning | Fill in the credentials first

The smoke backup only goes green once the `REPLACE_ME` value in the Secret is a **real** storage key or SAS token. With placeholders the `Repository` stalls at `Failed`, because kopia can't reach the container, and the `Snapshot` stays `Pending`.

///

**1. Apply the bundle.** That is the `backups` namespace, the Secret, the Repository, and the smoke-test objects:

```console
$ kubectl apply -f deploy/examples/backends/azure.yaml
```

**2. Wait for the repository to be `Ready`.** Everything else waits on this:

```console
$ kubectl -n backups wait --for=condition=Ready repository/azure-primary --timeout=2m
repository.kopiur.home-operations.com/azure-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it rather than apply it. The namespace, Secret, Repository, PVC, and policy already exist and report unchanged. The `Snapshot` is the one new object:

```console
$ kubectl create -f deploy/examples/backends/azure.yaml
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

**5. Prove the data really moved.** `status.stats` shows non-zero `bytesNew` and `filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia snapshot ID in your container:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

Both outputs are illustrative; sizes and the ID vary. Non-zero `bytesNew` proves the backup uploaded real content to Azure Blob.

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

/// warning | Provide exactly one credential

Setting **both** `AZURE_STORAGE_KEY` and `AZURE_STORAGE_SAS_TOKEN` is ambiguous. Provide one.

A SAS token must be pasted **without** a leading `?`, and it must grant read, write, list, and delete on the container.

///

/// note | SAS tokens expire, and take your backups offline with them

A SAS token carries an expiry, shown as `se=` in the token. When it lapses, every mover run starts failing with `AuthenticationFailed`, even though nothing in the cluster changed.

Pick an expiry you'll actually rotate before, put the rotation in your calendar, and update the Secret in place. The operator [watches the Secret](../repositories.md) and re-verifies the repository without you touching the `Repository` object.

///

- **`AuthenticationFailed`.** The key is wrong, the SAS token has expired, or the SAS is scoped to the wrong container. Check the `se=` timestamp inside the token. Regenerate the SAS scoped to _this_ container.
- **`ContainerNotFound`.** Create the container first. Kopiur won't.
- **Works with the key, fails with SAS.** Either the SAS is missing a permission, and it needs `racwdl`, or `storageAccount` is unset. A SAS token doesn't encode the account name.

## See also

- [Object lock (ransomware protection)](s3.md#object-lock-ransomware-protection): `spec.parameters.blobRetention` works on Azure Blob too. The S3 page documents it in full.
- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, and creation.
- [Movers, RBAC & credentials](../movers.md): where the credential Secret must live.
- Sibling backends: [S3](s3.md) · [GCS](gcs.md) · [B2](b2.md).
