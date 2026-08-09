{{- if eq .Values.installScope "cluster" -}}
# Least-privilege mover ClusterRole. RBAC rules SYNCED from
# `cargo xtask gen-rbac` (deploy/rbac/mover-clusterrole.yaml) — that xtask is the
# SOURCE OF TRUTH; edit it and re-run, then re-sync these rules.
#
# The mover Job runs in the WORKLOAD namespace as a dedicated `kopiur-mover`
# ServiceAccount (NOT the operator SA). The controller mints that SA + a RoleBinding
# to THIS ClusterRole in each Job's namespace at runtime, so the role is shipped once
# (cluster-scoped) and bound per-namespace. The rules are a tiny subset of the
# operator's: the mover only PATCHes the owning CR's `.status` and the bootstrap
# result ConfigMap. Names/labels are Helm-templated so the chart owns them.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.moverName" . }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
  - apiGroups:
      - kopiur.home-operations.com
    resources:
      - snapshots/status
      - restores/status
      - repositories/status
      - clusterrepositories/status
      - maintenances/status
      - snapshotpolicies/status
      - repositoryreplications/status
      - snapshotreplications/status
    verbs: [get, patch]
  - apiGroups: [""]
    resources:
      - configmaps
    verbs: [get, patch]
---
# Dedicated snapshot-replication mover ClusterRole (issue #368). RBAC rules
# SYNCED from `cargo xtask gen-rbac` (deploy/rbac/mover-clusterrole.yaml,
# second document) — that xtask is the SOURCE OF TRUTH; edit it and re-run,
# then re-sync these rules.
#
# The replication mover creates and DELETES Snapshot CRs (copy-CR
# reconciliation + pruning) — verbs the generic mover role above must NEVER
# hold (a compromised generic mover pod holding namespace-wide Snapshot delete
# could erase every backup record in its namespace). The controller mints the
# same-named ServiceAccount + a RoleBinding to THIS role per namespace, only
# for snapshot-replication mover Jobs.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.snapshotReplicationMoverName" . }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
  - apiGroups:
      - kopiur.home-operations.com
    resources:
      - snapshots
    verbs: [get, list, create, patch, delete]
  - apiGroups:
      - kopiur.home-operations.com
    resources:
      - snapshots/status
      - snapshotreplications/status
    verbs: [get, patch]
  - apiGroups: [""]
    resources:
      - configmaps
    verbs: [get, patch]
{{- end }}
