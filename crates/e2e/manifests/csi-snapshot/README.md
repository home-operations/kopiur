# CSI snapshot stack (e2e only)

The `snapshot-stack` mise task installs the CSI stack into the throwaway kind cluster.
It supports the `copyMethod: Snapshot`/`Clone` and group snapshot scenarios.
The snapshot controller and all six snapshot CRDs come from
`oci://ghcr.io/home-operations/charts/snapshot-controller`, pinned to chart `0.1.2`
(appVersion `v8.7.0`). The host-path driver, its sidecars, and the classes use the manifests in this directory.

The task caches the chart archive under `target/e2e/charts` and preloads the pinned
container images into the kind node. An uncached chart or image requires network access.
If image preload fails, the pods can pull their images from the registry.

The task applies the chart's CRDs and waits for establishment before installing the
`snapshot-controller` Helm release in `kube-system`. It applies the CRDs on every run
because Helm does not upgrade CRDs in an existing release.
The chart uses one replica and the labels that the staging-deadline tests expect.
The task sets `controller.verbosity=5` for diagnostic logs and retains the other chart defaults.

If the kind cluster uses the vendored controller deployment, recreate it before running this task.
Run `mise run //crates/e2e:down`, then `mise run //crates/e2e:test` to rebuild the test environment.
The old deployment lacks Helm ownership metadata and uses a different immutable pod selector.

## Provenance

| Files                                                                       | Source                                                                                                                                                                                                              | Pinned version |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------- |
| `10-hostpath-driverinfo`, `29-hostpath-plugin`, `30-hostpath-snapshotclass` | [kubernetes-csi/csi-driver-host-path](https://github.com/kubernetes-csi/csi-driver-host-path) `deploy/kubernetes-latest/hostpath` (→ `kubernetes-1.35`)                                                             | `v1.19.0`      |
| `31-hostpath-storageclass`                                                  | csi-driver-host-path `examples/csi-storageclass.yaml`                                                                                                                                                               | `v1.19.0`      |
| `20-23-rbac-*`                                                              | external-provisioner `v6.4.0`, external-attacher `v4.13.0` and external-resizer `v2.3.0` `deploy/kubernetes/rbac.yaml`; external-snapshotter `v8.7.0` `deploy/kubernetes/csi-snapshotter/rbac-csi-snapshotter.yaml` | per-sidecar    |
| `32-hostpath-storageclass-wffc`, `33-hostpath-groupsnapshotclass`           | local, not vendored                                                                                                                                                                                                 | n/a            |

The vendored files are copies of the upstream files, reformatted by `oxfmt`.

`29-hostpath-plugin` binds the plugin's ServiceAccount to a ClusterRole named
`external-snapshot-metadata-runner`. That role belongs to the optional snapshot-metadata
sidecar, which this stack does not install, so the binding grants nothing. It stays
because upstream ships it in the same file.

After the chart is installed, filename prefixes set the `kubectl apply` order:
driverinfo → sidecar RBAC → plugin → snapshot class → storage class → group snapshot class.

## Group snapshots

`groupsnapshot.storage.k8s.io` is a **separate API group** from
`snapshot.storage.k8s.io`, with its own CRDs in the chart and its own default-class
annotation. The CRDs serve three versions: `v1`, `v1beta2` (the stored version) and
`v1beta1` (deprecated). kopiur drives only `v1`, and `33-hostpath-groupsnapshotclass` is
written at `v1`.

Group snapshots are on by default from external-snapshotter 8.7 (`CSIVolumeGroupSnapshot`
is `{Default: true, PreRelease: GA}` in `pkg/features`). The chart also sets
`--feature-gates=CSIVolumeGroupSnapshot=true` explicitly. The `csi-snapshotter` sidecar
uses the upstream default.

`hostpathplugin` implements the CSI `GroupController` service and advertises
`CREATE_DELETE_GET_VOLUME_GROUP_SNAPSHOT`, so no driver change is needed — which is why
kopiur's `groupBy: VolumeGroupSnapshot` path is testable in kind at all.

The image tags preloaded by the `snapshot-stack` task must match the chart's controller
image and the `image:` refs in these files.

## Refreshing

Update `CHART_VERSION` in `crates/e2e/mise.toml` to change the snapshot controller and CRDs.
Make sure that the controller preload tag matches the chart's appVersion.
Update the CSI snapshotter sidecar image, its preload tag, and its RBAC with the same upstream release.
Keep the chart version and appVersion in this document current.

To update the host-path driver, copy the manifests from the selected upstream release.
Refresh the sidecar RBAC at the versions used by that release.
Format the vendored YAML with `oxfmt`, then update the preload tags in the `snapshot-stack` task.
The storage class with delayed binding (`32-*`) and group snapshot class (`33-*`) remain local fixtures.
