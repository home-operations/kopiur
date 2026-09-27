{{- include "kopiur.ui.validate" . -}}
{{- if and .Values.ui.enabled .Values.ui.rbac.execPolicy.enabled -}}
{{- $policy := .Values.ui.rbac.execPolicy -}}
{{- $prefixes := concat (list "kopiur-browse-") ($policy.extraPodPrefixes | default list) -}}
{{- $nameChecks := list -}}
{{- range $prefixes -}}
{{- $nameChecks = append $nameChecks (printf "request.name.startsWith(%q)" .) -}}
{{- end -}}
# The enforcement RBAC cannot express.
#
# `pods/exec create` is not name-scopable, so `<release>-ui-browse` necessarily
# grants exec on EVERY pod in the namespaces it is bound in — including the
# session pods that carry the repository credentials in their environment. This
# policy narrows that at the apiserver: for the listed subjects, an exec is only
# admitted into a `kopiur-browse-*` pod and only to run kopia.
#
# What it does NOT do: stop those subjects from running kopia commands (that is
# the point of browse), and it does not apply to anyone else — a cluster-admin
# with their own exec grant is unaffected. It also cannot protect a namespace
# where the subject holds exec from some OTHER binding.
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingAdmissionPolicy
metadata:
  name: {{ include "kopiur.ui.fullname" . }}-browse-exec
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
spec:
  failurePolicy: Fail
  matchConstraints:
    resourceRules:
      - apiGroups: [""]
        apiVersions: ["v1"]
        operations: [CONNECT]
        resources: [pods/exec]
  matchConditions:
    # Only the browse subjects. Everyone else's exec is none of this policy's
    # business, and matching them would break unrelated operations.
    - name: browse-subject
      expression: {{ include "kopiur.ui.execSubjectExpr" . | quote }}
  validations:
    - expression: {{ join " || " $nameChecks | quote }}
      message: "kopiur-ui browse may only exec into a kopiur-browse-* session pod"
      reason: Forbidden
    # Fail-closed on a missing/emtpy command: an exec with no explicit command
    # runs the image's entrypoint, which is not a kopia read.
    - expression: "object != null && has(object.command) && size(object.command) > 0 && object.command[0] == '/usr/local/bin/kopia'"
      message: "kopiur-ui browse may only exec /usr/local/bin/kopia in a session pod"
      reason: Forbidden
---
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingAdmissionPolicyBinding
metadata:
  name: {{ include "kopiur.ui.fullname" . }}-browse-exec
  labels:
    {{- include "kopiur.labels" . | nindent 4 }}
    app.kubernetes.io/component: ui
spec:
  policyName: {{ include "kopiur.ui.fullname" . }}-browse-exec
  # Deny, not Warn/Audit: a policy that only logs the exec it was installed to
  # prevent is a report, not a control.
  validationActions: [Deny]
{{- end }}
