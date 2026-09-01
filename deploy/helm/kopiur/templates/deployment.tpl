apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "kopiur.controller.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: controller
spec:
  # replicaCount > 1 is HA: the elected leader reconciles, others stand by.
  # Deterministic jitter keeps schedules identical across replicas and failover.
  replicas: {{ .Values.replicaCount }}
  selector:
    matchLabels:
      {{- include "kopiur.controller.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "kopiur.controller.selectorLabels" . | nindent 8 }}
        {{- with .Values.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with .Values.podAnnotations }}
      annotations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
    spec:
      serviceAccountName: {{ include "kopiur.serviceAccountName" . }}
      automountServiceAccountToken: {{ .Values.serviceAccount.automount }}
      {{- include "kopiur.imagePullSecrets" . | nindent 6 }}
      {{- with .Values.priorityClassName }}
      priorityClassName: {{ . }}
      {{- end }}
      securityContext:
        {{- toYaml .Values.podSecurityContext | nindent 8 }}
      containers:
        - name: controller
          image: {{ include "kopiur.image" (dict "root" $ "img" .Values.image) }}
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          args:
            - --leader-elect={{ .Values.leaderElection.enabled }}
            {{- if eq .Values.installScope "cluster" }}
            - --cluster-scope
            {{- else }}
            - --namespace={{ .Release.Namespace }}
            {{- end }}
            {{- with .Values.extraArgs }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
          env:
            {{- include "kopiur.loggingEnv" . | nindent 12 }}
            - name: KOPIUR_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
            {{- if .Values.leaderElection.enabled }}
            # Leader-election Lease name: the release fullname, so two releases
            # sharing a namespace never contend on the same Lease.
            - name: KOPIUR_LEASE_NAME
              value: {{ include "kopiur.fullname" . | quote }}
            {{- with .Values.leaderElection.timings }}
            # Protocol timings. Unset fields keep the client-go defaults; any
            # combination that could split-brain is rejected at startup.
            {{- with .leaseDurationSeconds }}
            - name: KOPIUR_LEASE_DURATION_SECONDS
              value: {{ . | quote }}
            {{- end }}
            {{- with .renewDeadlineSeconds }}
            - name: KOPIUR_RENEW_DEADLINE_SECONDS
              value: {{ . | quote }}
            {{- end }}
            {{- with .renewPeriodSeconds }}
            - name: KOPIUR_RENEW_PERIOD_SECONDS
              value: {{ . | quote }}
            {{- end }}
            {{- with .retryPeriodSeconds }}
            - name: KOPIUR_RETRY_PERIOD_SECONDS
              value: {{ . | quote }}
            {{- end }}
            {{- end }}
            {{- end }}
            # The mover image the controller stamps into every Backup/Restore Job.
            - name: KOPIUR_MOVER_IMAGE
              value: {{ include "kopiur.image" (dict "root" $ "img" .Values.mover.image) | quote }}
            - name: KOPIUR_MOVER_PULL_POLICY
              value: {{ .Values.mover.image.pullPolicy | quote }}
            # The mover PATCHes Backup/Restore .status, so its Job pods run as a
            # dedicated least-privilege ServiceAccount (NOT the operator SA and NOT
            # the namespace default). The controller mints this SA + a RoleBinding to
            # the mover role below in each Job's (workload) namespace, since a mover
            # Job runs in the workload namespace where the operator SA does not exist.
            - name: KOPIUR_MOVER_SERVICE_ACCOUNT
              value: {{ include "kopiur.moverName" . | quote }}
            # Name of the mover ClusterRole/Role (shipped by this chart) that the
            # controller-minted per-namespace RoleBinding references, and its kind:
            # a cluster install shares one ClusterRole bound per namespace; a
            # namespaced install uses a Role in the operator namespace.
            - name: KOPIUR_MOVER_CLUSTERROLE
              value: {{ include "kopiur.moverName" . | quote }}
            - name: KOPIUR_MOVER_ROLE_KIND
              value: {{ eq .Values.installScope "cluster" | ternary "ClusterRole" "Role" | quote }}
            # Runtime footprint controls (see docs/dev/watch-and-reconcile.md): the
            # tokio worker-thread cap (the runtime default sizes to host cores,
            # ignoring the cgroup CPU quota) and the WatchList streaming list.
            - name: KOPIUR_WORKER_THREADS
              value: {{ .Values.workerThreads | quote }}
            {{- if .Values.streamingLists }}
            - name: KOPIUR_STREAMING_LISTS
              value: "true"
            {{- end }}
            # Cap on concurrently running Snapshot-delete BATCH mover Jobs,
            # across every repository (mass-deletion protection). 0 = uncapped
            # (default) — batching is the primary protection; this is an
            # opt-in backstop.
            - name: KOPIUR_MAX_CONCURRENT_DELETE_JOBS
              value: {{ .Values.maxConcurrentDeleteJobs | quote }}
            # Cluster-wide cap on concurrently running POOLED mover Jobs
            # (backups, restores, replication sources) across every
            # repository — the backstop beneath each repository's own
            # spec.concurrency.maxConcurrentJobs. 0 = uncapped (default);
            # restores are always admitted and never queued.
            - name: KOPIUR_MAX_CONCURRENT_JOBS
              value: {{ .Values.maxConcurrentJobs | quote }}
            # Per-controller cap on concurrent reconciles. Bounds apiserver
            # load and fds during re-list storms/outages; 0 = unbounded (the
            # pre-fix behavior; not recommended).
            - name: KOPIUR_RECONCILE_CONCURRENCY
              value: {{ .Values.reconcileConcurrency | quote }}
            # Bind address for the HTTP server (/metrics, /healthz, /readyz),
            # rendered "[::]:<port>" from metrics.port — the dual-stack wildcard
            # serves both IPv4 and IPv6 kubelets.
            - name: KOPIUR_HTTP_ADDR
              value: {{ printf "[::]:%v" .Values.metrics.port | quote }}
            {{- if eq (include "kopiur.webhook.selfManaged" .) "true" }}
            # Self-managed webhook TLS (webhook.tls.mode: self): the controller
            # mints the serving cert into this Secret and injects caBundle into
            # the two webhook configurations below. No cert-manager required.
            - name: KOPIUR_WEBHOOK_TLS_MANAGED
              value: "true"
            - name: KOPIUR_WEBHOOK_SECRET_NAME
              value: {{ .Values.webhook.tls.secretName | quote }}
            - name: KOPIUR_WEBHOOK_SERVICE_NAME
              value: {{ include "kopiur.webhook.fullname" . | quote }}
            - name: KOPIUR_WEBHOOK_VALIDATING_CONFIG
              value: {{ include "kopiur.fullname" . }}-validating
            - name: KOPIUR_WEBHOOK_MUTATING_CONFIG
              value: {{ include "kopiur.fullname" . }}-mutating
            {{- end }}
            {{- include "kopiur.otlpEnv" . | nindent 12 }}
            {{- with .Values.extraEnv }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
          ports:
            - name: metrics
              containerPort: {{ .Values.metrics.port }}
              protocol: TCP
          {{- with .Values.livenessProbe }}
          livenessProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          {{- with .Values.readinessProbe }}
          readinessProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          resources:
            {{- toYaml .Values.resources | nindent 12 }}
          securityContext:
            {{- toYaml .Values.securityContext | nindent 12 }}
          # The controller runs short, in-process kopia ops (repository
          # connect/create). kopia defaults its cache/logs/config under $HOME,
          # which is /nonexistent on distroless:nonroot with a read-only rootfs;
          # this writable emptyDir (matched by KOPIA_* env the binary sets) gives
          # it somewhere to write. Mount path must match
          # kopiur_kopia::env::DEFAULT_CACHE_DIR.
          volumeMounts:
            - name: kopia-cache
              mountPath: /var/cache/kopia
            {{- with .Values.extraVolumeMounts }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
      volumes:
        - name: kopia-cache
          emptyDir: {}
        {{- with .Values.extraVolumes }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with (include "kopiur.nodeSelector" (dict "root" $ "component" .Values.nodeSelector)) }}
      nodeSelector:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.affinity" (dict "root" $ "component" .Values.affinity)) }}
      affinity:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.tolerations" (dict "root" $ "component" .Values.tolerations)) }}
      tolerations:
        {{- . | nindent 8 }}
      {{- end }}
      {{- with (include "kopiur.topologySpreadConstraints" (dict "root" $ "component" .Values.topologySpreadConstraints)) }}
      topologySpreadConstraints:
        {{- . | nindent 8 }}
      {{- end }}
