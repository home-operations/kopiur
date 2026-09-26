{{- if eq .Values.installScope "namespaced" -}}
# Least-privilege mover Role (namespaced-install mode). RBAC rules SYNCED
# from `cargo xtask gen-rbac` (deploy/rbac/mover-role.yaml) — that xtask is the
# SOURCE OF TRUTH; edit it and re-run, then re-sync these rules.
#
# Same minimal rules as the cluster-scoped mover ClusterRole MINUS
# `clusterrepositories/status`: the operator's Role can never hold a
# cluster-scoped kind, so an entry here would trip RBAC escalation prevention
# on the controller's runtime RoleBinding mint and block EVERY mover.
# (ClusterRepository is only reconciled in installScope=cluster anyway.) The
# controller mints the `kopiur-mover` ServiceAccount + a RoleBinding to this
# Role in the workload namespace at runtime.
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: {{ include "kopiur.moverName" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
  - apiGroups:
      - kopiur.home-operations.com
    resources:
      - snapshots/status
      - restores/status
      - repositories/status
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
# Dedicated snapshot-replication mover Role (namespaced-install mode, issue
# #368). RBAC rules SYNCED from `cargo xtask gen-rbac`
# (deploy/rbac/mover-role.yaml, second document) — that xtask is the SOURCE OF
# TRUTH; edit it and re-run, then re-sync these rules.
#
# Same rules as its cluster-scoped sibling (everything the snapshot-replication
# mover touches is namespaced, so no escalation-prevention carve-out applies).
# See clusterrole-mover.tpl for why these verbs live on a SEPARATE role.
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: {{ include "kopiur.snapshotReplicationMoverName" . }}
  namespace: {{ .Release.Namespace }}
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
---
# Dedicated stream-source mover Role. RBAC rules SYNCED from
# `cargo xtask gen-rbac` (deploy/rbac/mover-role.yaml, third document) —
# that xtask is the SOURCE OF TRUTH; edit it and re-run, then re-sync these rules.
#
# A `stream` source execs a command in a running workload pod, which needs
# `pods/exec`. That verb must NEVER reach the generic mover role above: every
# ordinary mover Job in the namespace runs as that ServiceAccount, so granting it
# there would let any backup Job run arbitrary commands in any pod in the
# namespace. The controller mints the same-named ServiceAccount + a RoleBinding to
# THIS role per namespace, only for stream-source mover Jobs.
#
# `resourceNames` cannot narrow `pods/exec` further — the pod name is not known
# until the selector resolves at run time, and RBAC has no label-selector form. The
# effective bound is the namespace, which is why stream sources additionally
# require a cluster-admin namespace opt-in annotation.
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: {{ include "kopiur.streamMoverName" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
  - apiGroups:
      - kopiur.home-operations.com
    resources:
      - snapshots/status
      - restores/status
      - repositories/status
      - maintenances/status
      - snapshotpolicies/status
      - repositoryreplications/status
      - snapshotreplications/status
    verbs: [get, patch]
  - apiGroups: [""]
    resources:
      - configmaps
    verbs: [get, patch]
  - apiGroups: [""]
    resources:
      - pods
    verbs: [get, list]
  - apiGroups: [""]
    resources:
      - pods/exec
    verbs: [create, get]
{{- end }}
