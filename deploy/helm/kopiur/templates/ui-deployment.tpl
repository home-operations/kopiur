{{- include "kopiur.ui.validate" . -}}
{{- if .Values.ui.enabled -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "kopiur.ui.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
spec:
  replicas: {{ .Values.ui.replicaCount }}
  selector:
    matchLabels:
      {{- include "kopiur.ui.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "kopiur.ui.selectorLabels" . | nindent 8 }}
        {{- with .Values.ui.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with .Values.ui.podAnnotations }}
      annotations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
    spec:
      serviceAccountName: {{ include "kopiur.ui.serviceAccountName" . }}
      # Required, not merely conventional: the projected token IS how the
      # console authenticates in order to impersonate.
      automountServiceAccountToken: {{ .Values.ui.serviceAccount.automount }}
      {{- include "kopiur.imagePullSecrets" . | nindent 6 }}
      {{- with .Values.ui.priorityClassName }}
      priorityClassName: {{ . }}
      {{- end }}
      securityContext:
        {{- toYaml .Values.ui.podSecurityContext | nindent 8 }}
      containers:
        - name: ui
          image: {{ include "kopiur.image" (dict "root" $ "img" .Values.ui.image) }}
          imagePullPolicy: {{ .Values.ui.image.pullPolicy }}
          env:
            {{- include "kopiur.ui.env" . | nindent 12 }}
          ports:
            - name: http
              containerPort: {{ .Values.ui.port }}
              protocol: TCP
            - name: ops
              containerPort: {{ .Values.ui.opsPort }}
              protocol: TCP
          {{- with .Values.ui.livenessProbe }}
          livenessProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          {{- with .Values.ui.readinessProbe }}
          readinessProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          resources:
            {{- toYaml .Values.ui.resources | nindent 12 }}
          securityContext:
            {{- toYaml .Values.ui.securityContext | nindent 12 }}
          {{- if or .Values.ui.auth.proxySecret.existingSecret .Values.ui.tls.existingSecret }}
          volumeMounts:
            {{- if .Values.ui.auth.proxySecret.existingSecret }}
            # The proxy shared secret is MOUNTED, never read through the
            # apiserver: the console's ServiceAccount has no `secrets` grant at
            # all, and adding one to save a volume would be the single change
            # that turns "can impersonate" into "can read every credential".
            - name: proxy-secret
              mountPath: /etc/kopiur-ui/proxy
              readOnly: true
            {{- end }}
            {{- if .Values.ui.tls.existingSecret }}
            - name: tls
              mountPath: /etc/kopiur-ui/tls
              readOnly: true
            {{- end }}
          {{- end }}
      {{- if or .Values.ui.auth.proxySecret.existingSecret .Values.ui.tls.existingSecret }}
      volumes:
        {{- if .Values.ui.auth.proxySecret.existingSecret }}
        - name: proxy-secret
          secret:
            secretName: {{ .Values.ui.auth.proxySecret.existingSecret }}
        {{- end }}
        {{- if .Values.ui.tls.existingSecret }}
        - name: tls
          secret:
            secretName: {{ .Values.ui.tls.existingSecret }}
        {{- end }}
      {{- end }}
      {{- with (include "kopiur.nodeSelector" (dict "root" $ "component" .Values.ui.nodeSelector)) }}
      nodeSelector:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.affinity" (dict "root" $ "component" .Values.ui.affinity)) }}
      affinity:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.tolerations" (dict "root" $ "component" .Values.ui.tolerations)) }}
      tolerations:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.topologySpreadConstraints" (dict "root" $ "component" .Values.ui.topologySpreadConstraints)) }}
      topologySpreadConstraints:
        {{- . | nindent 8 }}
      {{- end }}
{{- end }}
