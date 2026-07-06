{{- if .Values.monitoring.prometheusRule.enabled -}}
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: {{ include "kopiur.controller.fullname" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: controller
    {{- with .Values.monitoring.prometheusRule.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  groups:
    - name: kopiur.rules
      rules:
        - alert: KopiurBackupConsecutiveFailures
          expr: kopiur_snapshot_consecutive_failures >= 3
          for: 15m
          labels:
            severity: warning
          annotations:
            summary: "Backups for {{`{{ $labels.namespace }}/{{ $labels.name }}`}} are failing"
            description: "{{`{{ $value }}`}} consecutive backup failures (>=3) for SnapshotPolicy {{`{{ $labels.namespace }}/{{ $labels.name }}`}}."
        - alert: KopiurBackupStale
          # kopiur_policy_last_backup_success_timestamp_seconds is store-backed: its
          # series exists only while a Succeeded Snapshot CR for that (namespace,
          # policy) still exists. That means `time() - metric > threshold` alone goes
          # silent — no data, not an alert — for a policy that has never succeeded, or
          # whose Succeeded Snapshot CRs were all deleted (e.g. pruned by retention
          # while every recent run has been failing): exactly the case that most needs
          # to page. kopiur_snapshot_consecutive_failures{namespace,name} is a sync
          # gauge scoped to the SnapshotPolicy itself (not store-backed like the
          # Snapshot-derived families), so it keeps reporting even when every one of
          # the policy's Snapshot CRs is gone, making it the presence/liveness signal
          # for the second branch. Its label is `name` (the SnapshotPolicy name); the
          # new family labels the same value `policy`, hence the label_replace to join
          # on (namespace, policy) via `unless`.
          expr: >-
            (
              time() - kopiur_policy_last_backup_success_timestamp_seconds > {{ .Values.monitoring.prometheusRule.backupStaleAfterSeconds }}
            )
            or
            (
              label_replace(kopiur_snapshot_consecutive_failures, "policy", "$1", "name", "(.*)") > 0
              unless on (namespace, policy) kopiur_policy_last_backup_success_timestamp_seconds
            )
          for: 30m
          labels:
            severity: warning
          annotations:
            summary: "No recent successful backup for {{`{{ $labels.namespace }}/{{ $labels.policy }}`}}"
            description: "Either the last successful backup was over {{ div .Values.monitoring.prometheusRule.backupStaleAfterSeconds 3600 }}h ago, or SnapshotPolicy {{`{{ $labels.namespace }}/{{ $labels.policy }}`}} has never succeeded (or its Succeeded Snapshot CRs were all deleted) and is currently failing."
        - alert: KopiurRepositoryNotReady
          # Active-only emission means kopiur_resource_phase never carries a 0-valued
          # series, so `== 1` is redundant here; left in place because max-by over a
          # regex phase selector reads more obviously as a boolean gate with it, and
          # dropping it would change nothing observable.
          expr: max by (namespace, name) (kopiur_resource_phase{kind=~"Repository|ClusterRepository", phase=~"Degraded|Failed"}) == 1
          for: 15m
          labels:
            severity: critical
          annotations:
            summary: "Repository {{`{{ $labels.namespace }}/{{ $labels.name }}`}} is {{`{{ $labels.phase }}`}}"
            description: "A kopiur repository has been Degraded/Failed for 15m; backups to it will not run."
        - alert: KopiurSnapshotFailed
          expr: max by (namespace, name) (kopiur_resource_phase{kind="Snapshot", phase="Failed"}) == 1
          for: 10m
          labels:
            severity: warning
          annotations:
            summary: "Snapshot {{`{{ $labels.namespace }}/{{ $labels.name }}`}} failed"
            description: "A Snapshot CR has been in phase=Failed for 10m."
        - alert: KopiurRestoreFailed
          expr: max by (namespace, name) (kopiur_resource_phase{kind="Restore", phase="Failed"}) == 1
          for: 10m
          labels:
            severity: warning
          annotations:
            summary: "Restore {{`{{ $labels.namespace }}/{{ $labels.name }}`}} failed"
            description: "A Restore CR has been in phase=Failed for 10m."
        - alert: KopiurReconcileErrorsHigh
          expr: sum by (kind) (rate(kopiur_controller_reconcile_errors_total[10m])) > 0.2
          for: 15m
          labels:
            severity: warning
          annotations:
            summary: "High reconcile error rate for {{`{{ $labels.kind }}`}}"
            description: "kopiur controller is erroring on {{`{{ $labels.kind }}`}} reconciles (>0.2/s over 10m)."
{{- end }}
