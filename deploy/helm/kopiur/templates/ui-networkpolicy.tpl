{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.networkPolicy.enabled -}}
# What makes "only the proxy can set identity headers" true at the network layer
# rather than only on paper. Without it, any pod in the cluster can send
# X-Forwarded-User straight to the Service; the proxy shared secret is the other
# half of that defence, and this is the half that does not depend on a secret
# staying secret.
#
# Deliberately no CIDR allow-list: pod IPs are ephemeral and reassigned, so a
# CIDR rule in a cluster is a comment, not a control. Selectors only.
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: {{ include "kopiur.ui.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
spec:
  podSelector:
    matchLabels:
      {{- include "kopiur.ui.selectorLabels" . | nindent 6 }}
  # Ingress only. Egress is deliberately unrestricted: the console must reach
  # the apiserver, and pinning that endpoint in a chart breaks on every cluster
  # whose control plane is not where this guessed.
  policyTypes: [Ingress]
  ingress:
    # The app port: your authenticating proxy, and nothing else.
    - ports:
        - port: http
          protocol: TCP
      from:
        {{- toYaml (list .Values.ui.networkPolicy.proxySelector) | nindent 8 }}
    # The ops port: /metrics and the probes.
    - ports:
        - port: ops
          protocol: TCP
      {{- with .Values.ui.networkPolicy.monitoringSelector }}
      from:
        {{- toYaml (list .) | nindent 8 }}
      {{- else }}
      # No monitoringSelector set: the ops port stays reachable from anywhere in
      # the cluster. It serves no user data and no identity-bearing response, and
      # the kubelet's probes must reach it from the node — which a namespace or
      # pod selector cannot express.
      {{- end }}
{{- end }}
