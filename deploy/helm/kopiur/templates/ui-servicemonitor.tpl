{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.serviceMonitor.enabled -}}
# Scrapes the OPS port. `kopiur_ui_requests_total{identity_source}` is the metric
# worth an alert: it is what makes an anonymous-fallback downgrade visible
# instead of silent.
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: {{ include "kopiur.ui.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
    {{- with .Values.ui.serviceMonitor.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  selector:
    matchLabels:
      {{- include "kopiur.ui.selectorLabels" . | nindent 6 }}
  namespaceSelector:
    matchNames:
      - {{ .Release.Namespace }}
  endpoints:
    - port: ops
      path: /metrics
      interval: {{ .Values.ui.serviceMonitor.interval }}
      scrapeTimeout: {{ .Values.ui.serviceMonitor.scrapeTimeout }}
      {{- with .Values.ui.serviceMonitor.relabelings }}
      relabelings:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.ui.serviceMonitor.metricRelabelings }}
      metricRelabelings:
        {{- toYaml . | nindent 8 }}
      {{- end }}
{{- end }}
