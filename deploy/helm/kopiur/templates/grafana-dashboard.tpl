{{- if and .Values.monitoring.dashboards.enabled (not .Values.monitoring.dashboards.grafanaOperator.enabled) -}}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "kopiur.fullname" . }}-dashboard
  namespace: {{ default .Release.Namespace .Values.monitoring.dashboards.namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: dashboard
    {{ .Values.monitoring.dashboards.label }}: {{ .Values.monitoring.dashboards.labelValue | quote }}
    {{- with .Values.monitoring.dashboards.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- if or .Values.monitoring.dashboards.folderAnnotation .Values.monitoring.dashboards.annotations }}
  annotations:
    {{- with .Values.monitoring.dashboards.folderAnnotation }}
    {{ . }}: {{ $.Values.monitoring.dashboards.folder | quote }}
    {{- end }}
    {{- with .Values.monitoring.dashboards.annotations }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- end }}
data:
  kopiur.json: |-
    {{- .Files.Get "files/dashboards/kopiur.json" | nindent 4 }}
{{- end }}
