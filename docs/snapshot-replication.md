# Snapshot replication

A **`SnapshotReplication`** copies selected **snapshots** — the manifests and the content they reference — from one repository into another on a schedule, wrapping `kopia snapshot migrate`. Unlike [repository replication](replication.md), which mirrors a repository's raw **blobs** to a passive destination backend, snapshot replication's source and destination are **both real repository CRs** (`Repository` or `ClusterRepository`), each with its own password, its own format, and — crucially — its own life: the destination can keep taking direct backups of its own while also receiving copies.

/// tip | When to reach for it

- **One off-site repository that is both a mirror target and a live backup target.** `RepositoryReplication` (`kopia repository sync-to`) copies blobs verbatim, so its destination must be a passive byte-for-byte mirror with exactly one writer — it cannot simultaneously be a repository other policies back up into (the sync would fight the direct writes over indexes and epochs). Snapshot replication writes through the destination's own front door, so the destination stays an ordinary first-class repository.
- **Consolidation.** Copy several team repositories into one shared off-site `ClusterRepository`.
- **Seeding.** Populate a new repository from an old one (optionally `latestOnly: true` for a cheap seed).
- **Selective copies.** Only some identities (e.g. every `pg-*` policy), or excluding scratch paths.

If all you want is a passive, byte-identical off-site mirror of one repository — same password, restore-ready as-is — [`RepositoryReplication`](replication.md) is simpler and cheaper. Reach for `SnapshotReplication` when the destination must be a repository in its own right.

///

## How it works

- **Namespaced**, living alongside its source (like `Maintenance` and `RepositoryReplication`). `sourceRef` and `destinationRef` each name a `Repository` or `ClusterRepository`; a `ClusterRepository` destination is the flagship consolidation setup. Replicating a repository into itself is rejected at admission.
- On each cron slot the controller launches a **mover Job** (`<name>-srepl-<unix>`; croner + deterministic jitter, single-flight — the same scheduling kernel `Maintenance` uses). The mover connects to the **source read-only**, connects to the destination normally, and runs `kopia snapshot migrate`.
- **Identity is preserved.** A copied snapshot keeps its `username@hostname:path`, its start/end times, and its description — at the destination it looks exactly like the snapshot it is a copy of.
- **Idempotent and incremental.** kopia keys migration on `(identity, startTime)`: a snapshot already present at the destination is skipped (`status.lastRun.alreadyPresent`), and unchanged content is deduplicated against what the destination already stores. Re-running is always safe.
- **Every run is post-verified.** `kopia snapshot migrate` exits 0 even when individual sources failed to migrate, so the mover independently re-lists the destination and **fails the run loudly** if any selected snapshot did not arrive — a green `Succeeded` phase means the copies are really there.
- **Each copy becomes a `Snapshot` CR** (`origin: replicated`) in the replication's namespace, pinned to the destination repository (`spec.repository`), with `deletionPolicy: Delete` — so the copies are first-class: visible to `kubectl get snapshots`, restorable, deletable through the CR like any other snapshot. The destination's catalog scan recognizes replicated rows and does **not** duplicate them as `discovered`.
- The copy CRs carry **no ownerReference** back to the `SnapshotReplication`: deleting the replication CR never deletes the copies. Only `spec.pruning` — or deleting the copy `Snapshot` CRs yourself — removes replicated data.

## Try it / minimal manifest

