{{- if .Values.webhook.enabled -}}
{{- $mode := .Values.webhook.tls.mode | default "self" -}}
{{- if not (has $mode (list "self" "cert-manager" "manual")) -}}
{{- fail (printf "webhook.tls.mode must be one of self|cert-manager|manual, got %q" $mode) -}}
{{- end -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "kopiur.webhook.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: webhook
spec:
  replicas: {{ .Values.webhook.replicaCount }}
  selector:
    matchLabels:
      {{- include "kopiur.webhook.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "kopiur.webhook.selectorLabels" . | nindent 8 }}
        {{- with .Values.webhook.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with .Values.webhook.podAnnotations }}
      annotations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
    spec:
      serviceAccountName: {{ include "kopiur.serviceAccountName" . }}
      automountServiceAccountToken: {{ .Values.serviceAccount.automount }}
      {{- include "kopiur.imagePullSecrets" . | nindent 6 }}
      {{- with .Values.webhook.priorityClassName }}
      priorityClassName: {{ . }}
      {{- end }}
      securityContext:
        {{- toYaml .Values.webhook.podSecurityContext | nindent 8 }}
      containers:
        - name: webhook
          image: {{ include "kopiur.image" (dict "root" $ "img" .Values.webhook.image) }}
          imagePullPolicy: {{ .Values.webhook.image.pullPolicy }}
          env:
            {{- include "kopiur.loggingEnv" . | nindent 12 }}
            # Bind address rendered "[::]:<port>" from webhook.port (dual-stack
            # wildcard); the Service maps 443 -> this port.
            - name: KOPIUR_WEBHOOK_ADDR
              value: {{ printf "[::]:%v" .Values.webhook.port | quote }}
            - name: KOPIUR_WEBHOOK_TLS_CERT
              value: /tls/tls.crt
            - name: KOPIUR_WEBHOOK_TLS_KEY
              value: /tls/tls.key
            {{- include "kopiur.otlpEnv" . | nindent 12 }}
            {{- with .Values.webhook.extraEnv }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
          ports:
            - name: https
              containerPort: {{ .Values.webhook.port }}
              protocol: TCP
          {{- with .Values.webhook.livenessProbe }}
          livenessProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          {{- with .Values.webhook.readinessProbe }}
          readinessProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          resources:
            {{- toYaml .Values.webhook.resources | nindent 12 }}
          securityContext:
            {{- toYaml .Values.webhook.securityContext | nindent 12 }}
          volumeMounts:
            - name: tls
              mountPath: /tls
              readOnly: true
      volumes:
        - name: tls
          secret:
            secretName: {{ .Values.webhook.tls.secretName }}
      {{- with (include "kopiur.nodeSelector" (dict "root" $ "component" .Values.webhook.nodeSelector)) }}
      nodeSelector:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.affinity" (dict "root" $ "component" .Values.webhook.affinity)) }}
      affinity:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.tolerations" (dict "root" $ "component" .Values.webhook.tolerations)) }}
      tolerations:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.topologySpreadConstraints" (dict "root" $ "component" .Values.webhook.topologySpreadConstraints)) }}
      topologySpreadConstraints:
        {{- . | nindent 8 }}
      {{- end }}
{{- end }}
