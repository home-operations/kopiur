{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.rbac.userRoles -}}
{{- $root := . -}}
{{- range $i, $b := .Values.ui.rbac.bindings }}
---
# From ui.rbac.bindings[{{ $i }}]: what these people may do in the console.
# Opt-in by design — the chart binds nobody unless you list them here.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: {{ include "kopiur.ui.fullname" $root }}-{{ $b.role }}-{{ $b.name | default $i }}
  labels:
    {{- include "kopiur.labels" $root | nindent 4 }}
    app.kubernetes.io/component: ui
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: {{ include "kopiur.ui.fullname" $root }}-{{ $b.role }}
subjects:
  {{- range $b.groups }}
  - apiGroup: rbac.authorization.k8s.io
    kind: Group
    name: {{ . | quote }}
  {{- end }}
  {{- range $b.users }}
  - apiGroup: rbac.authorization.k8s.io
    kind: User
    name: {{ . | quote }}
  {{- end }}
{{- end }}
{{- end }}
