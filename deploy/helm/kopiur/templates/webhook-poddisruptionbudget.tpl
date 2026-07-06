{{- if and .Values.webhook.enabled .Values.webhook.podDisruptionBudget.enabled -}}
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "kopiur.webhook.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: webhook
spec:
  minAvailable: {{ .Values.webhook.podDisruptionBudget.minAvailable }}
  selector:
    matchLabels:
      {{- include "kopiur.webhook.selectorLabels" . | nindent 6 }}
{{- end }}
