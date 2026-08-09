# kopiur

**Kopiur** (Kopia + Rust) is a Kopia-native Kubernetes backup operator written in Rust on [`kube-rs`](https://github.com/kube-rs/kube). It makes a kopia repository a first-class Kubernetes resource and separates the backup **recipe** from its **invocation** from its **schedule**, so backups can be triggered by cron, `kubectl create`, Argo Events, or a Helm hook — and a kopia snapshot's lifecycle is tied to its `Snapshot` CR by a finalizer + `deletionPolicy`. The whole CRD surface is modeled as Rust enums so invalid states are unrepresentable and reconcilers handle every variant at compile time. See [ADR-0003](docs/adr/0003-kopiur-rust-operator.md) for the full design.

> Status: **alpha** — API group `kopiur.home-operations.com`, version `v1alpha1`. The CRD surface may still change between releases.

## The 9 CRDs (`kopiur.home-operations.com/v1alpha1`)

| CRD                     | Scope      | Layer                | Purpose                                                                                                                                 |
| ----------------------- | ---------- | -------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `Repository`            | Namespaced | Storage              | A kopia repository owned by one namespace: backend, encryption, credentials.                                                            |
| `ClusterRepository`     | Cluster    | Storage              | A shared repository for platform teams, gated by `allowedNamespaces`.                                                                   |
| `SnapshotPolicy`        | Namespaced | Recipe               | _What_ to back up: PVC sources, identity, retention, policy, hooks — into one repository or a 1–8 multi-repository fan-out. Idempotent. |
| `Snapshot`              | Namespaced | Invocation + Catalog | One kopia snapshot as a Kubernetes object. The universal trigger entry point.                                                           |
| `SnapshotSchedule`      | Namespaced | Cron                 | _When_ it runs: cron + jitter + timezone; creates `Snapshot` CRs.                                                                       |
| `Restore`               | Namespaced | Operation            | Restore a snapshot to a PVC, or act as a passive volume-populator source.                                                               |
| `Maintenance`           | Namespaced | Lifecycle            | Schedules `kopia maintenance` quick + full with an ownership lease.                                                                     |
| `RepositoryReplication` | Namespaced | Durability           | Mirror a repository's blobs to a second backend on a schedule (the "2" in 3-2-1).                                                       |
| `SnapshotReplication`   | Namespaced | Durability           | Copy selected snapshots from one repository into another on a schedule (kopia `snapshot migrate`).                                      |

## Quickstart

```bash
# The published OCI chart is the preferred install: cosign-signed, with all
# three images digest-pinned to the release. The webhook cert is self-managed
# by default — no cert-manager required.
helm install kopiur oci://ghcr.io/home-operations/charts/kopiur \
  --namespace kopiur-system --create-namespace
kubectl get crd -l app.kubernetes.io/part-of=kopiur
```

Then apply a worked example:

```bash
kubectl apply -f deploy/examples/01-single-pvc-scheduled.yaml
```

Full install guide, prerequisites (k8s >= 1.24, optional cert-manager), install modes, and the CRD-lifecycle caveat: **[docs/install.md](docs/install.md)**.

### kubectl plugin

Day-to-day operations without hand-written YAML — trigger/inspect/restore snapshots, run maintenance, browse files inside snapshots, diagnose installs, migrate from VolSync:

```bash
kubectl krew index add kopiur https://github.com/home-operations/kopiur.git
kubectl krew install kopiur/kopiur
kubectl kopiur status
```

Or via Homebrew: `brew install home-operations/tap/kopiur` (installs the
standalone `kopiur` command, so it coexists with a krew install).

Full reference: **[docs/cli/index.md](docs/cli/index.md)**.

## Layout

```
crates/          Rust workspace (api, kopia, webhook, controller, mover, cli, telemetry, e2e, xtask)
deploy/crds/     Generated CRDs (cargo xtask gen-crds) — checked in
deploy/rbac/     Generated RBAC (cargo xtask gen-rbac) — checked in
deploy/helm/     Helm chart (deploy/helm/kopiur)
deploy/examples/ 40 runnable usage walkthroughs (numbered ladder + backends + scenarios)
docs/adr/        Architecture Decision Records (0003 is canonical)
```

## Documentation

📖 **Docs site: <https://kopiur.home-operations.com/>** — user guide, ADRs, and the generated [Rust API reference](https://kopiur.home-operations.com/rustdoc/).

- [Install guide](docs/install.md)
- [Helm chart values & modes](deploy/helm/kopiur/README.md)
- [ADR-0003 — Kopiur, a Kopia-native backup operator in Rust](docs/adr/0003-kopiur-rust-operator.md)
- [Example manifests](deploy/examples/)

## Releases

Release artifacts (archives, the Homebrew cask, the krew plugin manifest, SBOMs, and Cosign signatures) are built and published with [GoReleaser Pro](https://goreleaser.com/pro/). If it's useful to your own projects, consider [sponsoring its author](https://github.com/sponsors/caarlos0).

## License

[AGPL-3.0-only](LICENSE)
