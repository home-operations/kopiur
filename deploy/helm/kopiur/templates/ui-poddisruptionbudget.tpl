{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.podDisruptionBudget.enabled -}}
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "kopiur.ui.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
spec:
  minAvailable: {{ .Values.ui.podDisruptionBudget.minAvailable }}
  selector:
    matchLabels:
      {{- include "kopiur.ui.selectorLabels" . | nindent 6 }}
{{- end }}
