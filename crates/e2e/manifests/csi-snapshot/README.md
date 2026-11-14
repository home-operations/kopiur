# Vendored CSI snapshot stack (e2e only)

These pinned manifests install a snapshot-capable CSI driver into the throwaway kind
cluster so the `copyMethod: Snapshot`/`Clone` e2e scenarios (`tests/copy_methods.rs`)
can run **hermetically** — applied from disk by the `snapshot-stack` mise task, with the
referenced images preloaded into the node (no network at provision time).

## Provenance

| Files                                                                                                          | Source                                                                                                                                                                                                 | Pinned version                       |
| -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------ |
| `01-03-crd-*`, `06-08-crd-volumegroupsnapshot*`, `04-rbac-snapshot-controller`, `05-setup-snapshot-controller` | [kubernetes-csi/external-snapshotter](https://github.com/kubernetes-csi/external-snapshotter) `client/config/crd` + `deploy/kubernetes/snapshot-controller`                                            | `v8.2.0` (controller image `v8.2.0`) |
| `10-hostpath-driverinfo`, `29-hostpath-plugin`, `30-hostpath-snapshotclass`                                    | [kubernetes-csi/csi-driver-host-path](https://github.com/kubernetes-csi/csi-driver-host-path) `deploy/kubernetes-latest/hostpath` (→ `kubernetes-1.28`)                                                | `v1.15.0`                            |
| `31-hostpath-storageclass`                                                                                     | csi-driver-host-path `examples/csi-storageclass.yaml`                                                                                                                                                  | `v1.15.0`                            |
| `09-crd-referencegrants`                                                                                       | [kubernetes-sigs/gateway-api](https://github.com/kubernetes-sigs/gateway-api) `config/crd/standard/gateway.networking.k8s.io_referencegrants.yaml`                                                     | `v1.7.0`                             |
| `20-24-rbac-*`                                                                                                 | each sidecar's `deploy/kubernetes/rbac.yaml` (external-provisioner `v5.1.0`, external-attacher `v4.7.0`, external-snapshotter `v8.2.0`, external-resizer `v1.12.0`, external-health-monitor `v0.13.0`) | per-sidecar                          |

Filename prefixes set the `kubectl apply` order: CRDs → driverinfo → sidecar RBAC →
plugin → snapshot class → storage class → group snapshot class.

## Group snapshots

`groupsnapshot.storage.k8s.io` is a **separate API group** from
`snapshot.storage.k8s.io`, with its own CRDs (`06-08-*`) and its own default-class
annotation. Two things beyond installing the CRDs are required, and without either the
CRDs are present but inert — a `VolumeGroupSnapshot` would sit forever with no
`status.readyToUse`, which is indistinguishable from a slow driver:

- `snapshot-controller` needs `--enable-volume-group-snapshots=true` (Beta as of
  external-snapshotter 8.2 / Kubernetes 1.32, but still flag-gated);
- the `csi-snapshotter` sidecar needs the same flag, so it watches
  `VolumeGroupSnapshotContent` and drives the driver's `CreateVolumeGroupSnapshot`.

`hostpathplugin` implements the CSI `GroupController` service and advertises
`CREATE_DELETE_GET_VOLUME_GROUP_SNAPSHOT`, so no driver change is needed — which is why
kopiur's `groupBy: VolumeGroupSnapshot` path is testable in kind at all.

## Cross-namespace data sources

`tests/central_execution.rs` restores a `VolumeSnapshot` into a PVC in another
namespace (kopiur `execution.mode: Central`). That is the alpha
`CrossNamespaceVolumeDataSource` feature, and it needs all of:

- the feature gate on kube-apiserver and kube-controller-manager
  (`crates/e2e/kind-config.yaml`). With it off, the API server drops the PVC's
  `dataSourceRef` and provisions an empty volume;
- the same gate on the `csi-provisioner` sidecar (`29-hostpath-plugin.yaml`);
- the `ReferenceGrant` CRD (`09-crd-referencegrants.yaml`) and the provisioner's
  `referencegrants` RBAC rule (`20-rbac-provisioner.yaml`). The provisioner syncs
  a `ReferenceGrant` informer at startup once the gate is on, so without both it
  never becomes ready.

The image tags preloaded by the `snapshot-stack` task MUST stay in lockstep with the
`image:` refs in these files (sig-storage release tags are immutable).

## Refreshing

```sh
git clone --depth 1 --branch <hostpath-tag> https://github.com/kubernetes-csi/csi-driver-host-path /tmp/csi-hp
# copy deploy/kubernetes-latest/hostpath/{csi-hostpath-driverinfo,csi-hostpath-plugin,csi-hostpath-snapshotclass}.yaml
# + examples/csi-storageclass.yaml, re-fetch the external-snapshotter CRDs/controller and
# the sidecar rbac.yaml files at the versions the plugin manifest's image tags imply,
# then update the preload tags in crates/e2e/mise.toml [tasks.snapshot-stack].
```
