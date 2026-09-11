{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.rbac.userRoles -}}
# Doctor's `ControllerRunning` check reads the controller Deployment. That is the
# only `apps` read any UI role needs, and it is a namespaced Role in the
# operator's namespace rather than part of the cluster-wide viewer role: a
# cluster-wide Deployment read exposes every workload's image, env var names and
# replica counts, which is a real grant to buy one health line.
#
# Bind it alongside `<release>-ui-user` for whoever should see the doctor page:
#
#   kubectl create rolebinding kopiur-ui-doctor \
#     --role={{ include "kopiur.ui.fullname" . }}-doctor --group=platform -n {{ .Release.Namespace }}
#
# Unbound, doctor degrades that one check to a Warn — it does not fail.
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: {{ include "kopiur.ui.fullname" . }}-doctor
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
rules:
  - apiGroups: [apps]
    resources: [deployments]
    verbs: [get, list]
{{- end }}
