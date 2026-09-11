{{- include "kopiur.ui.validate" . -}}
{{- if .Values.ui.enabled -}}
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: {{ include "kopiur.ui.fullname" . }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: {{ include "kopiur.ui.fullname" . }}
subjects:
  - kind: ServiceAccount
    name: {{ include "kopiur.ui.serviceAccountName" . }}
    namespace: {{ .Release.Namespace }}
{{- end }}
