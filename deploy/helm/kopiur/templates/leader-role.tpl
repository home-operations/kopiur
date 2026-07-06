{{- if and (eq .Values.installScope "cluster") .Values.leaderElection.enabled -}}
# Leader-election RBAC for the CLUSTER install (namespaced installs carry these
# rules in the operator Role instead). The election Lease is namespace-local, so
# this is deliberately a Role+RoleBinding paired with the operator ClusterRole —
# a cluster-wide `leases` grant would let the operator re-stamp node-heartbeat
# Leases or steal other controllers' elections. get/update are resourceName-
# scoped to the one Lease the protocol touches (KOPIUR_LEASE_NAME, the release
# fullname); create cannot be name-scoped but stays namespace-local. RBAC rules
# SYNCED from `cargo xtask gen-rbac` (leader_election_rules).
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: {{ include "kopiur.fullname" . }}-leader-election
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
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
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: {{ include "kopiur.fullname" . }}-leader-election
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: {{ include "kopiur.fullname" . }}-leader-election
subjects:
  - kind: ServiceAccount
    name: {{ include "kopiur.serviceAccountName" . }}
    namespace: {{ .Release.Namespace }}
{{- end }}
