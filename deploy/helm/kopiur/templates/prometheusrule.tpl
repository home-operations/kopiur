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
          # to page. kopiur_snapshot_consecutive_failures{namespace,name} is the
          # liveness signal for the second branch: since #345 M6 it is store-backed
          # too (derived from the policy's Snapshot CRs), which changes series
          # PRESENCE (it now vanishes with the policy's last Snapshot CR) but not
          # this branch's coverage — a streak > 0 was always computed from retained
          # Failed Snapshot CRs (the old sync gauge LISTed the same population), so
          # whenever the branch's `> 0` filter could match, the series exists. It
          # keeps reporting after every SUCCEEDED CR was pruned, the case this
          # branch is for. Its label is `name` (the SnapshotPolicy name); the new
          # family labels the same value `policy`, hence the label_replace to join
          # on (namespace, policy) via `unless`.
          #
          # This alert stays THE loud staleness signal while a repository circuit
          # breaker (#345) holds backups paused: gated Snapshots park Pending (no
          # new terminal runs), so the last-success age keeps growing until it
          # crosses the threshold. KopiurRepositoryBreakerOpen / KopiurSnapshotsGated
          # below explain the pause earlier; they do not replace this alert.
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
        - alert: KopiurLastBackupFailed
          # Policy-level, recovery-aware (#280): fires while a SnapshotPolicy's
          # MOST RECENT completed backup failed, and auto-resolves the moment a
          # newer backup succeeds (VolSync-style). Store-backed: the series
          # exists only while the policy's terminal Snapshot CRs exist —
          # KopiurBackupStale covers the all-CRs-pruned / never-succeeded case.
          # Aggregating across scrape targets fixes alert-identity churn when
          # pod replacement briefly overlaps two targets; min-vs-max decides who
          # wins while they DISAGREE. min errs toward the failure signal: a
          # deposed/wedged pod's frozen 1 must never mask a real failure (the
          # missed-alert direction is data-loss-shaped), while the converse
          # transient — a stale 0 against a fresh 1 — is evicted by scrape
          # staleness (~5m) before `for: 10m` completes, so it never pages.
          expr: min by (namespace, policy) (kopiur_snapshotpolicy_last_backup_success) == 0
          for: 10m
          labels:
            severity: warning
          annotations:
            summary: "Latest backup for {{`{{ $labels.namespace }}/{{ $labels.policy }}`}} failed"
            description: "The most recent completed backup for SnapshotPolicy {{`{{ $labels.namespace }}/{{ $labels.policy }}`}} failed. Resolves automatically once a newer backup succeeds."
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
          # Per-CR, gated on recovery (#280): failed Snapshot CRs are retained
          # by design, so an ungated phase match would page forever for CRs
          # whose policy has long since recovered. The `unless` drops every
          # failed CR whose policy's LATEST completed backup succeeded.
          # Snapshots without a policyRef have no `policy` label, never match
          # the health series, and so keep the always-fire behavior — for an
          # ad-hoc backup there is no "newer success" to defer to.
          # The RHS must aggregate with min: an `unless` only checks PRESENCE of
          # the on-label signature, so any lone series with value 1 — e.g. a
          # deposed leader's stale samples — would suppress a real failure.
          # min-by demands unanimity across scrape targets before suppressing;
          # the converse stale-0 transient is evicted by scrape staleness (~5m)
          # before `for: 10m` completes.
          expr: >-
            max by (namespace, name, policy) (kopiur_resource_phase{kind="Snapshot", phase="Failed"}) == 1
            unless on (namespace, policy) (min by (namespace, policy) (kopiur_snapshotpolicy_last_backup_success) == 1)
          for: 10m
          labels:
            severity: warning
          annotations:
            summary: "Snapshot {{`{{ $labels.namespace }}/{{ $labels.name }}`}} failed"
            description: "Snapshot {{`{{ $labels.namespace }}/{{ $labels.name }}`}} is Failed and its policy has not completed a newer successful backup."
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
        # Leader-election health (issue #319). A leader that keeps losing and
        # re-taking the Lease reconciles almost nothing: each transition costs a
        # full informer resync, and while it resyncs, schedules do not fire.
        # Before these metrics existed this was only visible by grepping logs.
        - alert: KopiurLeaderElectionFlapping
          # One transition per process start is normal; more than two in an hour
          # is not a rolling upgrade, it is a leader that cannot hold the Lease.
          expr: increase(kopiur_leader_transitions_total[1h]) > 2
          for: 5m
          labels:
            severity: warning
          annotations:
            summary: "kopiur leader election is flapping"
            description: "The kopiur controller has acquired the leader Lease {{`{{ $value }}`}} times in the last hour. Each transition costs a full informer resync, so reconciliation is stalling repeatedly. Check kopiur_leader_renew_duration_seconds and kopiur_leader_renew_failures_total for whether the API server, the network, or APF queueing is the cause."
        - alert: KopiurLeaderRenewSlow
          # The renew window is 10s. A p99 above 2s means the election is running
          # on a quarter of its budget — the leading indicator of the flapping
          # alert above, and the one worth catching first.
          expr: histogram_quantile(0.99, sum by (le) (rate(kopiur_leader_renew_duration_seconds_bucket[10m]))) > 2
          for: 15m
          labels:
            severity: warning
          annotations:
            summary: "kopiur leader Lease renewals are slow"
            description: "p99 Lease renew latency is {{`{{ $value }}`}}s against a 10s renew window. Healthy is single-digit milliseconds. If the API server is otherwise fine, the operator's lease traffic is most likely queueing in API Priority and Fairness — check that the kopiur FlowSchema exists (leaderElection.flowSchema.enabled)."
        - alert: KopiurRepositoryBreakerOpen
          # The repository circuit breaker (#345) is open: the backend health probe
          # exceeded spec.health.probe.failureThreshold under onFailure: Degrade, so
          # the repository moved to Degraded and stays there STABLY while open (M4's
          # launch-phase guarantee — no Initializing flap), which is what makes the
          # `for:` clause meaningful. Narrower than KopiurRepositoryNotReady (which
          # also matches Failed and pages critical): this one names the breaker
          # specifically and explains that the pause is deliberate and self-healing.
          # max-by for scrape-target-overlap identity hygiene, matching the other
          # phase alerts; `kind` is kept so the description can point kubectl at the
          # right resource.
          expr: max by (kind, namespace, name) (kopiur_resource_phase{kind=~"Repository|ClusterRepository", phase="Degraded"}) == 1
          for: 15m
          labels:
            severity: warning
          annotations:
            summary: "Repository circuit breaker open for {{`{{ $labels.kind }}`}} {{`{{ $labels.namespace }}`}}/{{`{{ $labels.name }}`}}"
            description: "The backend health probe kept failing past its threshold, so kopiur opened the circuit breaker: backups, maintenance, and replication against this repository are paused (Snapshots park Pending, they are not lost). Recovery is automatic once a connect succeeds — nothing needs restarting. Run `kubectl describe {{`{{ $labels.kind }}`}} {{`{{ $labels.name }}`}}` and check the BackendReachable condition for the cause."
        - alert: KopiurSnapshotsGated
          # Sustained parked backups: Snapshots held Pending behind a not-Ready
          # repository (Ready reason RepositoryNotReady). These are deferrals, not
          # refusals — they launch automatically on recovery — so this is
          # informational context alongside KopiurRepositoryBreakerOpen, and
          # KopiurBackupStale remains the escalating signal if the pause drags on.
          # kopiur_snapshot_gated is store-backed (series absent when nothing is
          # parked), so > 0 is a pure presence-and-magnitude check.
          expr: sum by (namespace) (kopiur_snapshot_gated) > 0
          for: 30m
          labels:
            severity: info
          annotations:
            summary: "Backups in {{`{{ $labels.namespace }}`}} are parked behind a not-Ready repository"
            description: "{{`{{ $value }}`}} Snapshot(s) in {{`{{ $labels.namespace }}`}} have been held Pending for 30m because their repository is not Ready (see KopiurRepositoryBreakerOpen / the repository's BackendReachable condition). They will launch automatically once the repository recovers."
{{- end }}
