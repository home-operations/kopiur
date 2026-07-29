{{- if and .Values.leaderElection.enabled .Values.leaderElection.flowSchema.enabled -}}
# API Priority and Fairness lane for the election Lease (issue #319).
#
# Kubernetes ships `system-leader-election` (matchingPrecedence 100) pointing at
# the `leader-election` PriorityLevelConfiguration — a lane with
# `lendablePercent: 0`, so its concurrency is guaranteed and never lent away.
# That FlowSchema matches ONLY system:kube-controller-manager,
# system:kube-scheduler, and ServiceAccounts in the kube-system namespace.
#
# An operator running in its own namespace therefore falls through to the
# built-in `service-accounts` schema (precedence 9000) -> `workload-low`
# (`lendablePercent: 90`), queueing its lease renewals behind every other
# ServiceAccount's bulk traffic in the cluster. Measured on one production
# cluster over 7 days, p99 APF queue wait: `leader-election` 0.005s vs
# `workload-low` 1.82s.
#
# This puts Kopiur's Lease traffic — and ONLY its Lease traffic — into the same
# protected lane the built-in controllers get. Precedence 200: after the
# built-in system-leader-election (100), ahead of everything else.
#
# Disable with `leaderElection.flowSchema.enabled: false` if APF is turned off
# on the cluster, or if the installing user cannot create cluster-scoped
# flowcontrol.apiserver.k8s.io objects. Kopiur runs correctly without it; the
# renew loop tolerates a congested lane, it just has less headroom.
apiVersion: flowcontrol.apiserver.k8s.io/v1
kind: FlowSchema
metadata:
  name: {{ include "kopiur.fullname" . }}-leader-election
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
spec:
  priorityLevelConfiguration:
    # The built-in guaranteed lane; not created by this chart.
    name: leader-election
  matchingPrecedence: {{ .Values.leaderElection.flowSchema.matchingPrecedence }}
  distinguisherMethod:
    type: ByUser
  rules:
    - resourceRules:
        - apiGroups: ["coordination.k8s.io"]
          resources: ["leases"]
          # The three verbs the election protocol uses. Deliberately NOT a
          # blanket grant: only election traffic belongs in this lane, and
          # widening it would let bulk work borrow the guaranteed concurrency
          # that exists to keep leases renewable under load.
          verbs: ["get", "create", "update"]
          namespaces: [{{ .Release.Namespace | quote }}]
      subjects:
        - kind: ServiceAccount
          serviceAccount:
            name: {{ include "kopiur.serviceAccountName" . }}
            namespace: {{ .Release.Namespace }}
{{- end }}
