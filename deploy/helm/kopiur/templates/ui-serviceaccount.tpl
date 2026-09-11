{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.serviceAccount.create -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include "kopiur.ui.serviceAccountName" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
  {{- with .Values.ui.serviceAccount.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
automountServiceAccountToken: {{ .Values.ui.serviceAccount.automount }}
{{- end }}
