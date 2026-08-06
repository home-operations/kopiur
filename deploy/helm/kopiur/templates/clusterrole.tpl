{{- if eq .Values.installScope "cluster" -}}
# RBAC rules SYNCED from `cargo xtask gen-rbac` (deploy/rbac/operator-clusterrole.yaml).
# That xtask is the SOURCE OF TRUTH — it derives the kopiur.home-operations.com rules from the
# kube-rs Resource traits. If you edit kopiur.home-operations.com permissions, edit the
# xtask and re-run `cargo xtask gen-rbac`, then re-sync these rules. Names/labels
# are Helm-templated so the chart owns them.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.fullname" . }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
  - apiGroups:
      - kopiur.home-operations.com
    resources:
      - repositories
      - snapshotpolicies
      - snapshots
      - snapshotschedules
      - restores
      - maintenances
      - repositoryreplications
      - clusterrepositories
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups:
      - kopiur.home-operations.com
    resources:
      - repositories/status
      - repositories/finalizers
      - snapshotpolicies/status
      - snapshotpolicies/finalizers
      - snapshots/status
      - snapshots/finalizers
      - snapshotschedules/status
      - snapshotschedules/finalizers
      - restores/status
      - restores/finalizers
      - maintenances/status
      - maintenances/finalizers
      - repositoryreplications/status
      - repositoryreplications/finalizers
      - clusterrepositories/status
      - clusterrepositories/finalizers
    verbs: [get, update, patch]
  - apiGroups: [""]
    resources:
      - pods
      - persistentvolumeclaims
      - configmaps
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [""]
    resources:
      - pods/exec
    verbs: [create, get]
  # kube's Recorder writes events.k8s.io/v1 Events (not the legacy core Event),
  # so both api groups are required or the create is 403'd and the Event dropped.
  - apiGroups: ["", "events.k8s.io"]
    resources:
      - events
    verbs: [create, patch]
  # Secrets READ is always required (the controller resolves repository credentials).
  # The WRITE verbs are gated per opt-in feature for least privilege — the generated
  # deploy/rbac/operator-clusterrole.yaml is the maximal "all features" set; the chart
  # only grants what's enabled. A feature used without its flag surfaces an actionable
  # 403 in the resource's status (see crates/controller/src/io/creds.rs + server.rs).
  - apiGroups: [""]
    resources:
      - secrets
    verbs: [get, list, watch]
  {{- if .Values.features.credentialProjection.enabled }}
  # Credential projection (spec.credentialProjection): SSA-copy the repository Secret
  # into each mover Job namespace. `create` cannot be resourceName-scoped (the
  # authorizer can't match a name at create time) and the projected name embeds the
  # consuming CR's name, so the grant is necessarily unscoped — a broader blast
  # radius, hence the toggle. ownerRef GC reaps the copy with its CR in the steady
  # state; `delete` backs the same feature's cleanup paths: the leader-only sweep of
  # legacy per-run copies left by pre-stable-naming versions, reap-on-shrink when a
  # backend re-config drops a source Secret, and reap-on-disable when the consumer
  # turns projection off.
  - apiGroups: [""]
    resources:
      - secrets
    verbs: [create, patch, delete]
  {{- end }}
  {{- if .Values.features.kopiaUi.enabled }}
  # kopia web-UI server (spec.server): create-once the generated-auth Secret, SSA the
  # cross-namespace credentials mirror, and delete both on teardown / namespace
  # migration (owner-ref GC can't reach a cluster-scoped owner's namespaced children).
  - apiGroups: [""]
    resources:
      - secrets
    verbs: [create, patch, delete]
  {{- end }}
  # Services exposing the kopia web-UI server (spec.server).
  - apiGroups: [""]
    resources:
      - services
    verbs: [get, list, watch, create, update, patch, delete]
  # Read Namespaces to check the privileged-movers opt-in annotation (§4.11/§G16).
  - apiGroups: [""]
    resources:
      - namespaces
    verbs: [get, list, watch]
  # Mover Jobs and the kopia web-UI server Deployment (spec.server).
  - apiGroups: [batch]
    resources:
      - jobs
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [apps]
    resources:
      - deployments
    verbs: [get, list, watch, create, update, patch, delete]
  # RWO Multi-Attach avoidance: discover the node a ReadWriteOnce source/destination
  # PVC is attached to so the mover can be pinned there. The bound PV's nodeAffinity
  # (topology-pinned volumes) and the CSI VolumeAttachment (ground-truth attached node)
  # are read-only fallbacks when no consuming pod is found. `patch` lets staging flip a
  # staged PVC's bound PV from Retain to Delete before deleting it (so a Retain
  # StorageClass doesn't leak the PV + backend volume). All cluster-scoped.
  - apiGroups: [""]
    resources:
      - persistentvolumes
    verbs: [get, list, watch, patch]
  - apiGroups: [storage.k8s.io]
    resources:
      - volumeattachments
    verbs: [get, list, watch]
  # Read StorageClasses to resolve the source PVC's CSI provisioner (the driver the
  # staged VolumeSnapshotClass must match).
  - apiGroups: [storage.k8s.io]
    resources:
      - storageclasses
    verbs: [get, list, watch]
  # CSI volume snapshots used as a consistent source for snapshotting (copyMethod:
  # Snapshot/Clone). `patch` backs the server-side apply used to create the staged
  # VolumeSnapshot. VolumeSnapshotClasses are read to pick the driver's class and to
  # detect whether the snapshot stack is installed; VolumeSnapshotContents are deleted
  # on cleanup.
  - apiGroups: [snapshot.storage.k8s.io]
    resources:
      - volumesnapshots
    verbs: [get, list, watch, create, patch, delete]
  - apiGroups: [snapshot.storage.k8s.io]
    resources:
      - volumesnapshotclasses
    verbs: [get, list, watch]
  - apiGroups: [snapshot.storage.k8s.io]
    resources:
      - volumesnapshotcontents
    verbs: [get, list, watch, delete]
  - apiGroups: [groupsnapshot.storage.k8s.io]
    resources:
      # Cluster-scoped: resolving the class for a group capture is a
      # cluster-wide read, which is why `groupBy: VolumeGroupSnapshot` cannot
      # work on a namespaced install.
      - volumegroupsnapshotclasses
    verbs: [get, list, watch]
  - apiGroups: [groupsnapshot.storage.k8s.io]
    resources:
      - volumegroupsnapshots
    # `patch` because N members race to server-side-apply the SAME shared group
    # object; SSA over a deterministic name is what makes that convergent.
    verbs: [get, list, watch, create, patch, delete]
  # Per-namespace mover RBAC minted by the controller (§4.12): a least-privilege
  # `kopiur-mover` ServiceAccount + a RoleBinding (to the mover ClusterRole) created
  # in each mover Job's namespace. Without these the mover Job's SA does not exist in
  # the workload namespace and the Job never schedules a pod.
  # `io::ensure_mover_rbac` mints these via server-side apply (a PATCH), so `patch`
  # (and `update`) are required alongside `create`/`get` — without `patch` the apply
  # is 403'd and the mover SA is never minted, so the mover Job FailedCreates.
  # `list`/`watch` for workload identity: the repository controllers watch
  # ServiceAccounts so creating an `auth.workloadIdentity` SA un-sticks a
  # blocked repository immediately.
  - apiGroups: [""]
    resources:
      - serviceaccounts
    verbs: [get, list, watch, create, update, patch]
  - apiGroups: ["rbac.authorization.k8s.io"]
    resources:
      - rolebindings
    verbs: [get, create, update, patch]
  {{- if eq (include "kopiur.webhook.selfManaged" .) "true" }}
  # Self-managed webhook TLS (webhook.tls.mode: self): the controller mints its
  # own serving cert and injects the caBundle into its webhook configurations, so
  # cert-manager is not required. get+patch are resourceName-scoped to the two
  # configs the chart owns. `secrets` create is unscoped (the authorizer cannot
  # match a resourceName at create time); the rotation re-apply (update/patch) is
  # scoped to the serving Secret by name. SYNCED from xtask (webhook_cert_* rules).
  - apiGroups: ["admissionregistration.k8s.io"]
    resources:
      - validatingwebhookconfigurations
      - mutatingwebhookconfigurations
    resourceNames:
      - {{ include "kopiur.fullname" . }}-validating
      - {{ include "kopiur.fullname" . }}-mutating
    verbs: [get, patch]
  - apiGroups: [""]
    resources: [secrets]
    verbs: [create]
  - apiGroups: [""]
    resources: [secrets]
    resourceNames:
      - {{ .Values.webhook.tls.secretName }}
    verbs: [update, patch]
  {{- end }}
{{- end }}
