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
    verbs: [get, patch]
  - apiGroups: [""]
    resources:
      - configmaps
    verbs: [get, patch]
{{- end }}