The apply-ready example is [`deploy/examples/39-snapshot-replication.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/39-snapshot-replication.yaml) — a destination `Repository` (off-site S3) with its **own** password Secret, plus the replication CR:

```yaml
--8<-- "deploy/examples/39-snapshot-replication.yaml:snapshot-replication"
```

Watch it:

```console
$ kubectl -n billing get snapshotreplications
NAME                     SOURCE        DESTINATION   SCHEDULE    PHASE       LAST   AGE
nas-primary-to-offsite   nas-primary   offsite-s3    0 6 * * *   Succeeded   2m     1d

$ kubectl -n billing get snapshots -l kopiur.home-operations.com/origin=replicated
```

`status.lastRun` carries the per-run counters: `identitiesSelected`, `snapshotsCopied`, `alreadyPresent`, `failed`, `pruned`. `kubectl kopiur status` and `kubectl kopiur doctor` render them too, and `kubectl kopiur snapshots list` shows where a replicated row was copied from (`status.copiedFrom`).

## The fields you'll change

| Field | What it does |
| --- | --- |
| `sourceRef` / `destinationRef` | The two repositories (`kind` defaults to `Repository`; both must exist, differ, and be `Ready`; the destination must be writable). |
| `schedule.cron` / `jitter` | When replication runs (Jenkins-style `H` supported). Run it **after** your backup window so each night's snapshots are there to copy. |
| `selection.identities.include` / `exclude` | Which kopia identities to copy — see [Selecting what to copy](#selecting-what-to-copy). Omit `selection` entirely to copy **every** identity's full history. |
| `selection.latestOnly` | `true` = only each identity's most recent snapshot (cheap seed); default `false` = full history. |
| `migrate.parallel` | Snapshots migrated concurrently (kopia default: 1, sequential) — the main knob for large first runs. |
| `migrate.policies` | Whether kopia **policies** ride along: `none` (default — a Kopiur-managed destination keeps retention CR-driven), `copy`, or `copyOverwrite`. |
| `migrate.throttle.source` / `.destination` | Bandwidth/ops caps for this replication's runs, one block **per side** — see [Throttling a replication](#throttling-a-replication). Each overrides that side's repository's `moverDefaults.throttle` field by field. |
| `pruning` | What happens to already-made copies on later runs: exactly one of `none` / `mirrorSource` / `retention` — see [Pruning](#pruning-the-copies). |
| `mover` | Per-run mover overrides (resources, scheduling, security context). Inherits the source repository's `moverDefaults`. `inheritSecurityContextFrom` is rejected here, as for `RepositoryReplication` — there is no workload to inherit from. |
| `credentialProjection` | Opt in to [credential projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) for a `ClusterRepository` source/destination whose Secret lives elsewhere. |
| `suspend` | Pause replication without deleting the CR. |

## Run it now

A `SnapshotReplication` normally fires on its cron, but you can ask for a copy pass **right now** — after seeding a new destination, after fixing a failed run, or just to watch the first migrate work:

```console
$ kubectl kopiur replication run nas-primary-to-offsite -n billing --wait
snapshotreplication.kopiur.home-operations.com/nas-primary-to-offsite run requested (2026-06-11T12:00:00Z)
SnapshotReplication nas-primary-to-offsite run completed at 2026-06-11T12:09:51Z
```

If a `RepositoryReplication` and a `SnapshotReplication` share a name in one namespace, add `--kind snapshot` (otherwise the kind is detected for you). The plugin just stamps the `kopiur.home-operations.com/run-requested` annotation with an RFC3339 timestamp, so plain `kubectl` works too:

```console
$ kubectl annotate snapshotreplication nas-primary-to-offsite -n billing \
    kopiur.home-operations.com/run-requested="$(date -u +%Y-%m-%dT%H:%M:%SZ)" --overwrite
```

The timestamp pins *which* request the status answers: re-applying the same value is a no-op (safe in GitOps), a **new** timestamp starts a new run, and progress lands in `status.manualRun` (`requestedAt` / `phase` / `completedAt`).

The requested run takes the **same** path as a scheduled one — the same mover, the same both-repositories-`Ready` and destination-writable gates, the same `IdentityOverlap` guard, and the same single-flight rule.

/// note | A requested run re-anchors the schedule

The next cron slot is computed from `status.lastReplicated`, and a successful requested run stamps it exactly as a scheduled run does. So running at 14:00 on an `0 6 * * *` replication means the next automatic run is 06:00 **tomorrow**. That is intended: the cron means "at least this often", and the snapshots just copied would only be re-scanned by a redundant run.

///

/// warning | Suspended? The request waits, it does not vanish

Requesting a run on a `suspend: true` replication records it as `status.manualRun.phase: Pending` and surfaces `Ready=False` with reason `SuspendedWithPendingRun`. Nothing starts until you [resume](cli/operations.md#suspend--resume) it — at which point the still-unanswered request fires immediately.

///

## Selecting what to copy

`selection.identities` takes `include` and `exclude` lists of **matchers**. Each matcher sets any of the three identity components — `username`, `hostname`, `sourcePath` — and every **set** component must match for the matcher to match (an unset component matches anything). At least one component must be set per matcher (webhook-enforced).

Components are matched with **anchored globs**: `*` matches any run of characters (including none), `?` matches exactly one — against the *whole* component, so `pg-*` matches `pg-main` but plain `pg` does not match `pg-main`. A snapshot is copied when it matches **any** `include` (an empty/absent `include` means "everything") **and no** `exclude` — exclude always wins.

```yaml
selection:
    identities:
        include:
            - username: "pg-*" # every postgres policy…
            - hostname: "media" # …plus everything from the media namespace
        exclude:
            - sourcePath: "/scratch/*" # …but never scratch paths
    latestOnly: false
```

Matching zero identities is a **successful no-op** (`NoIdentitiesMatched`), not an error — a fresh source simply has nothing to copy yet. Incomplete (interrupted) source snapshots are never copied.

## Throttling a replication

A replication is often the heaviest thing Kopiur does to your network: it reads a
whole history out of one repository and writes it into another, frequently across
a WAN link that other people are also using. `spec.migrate.throttle` caps it —
**per side**, because a replication is two repositories.

```yaml
migrate:
    throttle:
        source: # reading out of nas-primary (on-LAN, be generous)
            downloadBytesPerSecond: 209715200 # 200 MiB/s
        destination: # writing to the off-site bucket (the scarce link)
            uploadBytesPerSecond: 10485760 # 10 MiB/s
            writeOpsPerSecond: 100
```

Each side takes the same four knobs as a repository's
[`moverDefaults.throttle`](reference/crds/shared-types.md#moverdefaults):
`uploadBytesPerSecond`, `downloadBytesPerSecond`, `readOpsPerSecond`,
`writeOpsPerSecond`. Every knob you set must be at least `1` (a `0` is rejected at
admission — kopia's "no limit" is the *absent* knob, not a zero).

**Two layers, merged field by field.** Each repository can already declare
`moverDefaults.throttle`, which every mover that touches it honors. `migrate.throttle`
sits on top of that, per side:

- `throttle.source` overrides the **source** repository's defaults;
  `throttle.destination` overrides the **destination** repository's.
- The merge is per knob: a knob you set here wins, a knob you leave unset keeps
  the repository's value. Pinning one knob never drops the repository's others.
- Omit `migrate.throttle` entirely and each side simply uses its own repository's
  defaults.

The two sides never mix. A cap on the source constrains only the read connection,
because kopia's limits are **per connection** and a replication opens two of them
under two separate kopia configurations.

/// note | Why the cap isn't a flag on `snapshot migrate`

`kopia snapshot migrate` has no speed options at all. The only lever kopia offers
is `kopia repository throttle set`, which writes the limits into a *connection's*
config — so Kopiur connects each side, applies that side's limits, and lets the
migrate (which reopens those configs) inherit them. That is also why each side
needs its own block: there is nothing repository-wide to inherit. Applying a cap
to the read-only source connection is fine — kopia accepts `throttle set` there —
so a `mode: ReadOnly` source is throttled like any other.

If either application fails, the run **fails** rather than proceeding uncapped:
saturating exactly the link you asked to protect is worse than not running. The
mover logs `applied repository throttle` once per capped connection (twice for a
healthy replication), which is the quickest way to confirm the limits landed:

```console
$ kubectl -n billing logs job/nas-primary-to-offsite-srepl-1765440000 | grep 'applied repository throttle'
```

///

/// warning | A byte cap bites much harder than its number suggests

Byte-rate caps apply to **cold** backend traffic only — content already in the
mover's kopia cache is served without touching the limiter, so a warm re-run can
look entirely unthrottled. And on small-object workloads the effective throughput
lands far below the nominal number: a measured 2 MB/s cap took ~14 s to move a
28 KiB cold repository, while caps of 10 MB/s and above often did not bind at all
at that size. Set these as a **ceiling for large transfers**, generously, and
measure against your own data — a number picked to "feel safe" can turn a nightly
replication into one that never finishes.

///

/// warning | New CRD fields need `kubectl apply`, not just `helm upgrade`

`migrate.throttle` is a new CRD field. Helm's `crds/` directory is **install-only**
— a `helm upgrade` never updates CRD schemas — so on a helm-CLI upgrade an
apiserver running the old schema **silently prunes** the field from your manifest:
the object admits cleanly, `kubectl get -o yaml` shows no `throttle`, and the runs
stay uncapped with nothing to see. Apply the CRDs first (`kubectl apply
--server-side -f deploy/crds/`), or use a GitOps flow with a `CreateReplace` CRD
policy. See [CRD lifecycle](install.md#crd-lifecycle).

///

## Pruning the copies

`pruning` is exactly one of three modes; **absent means `none`**. Whatever the mode, pruning only ever considers snapshots **this replication created** (the copy CRs it labels) — never the destination's own directly-written snapshots, and never another replication's copies.

| Mode | Behavior |
| --- | --- |
| `none` *(default)* | Never prune. Copies accumulate until you delete them (or the CR they became). |
| `mirrorSource` | Delete a copy when its `(identity, startTime)` has **vanished from the source** — the destination tracks the source's own retention. |
| `retention` | Independent GFS retention over the copies at the destination (`keepDaily`, `keepWeekly`, … — the same shape as a `SnapshotPolicy`'s), regardless of what the source still holds. A `retention:` block that keeps nothing is rejected at admission, exactly as on a policy. |

/// warning | `mirrorSource` meets the mass-deletion breaker — by design

`mirrorSource` deletes at the destination whatever disappeared at the source. To stop that from becoming an attack path, mirror-source deletes are deliberately **not** stamped as operator prunes: they count as **external** deletions against the destination repository's [mass-deletion breaker](repositories.md#deletionprotection--the-mass-deletion-circuit-breaker) (`deletionProtection.threshold`, default 10). A *bulk* source-side vanish — ransomware emptying the source, a fat-fingered mass delete — is therefore **held** at the destination instead of cascading into the off-site copy in one wave. The hold shows up as `DeletionHeld` on the affected copy `Snapshot` CRs and `MassDeletionHeld` on the destination repository, and is released with the breaker's normal timestamp acknowledgement once you've confirmed the deletions are intended.

`retention`-mode prunes, by contrast, **are** operator prunes (stamped `pruned-by: replication-retention`) and bypass the breaker — they are bounded, GFS-selected, and initiated by your own spec.

///

## Operational notes

- **Copies survive the CR.** No ownerReferences: deleting the `SnapshotReplication` leaves every copy `Snapshot` CR (and its data) in place. To remove copies, delete those `Snapshot` CRs (their `deletionPolicy: Delete` cascades to the destination repository, subject to its breaker) — or let a `pruning` mode do it.
- **Both repositories must be `Ready`, and the destination writable.** The controller holds runs with `WaitingForSourceRepository` / `WaitingForDestinationRepository` conditions, and a `mode: ReadOnly` destination stalls with `DestinationReadOnly`.
- **Different passwords are fine — and expected.** The source is opened read-only with its own credentials; the destination with its own. Nothing about the two repositories needs to match (different backends, formats, passwords all work).
- **Identity overlap with destination-side policies is guarded.** If a `SnapshotPolicy` writing *directly* into the destination produces the **same kopia identity** as a copied snapshot, the two histories would interleave. The webhook denies that combination outright when `pruning: mirrorSource` is set (the prune would eat the policy's own snapshots) and warns otherwise; at runtime the controller re-checks each pass and surfaces an `IdentityOverlap` condition (skipping the run under `mirrorSource`).
- **A dedicated mover ServiceAccount.** Replication movers create/patch/delete `Snapshot` CRs (the copies), which the ordinary backup mover must never be able to do — so they run as the dedicated `kopiur-snapshot-replication-mover` ServiceAccount with its own narrowly-scoped Role, generated alongside the rest of the [RBAC](rbac.md).
- **No destination maintenance behind your back.** The mover always disables kopia's auto-maintenance; the destination's own [`Maintenance`](maintenance.md) remains the only compaction that runs there.
- **Every run is counted.** `kopiur_replication_runs_total{kind,trigger,outcome}` records each finished run, so "the nightly copy has been failing" is alertable without watching conditions. `trigger` separates `cron` from the [requested](#run-it-now) runs.
- **Size the first run.** A full-history first replication of a large repository moves everything once (idempotent thereafter). Raise `migrate.parallel`, consider `latestOnly: true` for seeding, and note the mover Job's deadline can be tuned via `mover`/`failurePolicy` knobs on big estates. If that first run is what worries the network, cap it with [`migrate.throttle`](#throttling-a-replication) rather than by shrinking the selection — but leave headroom, or the deadline becomes the thing that fails.

## See also

- [Repository replication](replication.md) — the blob-level mirror, when the destination is a passive copy.
- [Multi-repository fan-out](backups.md#repositories--one-recipe-several-repositories-fan-out) — backing up into N repositories *directly* from one `SnapshotPolicy` (and why hooks + fan-out points you back here).
- [Repositories & backends](repositories.md) — the catalog, `deletionProtection`, `identityDefaults`.
- [Disaster recovery scenario](scenarios/disaster-recovery.md)
- [Scenario 10 — DR from a replicated repository](scenarios/dr-with-replicated-repository.md) — the one-shot counterpart: `Repository.spec.seed` copies a whole repository in at first bootstrap (`seed.from.repository` is the same `kopia snapshot migrate`), instead of copying selected snapshots on a schedule.
