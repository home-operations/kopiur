# CSI snapshot stack (e2e only)

The `snapshot-stack` mise task installs the CSI stack into the throwaway kind cluster.
It supports the `copyMethod: Snapshot`/`Clone` and group snapshot scenarios.
The snapshot controller and all six snapshot CRDs come from
`oci://ghcr.io/home-operations/charts/snapshot-controller`, pinned to chart `0.1.2`
(appVersion `v8.7.0`). The `snapshot-manifests` dependency uses vendir to fetch the
host-path driver manifests and sidecar RBAC into `target/e2e/csi-snapshot`.
The committed `vendir.yml` selects the upstream files and release tags.
The committed `vendir.lock.yml` pins their exact Git commits.
Normal setup uses `vendir sync --locked`.
CI caches the fetched directory using a key derived from `vendir.lock.yml` and `vendir.yml`.
An exact cache hit skips the fetch; changing either file creates a new cache entry.
Only the local storage class with delayed binding (`32-*`) and group snapshot class (`33-*`) stay in this directory as manifests.

The task caches the chart archive under `target/e2e/charts` and preloads the pinned
container images into the kind node. Fetching the manifests and uncached charts or images requires network access.
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

Paths below are relative to `target/e2e/csi-snapshot`.
The Helm chart supplies RBAC for the snapshot controller.
The host-path driver's provisioner, attacher, snapshotter, and resizer sidecars need the separate RBAC files listed here.

| Generated files                                                                                                                           | Upstream source                       | Release   |
| ----------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------- | --------- |
| `hostpath/deploy/kubernetes-1.35/hostpath/csi-hostpath-{driverinfo,plugin,snapshotclass}.yaml`, `hostpath/examples/csi-storageclass.yaml` | `kubernetes-csi/csi-driver-host-path` | `v1.19.0` |
| `provisioner/rbac.yaml`                                                                                                                   | `kubernetes-csi/external-provisioner` | `v6.4.0`  |
| `attacher/rbac.yaml`                                                                                                                      | `kubernetes-csi/external-attacher`    | `v4.13.0` |
| `snapshotter/rbac-csi-snapshotter.yaml`                                                                                                   | `kubernetes-csi/external-snapshotter` | `v8.7.0`  |
| `resizer/rbac.yaml`                                                                                                                       | `kubernetes-csi/external-resizer`     | `v2.3.0`  |

vendir copies the upstream files without local changes. The generated directory is
covered by the repository's `target/` ignore rule.

`csi-hostpath-plugin.yaml` binds the plugin's ServiceAccount to a ClusterRole named
`external-snapshot-metadata-runner`. That role belongs to the optional snapshot-metadata
sidecar, which this stack does not install, so the binding grants nothing. It stays
because upstream ships it in the same file.

After the chart is installed, the task applies the files in this order:
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
image and the `image:` refs in the generated plugin manifest.

## Refreshing

Update `CHART_VERSION` in `crates/e2e/mise.toml` to change the snapshot controller and CRDs.
Make sure that the controller preload tag matches the chart's appVersion.
Update the CSI snapshotter sidecar image, its preload tag, and its RBAC with the same upstream release.
Keep the chart version and appVersion in this document current.

To update the host-path driver or a sidecar, change its release tag in `vendir.yml`.
If the release changes manifest locations, update `includePaths` and the paths used by the task.
From the repository root, regenerate the lock and fetched files:

```sh
mise exec -- vendir sync --file crates/e2e/manifests/csi-snapshot/vendir.yml \
  --lock-file crates/e2e/manifests/csi-snapshot/vendir.lock.yml
```

Review and commit the configuration and lock changes, then update the image preload tags in `snapshot-stack`.
Run `mise run //crates/e2e:snapshot-manifests` to test fetching from the lock alone.
Run `mise run //crates/e2e:snapshot-stack` against the throwaway kind cluster to test installation.
Do not commit the generated files under `target/`.
