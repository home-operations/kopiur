{{- include "kopiur.ui.validate" . -}}
{{- if .Values.ui.enabled -}}
# RBAC rules SYNCED from `cargo xtask gen-rbac` (deploy/rbac/ui.yaml, the
# `kopiur-ui` ClusterRole). That xtask is the SOURCE OF TRUTH; this template
# renders the same rules with the release's names and narrows them to the
# install's actual configuration.
#
# This is the console's OWN identity — not any user's. It may impersonate, and
# (only with the read cache on) read the Kopiur kinds and create access reviews.
# It must never hold `secrets` or `pods/exec`: a ServiceAccount with impersonate
# AND either of those could read every repository credential in the cluster
# while appearing to be nobody in particular. `crates/xtask/tests/ui_rbac.rs`
# asserts those absences.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.ui.fullname" . }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
rules:
{{- $anon := .Values.ui.auth.anonymous }}
{{- $anonOnly := and $anon.enabled (eq (.Values.ui.auth.userHeader | default "") "") }}
{{- if $anonOnly }}
  # Anonymous-only mode has exactly ONE identity, so the apiserver — not just
  # the console's header parsing — is what says so. Even a fooled parser cannot
  # reach a second user through a name-scoped impersonate rule.
  - apiGroups: [""]
    resources: [users]
    resourceNames: [{{ $anon.user | quote }}]
    verbs: [impersonate]
  # `system:authenticated` rides along on every impersonated identity, so a
  # name-scoped groups rule that omitted it would reject every request.
  - apiGroups: [""]
    resources: [groups]
    resourceNames: {{ concat ($anon.groups | default list) (list "system:authenticated") | uniq | sortAlpha | toJson }}
    verbs: [impersonate]
{{- else }}
  # Header mode: the proxy asserts whoever authenticated, so the user rule
  # cannot be name-scoped. The guards are the proxy shared secret, the
  # console's `system:` deny-list, and (optionally) allowedGroups below.
  - apiGroups: [""]
    resources: [users]
    verbs: [impersonate]
  {{- with .Values.ui.auth.allowedGroups }}
  # allowedGroups is enforced twice: once by the console's parser and once
  # here, so a bug in the former is still caught by the apiserver.
  - apiGroups: [""]
    resources: [groups]
    resourceNames: {{ concat . (list "system:authenticated") | uniq | sortAlpha | toJson }}
    verbs: [impersonate]
  {{- else }}
  - apiGroups: [""]
    resources: [groups]
    verbs: [impersonate]
  {{- end }}
{{- end }}
{{- range .Values.ui.auth.impersonateExtraKeys }}
  # One rule PER key: Kubernetes RBAC has no `userextras/*` — the wildcard form
  # is accepted and authorizes nothing, failing at the first request instead of
  # at install.
  - apiGroups: [authentication.k8s.io]
    resources: [{{ printf "userextras/%s" . | quote }}]
    verbs: [impersonate]
{{- end }}
{{- if .Values.ui.cache.enabled }}
  # The ONLY reason the console reads a Kopiur object as itself: watch-fed
  # stores, whose contents are then filtered per user with SubjectAccessReviews.
  # Read-only, and gone entirely when ui.cache.enabled is false.
  - apiGroups: [kopiur.home-operations.com]
    resources:
      - repositories
      - snapshotpolicies
      - snapshots
      - snapshotschedules
      - restores
      - maintenances
      - repositoryreplications
      - snapshotreplications
      - clusterrepositories
    verbs: [get, list, watch]
  - apiGroups: [authorization.k8s.io]
    resources: [subjectaccessreviews]
    verbs: [create]
{{- end }}
{{- end }}
