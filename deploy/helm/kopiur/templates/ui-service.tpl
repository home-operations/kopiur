{{- include "kopiur.ui.validate" . -}}
{{- if .Values.ui.enabled -}}
# ClusterIP only, and NO Ingress — the chart never renders one, in any mode.
# Same posture as the operator's kopia server surface: kopiur does not own your
# edge. Point your authenticating proxy at this Service, and put your own
# Ingress/HTTPRoute in front of the PROXY. An Ingress pointed straight here
# would expose a console that trusts identity headers to whoever can set them.
apiVersion: v1
kind: Service
metadata:
  name: {{ include "kopiur.ui.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
    {{- with .Values.ui.service.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- with .Values.ui.service.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  type: {{ .Values.ui.service.type }}
  selector:
    {{- include "kopiur.ui.selectorLabels" . | nindent 4 }}
  ports:
    # The SPA and /api. Restrict this one to your proxy (ui.networkPolicy).
    - name: http
      port: {{ .Values.ui.port }}
      targetPort: http
      protocol: TCP
    # /metrics, /healthz, /readyz. A separate port so probes and Prometheus stay
    # reachable while the app port is firewalled, and so /metrics never sits
    # behind the identity middleware.
    - name: ops
      port: {{ .Values.ui.opsPort }}
      targetPort: ops
      protocol: TCP
{{- end }}
