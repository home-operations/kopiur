{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.rbac.userRoles -}}
# The HUMAN roles, SYNCED from `cargo xtask gen-rbac` (deploy/rbac/ui.yaml).
#
# The chart creates no bindings: who may look at and operate your backups is not
# a chart decision. Bind `<release>-ui-user` to your people with your own
# ClusterRoleBinding (or a namespaced RoleBinding to scope them to one
# namespace's objects).
#
# Note what is NOT here: the file browser. `<release>-ui-browse` is a separate,
# unaggregated role because granting it grants that namespace's repository
# credentials — see ui-browse-clusterrole.tpl.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.ui.fullname" . }}-viewer
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
    rbac.kopiur.home-operations.com/aggregate-to-ui-user: "true"
rules:
  - apiGroups: [kopiur.home-operations.com]
    resources:
      - repositories
      - snapshotpolicies
      - snapshots
      - snapshotschedules
      - restores
      - maintenances
      - repositoryreplications
      - snapshotreplications
      - clusterrepositories
    verbs: [get, list, watch]
  # Doctor compares the installed CRD schemas with the ones this build expects.
  - apiGroups: [apiextensions.k8s.io]
    resources: [customresourcedefinitions]
    verbs: [get, list]
  # The per-object event feed. `events.k8s.io/v1` is the group kube's Recorder
  # writes, so the legacy core group is deliberately not granted.
  - apiGroups: [events.k8s.io]
    resources: [events]
    verbs: [list]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.ui.fullname" . }}-editor
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
    rbac.kopiur.home-operations.com/aggregate-to-ui-user: "true"
# Exactly the verbs behind the console's buttons, and nothing else. No `update`
# anywhere — every console write is a merge patch — and no read the viewer role
# does not already carry, so the pair is additive by construction.
rules:
  # "Snapshot now" and "delete snapshot".
  - apiGroups: [kopiur.home-operations.com]
    resources: [snapshots]
    verbs: [create, delete]
  # The restore dialog.
  - apiGroups: [kopiur.home-operations.com]
    resources: [restores]
    verbs: [create]
  # Suspend/resume, run maintenance, run replication, scan catalog.
  - apiGroups: [kopiur.home-operations.com]
    resources:
      - snapshotpolicies
      - snapshotschedules
      - repositories
      - clusterrepositories
      - maintenances
      - repositoryreplications
      - snapshotreplications
    verbs: [patch]
  # Expanding a policy's pvcSelector in the snapshot-now dialog.
  - apiGroups: [""]
    resources: [persistentvolumeclaims]
    verbs: [list]
---
# The role to actually bind. Its rules come from the aggregation above, so it
# ships with none of its own — bind this one and the apiserver keeps it current
# as the chart's roles change.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.ui.fullname" . }}-user
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
aggregationRule:
  clusterRoleSelectors:
    - matchLabels:
        rbac.kopiur.home-operations.com/aggregate-to-ui-user: "true"
{{- end }}
