{{- if eq .Values.installScope "namespaced" -}}
# RBAC rules SYNCED from `cargo xtask gen-rbac` (deploy/rbac/operator-role.yaml).
# That xtask is the SOURCE OF TRUTH — it derives the kopiur.home-operations.com rules from the
# kube-rs Resource traits. If you edit kopiur.home-operations.com permissions, edit the
# xtask and re-run `cargo xtask gen-rbac`, then re-sync these rules. Names/labels
# are Helm-templated so the chart owns them.
#
# Note vs. the ClusterRole: a namespaced Role intentionally omits
# `clusterrepositories` (a cluster-scoped kind unreachable from a Role).
# ClusterRepository is only reconciled in installScope=cluster. The mover SA +
# RoleBinding minting rules ARE retained: the controller mints the mover RBAC in the
# (single, in-scope) workload namespace before each mover Job.
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: {{ include "kopiur.fullname" . }}
  namespace: {{ .Release.Namespace }}
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
      - snapshotreplications
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
      - snapshotreplications/status
      - snapshotreplications/finalizers
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
  # deploy/rbac/operator-role.yaml is the maximal "all features" set; the chart only
  # grants what's enabled. A feature used without its flag surfaces an actionable 403
  # in the resource's status (see crates/controller/src/io/creds.rs + server.rs).
  - apiGroups: [""]
    resources:
      - secrets
    verbs: [get, list, watch]
  {{- if .Values.features.credentialProjection.enabled }}
  # Credential projection (spec.credentialProjection): SSA-copy the repository Secret
  # into each mover Job namespace. create is unscopable (per-CR name); delete backs
  # the feature's cleanup paths (legacy-copy sweep, reap-on-shrink, reap-on-disable —
  # ownerRef GC covers the steady state).
  - apiGroups: [""]
    resources:
      - secrets
    verbs: [create, patch, delete]
  {{- end }}
  {{- if .Values.features.kopiaUi.enabled }}
  # kopia web-UI server (spec.server): create-once the generated-auth Secret, SSA the
  # cross-namespace credential mirrors (one per distinct Secret the repository
  # references — the password Secret plus a separate backend auth Secret), and delete
  # all of them on teardown / namespace migration (owner-ref GC can't reach a
  # cluster-scoped owner's namespaced children).
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
  # Mover Jobs and the kopia web-UI server Deployment (spec.server).
  - apiGroups: [batch]
    resources:
      - jobs
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [apps]
    resources:
      - deployments
    verbs: [get, list, watch, create, update, patch, delete]
  # CSI volume snapshots used as a consistent source for snapshotting (copyMethod:
  # Snapshot/Clone). `patch` backs the server-side apply that creates the staged
  # VolumeSnapshot. (Cluster-scoped VolumeSnapshotClasses/Contents + StorageClasses are
  # granted in the ClusterRole; a namespaced install cannot stage CSI snapshots.)
  - apiGroups: [snapshot.storage.k8s.io]
    resources:
      - volumesnapshots
    verbs: [get, list, watch, create, patch, delete]
  - apiGroups: [groupsnapshot.storage.k8s.io]
    resources:
      - volumegroupsnapshots
    # `patch` because N members race to server-side-apply the SAME shared group
    # object; SSA over a deterministic name is what makes that convergent.
    verbs: [get, list, watch, create, patch, delete]
  # Per-namespace mover RBAC minted by the controller (§4.12). Minted via
  # server-side apply (PATCH), so `patch`/`update` are required, not just create/get.
  # `list`/`watch` for workload identity (SA watch — see clusterrole.yaml).
  - apiGroups: [""]
    resources:
      - serviceaccounts
    verbs: [get, list, watch, create, update, patch]
  - apiGroups: ["rbac.authorization.k8s.io"]
    resources:
      - rolebindings
    verbs: [get, create, update, patch]
  {{- if .Values.leaderElection.enabled }}
  # Leader election (--leader-elect): the controller claims/renews ONE Lease in
  # the release namespace. get/update are resourceName-scoped to it; create
  # cannot be name-scoped (no resourceName at create time) but stays
  # namespace-local. SYNCED from xtask (leader_election_rules).
  - apiGroups: ["coordination.k8s.io"]
    resources:
      - leases
    verbs: [create]
  - apiGroups: ["coordination.k8s.io"]
    resources:
      - leases
    resourceNames:
      - {{ include "kopiur.fullname" . }}
    verbs: [get, update]
  {{- end }}
  {{- if eq (include "kopiur.webhook.selfManaged" .) "true" }}
  # Self-managed webhook TLS (webhook.tls.mode: self): writing the serving Secret
  # (namespace-local). create is unscoped (no resourceName at create time); the
  # rotation re-apply is scoped to the serving Secret by name. The cluster-scoped
  # webhook-config patch can't live in a Role — it's granted by the ClusterRole
  # below. SYNCED from xtask (webhook_cert_secret_rules).
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
{{- if and (eq .Values.installScope "namespaced") (eq (include "kopiur.webhook.selfManaged" .) "true") }}
---
# Webhook configurations are cluster-scoped, so even a namespaced install needs a
# (tightly resourceName-scoped) ClusterRole to inject their caBundle in self mode.
# This is the ONLY cluster-level grant a namespaced install carries.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.fullname" . }}-webhook-cert
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
  - apiGroups: ["admissionregistration.k8s.io"]
    resources:
      - validatingwebhookconfigurations
      - mutatingwebhookconfigurations
    resourceNames:
      - {{ include "kopiur.fullname" . }}-validating
      - {{ include "kopiur.fullname" . }}-mutating
    verbs: [get, patch]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: {{ include "kopiur.fullname" . }}-webhook-cert
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: {{ include "kopiur.fullname" . }}-webhook-cert
subjects:
  - kind: ServiceAccount
    name: {{ include "kopiur.serviceAccountName" . }}
    namespace: {{ .Release.Namespace }}
{{- end }}
