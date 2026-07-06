{{- if and .Values.monitoring.dashboards.enabled .Values.monitoring.dashboards.grafanaOperator.enabled -}}
apiVersion: grafana.integreatly.org/v1beta1
kind: GrafanaDashboard
metadata:
  name: {{ include "kopiur.fullname" . }}-dashboard
  namespace: {{ default .Release.Namespace .Values.monitoring.dashboards.namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: dashboard
    {{- with .Values.monitoring.dashboards.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- with .Values.monitoring.dashboards.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  allowCrossNamespaceImport: {{ .Values.monitoring.dashboards.grafanaOperator.allowCrossNamespaceImport }}
  resyncPeriod: {{ .Values.monitoring.dashboards.grafanaOperator.resyncPeriod | quote }}
  {{- with .Values.monitoring.dashboards.grafanaOperator.folder }}
  folder: {{ . | quote }}
  {{- end }}
  instanceSelector:
    matchLabels:
      {{- toYaml .Values.monitoring.dashboards.grafanaOperator.matchLabels | nindent 6 }}
  json: |-
    {{- .Files.Get "files/dashboards/kopiur.json" | nindent 4 }}
{{- end }}
