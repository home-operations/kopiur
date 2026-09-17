{{- if and (eq .Values.installScope "namespaced") .Values.rbacNamespaceReadForStreamSources -}}
# Supplementary cluster-scoped read for a NAMESPACED install, required only by
# `stream` sources.
#
# A stream source execs a command inside a workload pod, so the operator must mint
# `pods/exec` for the mover — arbitrary code execution in every pod in that
# namespace, carried on a ServiceAccount that outlives the Job. Kopiur refuses
# unless the namespace carries
# `kopiur.home-operations.com/stream-exec-movers=true`, and it FAILS CLOSED when
# it cannot read the Namespace to check (see
# `io::mover::namespace_stream_exec_opt_in`). A namespaced install's Role cannot
# reach a cluster-scoped resource, so without this ClusterRole every stream
# source is refused permanently.
#
# Deliberately `get` ONLY, on `namespaces` ONLY — the opt-in check is a single
# GET. It does NOT include list/watch (which the cluster-scoped ClusterRole has
# for its Namespace informer; a namespaced install runs no such informer).
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.fullname" . }}-namespace-read
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
rules:
  - apiGroups: [""]
    resources:
      - namespaces
    verbs: [get]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: {{ include "kopiur.fullname" . }}-namespace-read
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: {{ include "kopiur.fullname" . }}-namespace-read
subjects:
  - kind: ServiceAccount
    name: {{ include "kopiur.serviceAccountName" . }}
    namespace: {{ .Release.Namespace }}
{{- end }}
