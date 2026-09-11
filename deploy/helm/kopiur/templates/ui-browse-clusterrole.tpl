{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.rbac.browseRole -}}
# OPT-IN (ui.rbac.browseRole): the in-snapshot file browser.
#
# GRANTING THIS GRANTS THAT NAMESPACE'S REPOSITORY CREDENTIALS. Browsing runs a
# read-only session pod in the snapshot's namespace and `pods/exec`s kopia into
# it; that pod loads the repository credentials through `envFrom`, so anyone who
# can exec in the namespace can print them with `env`. `pods/exec create` is not
# name-scopable, so RBAC cannot narrow it to the session pod — which is why this
# role is deliberately NOT aggregated into `<release>-ui-user`.
#
# Bind it on purpose, and prefer a namespaced RoleBinding over a
# ClusterRoleBinding so the grant reaches one namespace's repositories rather
# than every namespace's:
#
#   kubectl create rolebinding kopiur-browse-media \
#     --clusterrole={{ include "kopiur.ui.fullname" . }}-browse --group=platform -n media
#
# For the enforcement RBAC cannot express — restricting the exec to
# `kopiur-browse-*` pods running kopia — see ui.rbac.execPolicy.
#
# Differs from the CLI's `{{ include "kopiur.fullname" . }}-browse` role by
# dropping `apps/deployments`: the console pins the mover image through
# KOPIUR_MOVER_IMAGE, so a browsing user never needs to discover it.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "kopiur.ui.fullname" . }}-browse
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
rules:
  # Resolve the Snapshot → repository chain.
  - apiGroups: [kopiur.home-operations.com]
    resources:
      - snapshots
      - repositories
    verbs: [get, list]
  - apiGroups: [kopiur.home-operations.com]
    resources: [clusterrepositories]
    verbs: [get]
  # The session Job (find-or-create, end).
  - apiGroups: [batch]
    resources: [jobs]
    verbs: [create, get, list, delete]
  # `get`: a repository's `tls.caBundleRef` CA-bundle ConfigMap, so a private-CA
  # endpoint is trusted. `delete`: reap a LEGACY session's work-spec ConfigMap.
  - apiGroups: [""]
    resources: [configmaps]
    verbs: [get, delete]
  # Wait for the session pod to become Ready; surface its logs on failure.
  - apiGroups: [""]
    resources: [pods]
    verbs: [get, list, watch]
  - apiGroups: [""]
    resources: [pods/log]
    verbs: [get]
  # The read path itself — and the reason this role is dangerous.
  - apiGroups: [""]
    resources: [pods/exec]
    verbs: [create]
{{- end }}
