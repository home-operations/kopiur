# Vendored CSI snapshot stack (e2e only)

These pinned manifests install a snapshot-capable CSI driver into the throwaway kind
cluster so the `copyMethod: Snapshot`/`Clone` e2e scenarios (`tests/copy_methods.rs`)
can run **hermetically** — applied from disk by the `snapshot-stack` mise task, with the
referenced images preloaded into the node (no network at provision time).

## Provenance

| Files                                                                                                          | Source                                                                                                                                                                                                              | Pinned version                       |
| -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------ |
| `01-03-crd-*`, `06-08-crd-volumegroupsnapshot*`, `04-rbac-snapshot-controller`, `05-setup-snapshot-controller` | [kubernetes-csi/external-snapshotter](https://github.com/kubernetes-csi/external-snapshotter) `client/config/crd` + `deploy/kubernetes/snapshot-controller`                                                         | `v8.7.0` (controller image `v8.7.0`) |
| `10-hostpath-driverinfo`, `29-hostpath-plugin`, `30-hostpath-snapshotclass`                                    | [kubernetes-csi/csi-driver-host-path](https://github.com/kubernetes-csi/csi-driver-host-path) `deploy/kubernetes-latest/hostpath` (→ `kubernetes-1.35`)                                                             | `v1.19.0`                            |
| `31-hostpath-storageclass`                                                                                     | csi-driver-host-path `examples/csi-storageclass.yaml`                                                                                                                                                               | `v1.19.0`                            |
| `20-23-rbac-*`                                                                                                 | external-provisioner `v6.4.0`, external-attacher `v4.13.0` and external-resizer `v2.3.0` `deploy/kubernetes/rbac.yaml`; external-snapshotter `v8.7.0` `deploy/kubernetes/csi-snapshotter/rbac-csi-snapshotter.yaml` | per-sidecar                          |
| `32-hostpath-storageclass-wffc`, `33-hostpath-groupsnapshotclass`                                              | local, not vendored                                                                                                                                                                                                 | n/a                                  |

The vendored files are copies of the upstream files, reformatted by `oxfmt`. There is
one local change: `05-setup-snapshot-controller` sets the controller image to `v8.7.0`,
because the upstream manifest at the `v8.7.0` tag still pins `v8.6.0`.

`29-hostpath-plugin` binds the plugin's ServiceAccount to a ClusterRole named
`external-snapshot-metadata-runner`. That role belongs to the optional snapshot-metadata
sidecar, which this stack does not install, so the binding grants nothing. It stays
because upstream ships it in the same file.

Filename prefixes set the `kubectl apply` order: CRDs → driverinfo → sidecar RBAC →
plugin → snapshot class → storage class → group snapshot class.

## Group snapshots

`groupsnapshot.storage.k8s.io` is a **separate API group** from
`snapshot.storage.k8s.io`, with its own CRDs (`06-08-*`) and its own default-class
annotation. The CRDs serve three versions: `v1`, `v1beta2` (the stored version) and
`v1beta1` (deprecated). kopiur drives only `v1`, and `33-hostpath-groupsnapshotclass` is
written at `v1`.

Group snapshots are on by default from external-snapshotter 8.7 (`CSIVolumeGroupSnapshot`
is `{Default: true, PreRelease: GA}` in `pkg/features`). Neither the `snapshot-controller`
nor the `csi-snapshotter` sidecar needs a flag. Older releases needed
`--feature-gates=CSIVolumeGroupSnapshot=true` on both, and without it a
`VolumeGroupSnapshot` stayed forever with no `status.readyToUse`.

`hostpathplugin` implements the CSI `GroupController` service and advertises
`CREATE_DELETE_GET_VOLUME_GROUP_SNAPSHOT`, so no driver change is needed — which is why
kopiur's `groupBy: VolumeGroupSnapshot` path is testable in kind at all.

The image tags preloaded by the `snapshot-stack` task MUST stay in lockstep with the
`image:` refs in these files (sig-storage release tags are immutable).

## Refreshing

```sh
git clone --depth 1 --branch <hostpath-tag> https://github.com/kubernetes-csi/csi-driver-host-path /tmp/csi-hp
# copy deploy/kubernetes-latest/hostpath/{csi-hostpath-driverinfo,csi-hostpath-plugin,csi-hostpath-snapshotclass}.yaml
# + examples/csi-storageclass.yaml, re-fetch the external-snapshotter CRDs/controller and
# the sidecar rbac.yaml files at the versions the plugin manifest's image tags imply,
# run `mise exec -- oxfmt crates/e2e/manifests/csi-snapshot/*.yaml`, re-apply the local
# change listed under Provenance, then update the preload tags in crates/e2e/mise.toml
# [tasks.snapshot-stack].
```
