{{/*
Expand the name of the chart.
*/}}
{{- define "kopiur.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
Truncated at 63 chars (k8s name limit, DNS-1123).
*/}}
{{- define "kopiur.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Chart name and version, as used by the helm.sh/chart label.
*/}}
{{- define "kopiur.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels. Includes global.commonLabels (fleet-wide labelling) so every
object the chart renders carries them.
*/}}
{{- define "kopiur.labels" -}}
helm.sh/chart: {{ include "kopiur.chart" . }}
{{ include "kopiur.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: kopiur
{{- with .Values.global.commonLabels }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{/*
Selector labels (stable across upgrades — never add version here).
*/}}
{{- define "kopiur.selectorLabels" -}}
app.kubernetes.io/name: {{ include "kopiur.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Controller component selector labels.
*/}}
{{- define "kopiur.controller.selectorLabels" -}}
{{ include "kopiur.selectorLabels" . }}
app.kubernetes.io/component: controller
{{- end }}

{{/*
Webhook component selector labels.
*/}}
{{- define "kopiur.webhook.selectorLabels" -}}
{{ include "kopiur.selectorLabels" . }}
app.kubernetes.io/component: webhook
{{- end }}

{{/*
The name of the ServiceAccount to use.
*/}}
{{- define "kopiur.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "kopiur.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Controller component name.
*/}}
{{- define "kopiur.controller.fullname" -}}
{{- printf "%s-controller" (include "kopiur.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Mover identity. The controller mints — in each mover Job's (workload) namespace — a
least-privilege ServiceAccount of this name bound to the mover Role/ClusterRole of
the same name. Both names are passed to the controller via env so the
runtime-minted objects match the chart-shipped role.
*/}}
{{- define "kopiur.moverName" -}}
{{- printf "%s-mover" (include "kopiur.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Dedicated snapshot-replication mover identity (issue #368). The controller
DERIVES this name from KOPIUR_MOVER_CLUSTERROLE by replacing the `-mover`
suffix (`io::snapshot_replication_mover_name`), so this helper MUST stay
`<fullname>-snapshot-replication-mover` — renaming either side alone breaks the
runtime RoleBinding's roleRef. The controller mints the same-named SA +
RoleBinding per namespace, only for snapshot-replication mover Jobs.
*/}}
{{- define "kopiur.snapshotReplicationMoverName" -}}
{{- printf "%s-snapshot-replication-mover" (include "kopiur.fullname" .) | trimSuffix "-" }}
{{- end }}

{{/*
Webhook component name.
*/}}
{{- define "kopiur.webhook.fullname" -}}
{{- printf "%s-webhook" (include "kopiur.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Resolve an image reference from a component image block (repository is the full
registry+path string): repository@digest if digest set, else repository:tag
(tag defaults to .Chart.AppVersion).
Usage: include "kopiur.image" (dict "root" $ "img" .Values.image)
       include "kopiur.image" (dict "root" $ "img" .Values.webhook.image)
       include "kopiur.image" (dict "root" $ "img" .Values.mover.image)
*/}}
{{- define "kopiur.image" -}}
{{- $root := .root -}}
{{- $img := .img -}}
{{- $repo := $img.repository -}}
{{- if $img.digest -}}
{{- printf "%s@%s" $repo $img.digest -}}
{{- else -}}
{{- $tag := default $root.Chart.AppVersion $img.tag -}}
{{- printf "%s:%s" $repo $tag -}}
{{- end -}}
{{- end }}

{{/*
Merged image pull secrets: global.imagePullSecrets concatenated with the
top-level imagePullSecrets. Emits nothing when both are empty.
Usage: {{- include "kopiur.imagePullSecrets" . | nindent 6 }}
*/}}
{{- define "kopiur.imagePullSecrets" -}}
{{- $merged := concat (.Values.global.imagePullSecrets | default list) (.Values.imagePullSecrets | default list) -}}
{{- with $merged }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/*
Global-merge helpers for the two named components (root = controller, webhook.*).
Map-valued globals (nodeSelector, affinity) take the component value when it is
set, else the global. List-valued globals (tolerations, topologySpreadConstraints)
concatenate global + component. Each emits nothing when the merged value is empty,
so callers can `with` the result.
Usage: {{- with (include "kopiur.nodeSelector" (dict "root" $ "component" .Values.nodeSelector)) }}
*/}}
{{- define "kopiur.nodeSelector" -}}
{{- $c := .component -}}
{{- if $c -}}
{{- toYaml $c -}}
{{- else -}}
{{- with .root.Values.global.nodeSelector -}}
{{- toYaml . -}}
{{- end -}}
{{- end -}}
{{- end }}

{{- define "kopiur.affinity" -}}
{{- $c := .component -}}
{{- if $c -}}
{{- toYaml $c -}}
{{- else -}}
{{- with .root.Values.global.affinity -}}
{{- toYaml . -}}
{{- end -}}
{{- end -}}
{{- end }}

{{- define "kopiur.tolerations" -}}
{{- $merged := concat (.root.Values.global.tolerations | default list) (.component | default list) -}}
{{- with $merged -}}
{{- toYaml . -}}
{{- end -}}
{{- end }}

{{- define "kopiur.topologySpreadConstraints" -}}
{{- $merged := concat (.root.Values.global.topologySpreadConstraints | default list) (.component | default list) -}}
{{- with $merged -}}
{{- toYaml . -}}
{{- end -}}
{{- end }}

{{/*
Whether RBAC should be cluster-scoped.
*/}}
{{- define "kopiur.clusterScoped" -}}
{{- eq .Values.installScope "cluster" -}}
{{- end }}

{{/*
Whether the operator self-manages the webhook serving certificate
(webhook.tls.mode: self) — the controller mints the cert + injects the caBundle,
so cert-manager is not required. Renders "true" only when the webhook is enabled
AND in self mode. Used to gate the extra controller RBAC and the chart's
cert-management templates.
*/}}
{{- define "kopiur.webhook.selfManaged" -}}
{{- if and .Values.webhook.enabled (eq (.Values.webhook.tls.mode | default "self") "self") -}}
true
{{- end -}}
{{- end }}

{{/*
OTLP environment for the controller + webhook (and, via the controller, mover
Jobs). Emits the standard OTEL_EXPORTER_OTLP_* vars only when
observability.otlp.enabled. The env var NAMES match crates/telemetry/src/env.rs.
Usage: {{- include "kopiur.otlpEnv" . | nindent 12 }}
*/}}
{{- define "kopiur.otlpEnv" -}}
{{- if .Values.observability.otlp.enabled }}
- name: OTEL_EXPORTER_OTLP_ENDPOINT
  value: {{ required "observability.otlp.endpoint is required when observability.otlp.enabled" .Values.observability.otlp.endpoint | quote }}
{{- with .Values.observability.otlp.protocol }}
- name: OTEL_EXPORTER_OTLP_PROTOCOL
  value: {{ . | quote }}
{{- end }}
{{- with .Values.observability.otlp.headers }}
- name: OTEL_EXPORTER_OTLP_HEADERS
  value: {{ . | quote }}
{{- end }}
{{- if .Values.observability.otlp.strict }}
- name: KOPIUR_OTEL_STRICT
  value: "true"
{{- end }}
{{- with .Values.observability.otlp.extraEnv }}
{{- toYaml . }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Web UI component name and selector labels.
*/}}
{{- define "kopiur.ui.fullname" -}}
{{- printf "%s-ui" (include "kopiur.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "kopiur.ui.selectorLabels" -}}
{{ include "kopiur.selectorLabels" . }}
app.kubernetes.io/component: ui
{{- end }}

{{/*
The web UI's own ServiceAccount. Deliberately NOT the operator's: the operator
reads Secrets on every reconcile, and the console must never be able to — it
impersonates instead, so every read it performs is a named user's read that
Kubernetes RBAC decided on.
*/}}
{{- define "kopiur.ui.serviceAccountName" -}}
{{- if .Values.ui.serviceAccount.create }}
{{- default (include "kopiur.ui.fullname" .) .Values.ui.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.ui.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Every way of configuring the web UI that would deploy an open door, refused at
render time. Included by EVERY ui-*.tpl, so no combination of `helm template
-s` can dodge it.

`fail` rather than a comment or a NOTES.txt warning, because each of these
produces a console that works — and quietly lets the wrong person act as
someone else. Each message names the value to set, not just the value that is
wrong. The rules mirror `UiArgs::resolve` in crates/ui/src/config.rs, which
enforces the same invariants at startup; catching them here turns a CrashLoop
into a failed `helm upgrade`.
*/}}
{{- define "kopiur.ui.validate" -}}
{{- if .Values.ui.enabled -}}
{{- $auth := .Values.ui.auth -}}
{{- $anon := $auth.anonymous -}}
{{- $hasHeader := ne ($auth.userHeader | default "") "" -}}
{{- $anonUser := $anon.user | default "" -}}
{{- $hasAnon := and $anon.enabled (ne $anonUser "") -}}

{{/* Impersonation is cluster-wide by nature, and so are the console's reads. */}}
{{- if ne .Values.installScope "cluster" -}}
{{- fail (printf "ui.enabled requires installScope: cluster, but installScope is %q. The web UI impersonates users on cluster-wide apiserver calls and (with ui.cache.enabled) watches the Kopiur kinds across namespaces; neither can be expressed in a namespaced Role, so it would 403 on its first request. Set installScope: cluster, or ui.enabled: false." .Values.installScope) -}}
{{- end -}}

{{/* Anonymous mode with nobody to be. */}}
{{- if and $anon.enabled (eq $anonUser "") -}}
{{- fail "ui.auth.anonymous.enabled is true but ui.auth.anonymous.user is empty, so there is no identity for requests to run as. Set ui.auth.anonymous.user to the username every request should be authorized as (and ui.auth.anonymous.groups to its groups), then bind kopiur-ui-user or kopiur-ui-viewer to it — or set ui.auth.anonymous.enabled: false and configure ui.auth.userHeader instead." -}}
{{- end -}}

{{/* A fallback with nothing to fall back TO. */}}
{{- if and $anon.fallback (not $hasAnon) -}}
{{- fail "ui.auth.anonymous.fallback is true but no anonymous identity is configured, so a request without the identity header would have nobody to run as. Set ui.auth.anonymous.enabled: true and ui.auth.anonymous.user, or set ui.auth.anonymous.fallback: false to reject header-less requests with 401 instead." -}}
{{- end -}}

{{/* No identity source at all: every request would run as the console's own
     ServiceAccount, which can impersonate anyone. */}}
{{- if and (not $hasHeader) (not $hasAnon) -}}
{{- fail "kopiur-ui has no identity source, so it cannot tell who is calling and every request would run as the console's own ServiceAccount — which can impersonate anyone. Either set ui.auth.userHeader to the header your authenticating proxy injects (e.g. X-Forwarded-User) and ui.auth.proxySecret.existingSecret to a Secret the proxy also sends as X-Kopiur-Proxy-Token, or set ui.auth.anonymous.enabled: true with ui.auth.anonymous.user to run every request as one fixed identity." -}}
{{- end -}}

{{/* Header mode with no trust boundary. Any pod — or anyone with
     services/proxy on the apiserver — can send the same headers straight to
     the Service and become any user. */}}
{{- if and $hasHeader (eq ($auth.proxySecret.existingSecret | default "") "") (not $auth.acknowledgeNoProxySecret) -}}
{{- fail (printf "ui.auth.userHeader is set (%q) but ui.auth.proxySecret.existingSecret is empty. Identity headers are only trustworthy if nothing else can set them, and in a cluster any pod — or anyone who can reach the Service, including through the apiserver's services/proxy — can send those same headers directly to kopiur-ui and become any user, including one bound to kopiur-ui-editor. Create a Secret holding a shared token, set ui.auth.proxySecret.existingSecret to its name, and configure your proxy to send that token as the X-Kopiur-Proxy-Token header. If something else already prevents direct access (a mesh with mTLS, a NetworkPolicy — see ui.networkPolicy), set ui.auth.acknowledgeNoProxySecret: true to record that decision." $auth.userHeader) -}}
{{- end -}}

{{/* A NetworkPolicy with no peer is either a lockout or, written loosely, an
     allow-all — and the second one looks like protection. */}}
{{- if and .Values.ui.networkPolicy.enabled (not .Values.ui.networkPolicy.proxySelector) -}}
{{- fail "ui.networkPolicy.enabled is true but ui.networkPolicy.proxySelector is empty, so the rendered policy would name no peer allowed to reach the app port — the console would be unreachable, and a reader would believe it was protected. Set ui.networkPolicy.proxySelector to a NetworkPolicy peer for your authenticating proxy, e.g. {namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: auth}}, podSelector: {matchLabels: {app.kubernetes.io/name: oauth2-proxy}}}." -}}
{{- end -}}

{{/* An admission policy that matches nobody enforces nothing. */}}
{{- if .Values.ui.rbac.execPolicy.enabled -}}
{{- if not .Values.ui.rbac.execPolicy.subjects -}}
{{- fail "ui.rbac.execPolicy.enabled is true but ui.rbac.execPolicy.subjects is empty, so the ValidatingAdmissionPolicy would match nobody and enforce nothing while appearing to restrict browse exec. List the same users/groups you bound kopiur-ui-browse to, e.g. [{kind: Group, name: platform}]." -}}
{{- end -}}
{{/* Evaluated for its refusals: an untranslatable subject must stop the whole
     install, not just the policy template. Assigned, never emitted — an
     `include` writes its result, and this one's result is CEL. */}}
{{- $_ := include "kopiur.ui.execSubjectExpr" . -}}
{{- end -}}
{{- end -}}
{{- end }}

{{/*
The CEL that decides whether a request came from a browse subject, as one OR'd
expression over ui.rbac.execPolicy.subjects.

A subject kind the chart cannot translate is a `fail`, never a skip: silently
dropping one renders a policy that covers fewer people than the values say it
does — the worst possible outcome for a control whose entire value is that a
reader believes it.
*/}}
{{- define "kopiur.ui.execSubjectExpr" -}}
{{- $subjects := list -}}
{{- range .Values.ui.rbac.execPolicy.subjects -}}
{{- if eq .kind "User" -}}
{{- $subjects = append $subjects (printf "request.userInfo.username == %q" .name) -}}
{{- else if eq .kind "Group" -}}
{{- $subjects = append $subjects (printf "%q in request.userInfo.groups" .name) -}}
{{- else if eq .kind "ServiceAccount" -}}
{{- $ns := .namespace | default "" -}}
{{- if eq $ns "" -}}
{{- fail (printf "ui.rbac.execPolicy.subjects: the ServiceAccount subject %q has no namespace, and a ServiceAccount username is namespace-qualified (system:serviceaccount:<namespace>:<name>) — without it the policy would match nobody. Add `namespace:` to that subject." .name) -}}
{{- end -}}
{{- $subjects = append $subjects (printf "request.userInfo.username == %q" (printf "system:serviceaccount:%s:%s" $ns .name)) -}}
{{- else -}}
{{- fail (printf "ui.rbac.execPolicy.subjects: unknown subject kind %q (expected User, Group or ServiceAccount). A subject the chart cannot translate would be dropped from the rendered policy, so it refuses instead." .kind) -}}
{{- end -}}
{{- end -}}
{{- join " || " $subjects -}}
{{- end }}

{{/*
The web UI's environment. Names come from crates/ui/src/config.rs and are the
chart's contract with the binary; an empty value means "unset" there, so a knob
left blank is simply omitted here rather than rendered as "".
*/}}
{{- define "kopiur.ui.env" -}}
{{- $ui := .Values.ui -}}
{{- include "kopiur.loggingEnv" . }}
- name: KOPIUR_UI_ADDR
  value: {{ printf "[::]:%v" $ui.port | quote }}
- name: KOPIUR_UI_OPS_ADDR
  value: {{ printf "[::]:%v" $ui.opsPort | quote }}
{{- with $ui.auth.userHeader }}
- name: KOPIUR_UI_USER_HEADER
  value: {{ . | quote }}
{{- end }}
{{- with $ui.auth.groupsHeader }}
- name: KOPIUR_UI_GROUPS_HEADER
  value: {{ . | quote }}
{{- end }}
{{- with $ui.auth.groupsSeparator }}
- name: KOPIUR_UI_GROUPS_SEPARATOR
  value: {{ . | quote }}
{{- end }}
{{- with $ui.auth.emailHeader }}
- name: KOPIUR_UI_EMAIL_HEADER
  value: {{ . | quote }}
{{- end }}
{{- with $ui.auth.impersonateExtraKeys }}
- name: KOPIUR_UI_IMPERSONATE_EXTRA_KEYS
  value: {{ join "," . | quote }}
{{- end }}
{{- with $ui.auth.allowedGroups }}
- name: KOPIUR_UI_ALLOWED_GROUPS
  value: {{ join "," . | quote }}
{{- end }}
{{- if $ui.auth.anonymous.enabled }}
- name: KOPIUR_UI_ANONYMOUS_USER
  value: {{ $ui.auth.anonymous.user | quote }}
{{- with $ui.auth.anonymous.groups }}
- name: KOPIUR_UI_ANONYMOUS_GROUPS
  value: {{ join "," . | quote }}
{{- end }}
{{- end }}
{{- if $ui.auth.anonymous.fallback }}
- name: KOPIUR_UI_ANONYMOUS_FALLBACK
  value: "true"
{{- end }}
{{- if $ui.auth.proxySecret.existingSecret }}
- name: KOPIUR_UI_PROXY_SECRET_FILE
  value: {{ printf "/etc/kopiur-ui/proxy/%s" $ui.auth.proxySecret.key }}
{{- else if $ui.auth.acknowledgeNoProxySecret }}
# Recorded deliberately: the binary refuses to start in header mode without
# either a proxy secret or this acknowledgement, so the two sides agree.
- name: KOPIUR_UI_ACKNOWLEDGE_NO_PROXY_SECRET
  value: "true"
{{- end }}
- name: KOPIUR_NAMESPACE
  valueFrom:
    fieldRef:
      fieldPath: metadata.namespace
# Pinned, never discovered. Without this the browse plane would read the mover
# image off the controller Deployment AS THE BROWSING USER, and kopiur-ui-browse
# deliberately grants no `apps/deployments` read.
- name: KOPIUR_MOVER_IMAGE
  value: {{ include "kopiur.image" (dict "root" . "img" .Values.mover.image) | quote }}
- name: KOPIUR_UI_CACHE
  value: {{ $ui.cache.enabled | quote }}
- name: KOPIUR_UI_SESSION_TTL
  value: {{ $ui.session.ttl | quote }}
- name: KOPIUR_UI_SESSION_READY_TIMEOUT
  value: {{ $ui.session.readyTimeout | quote }}
- name: KOPIUR_UI_MAX_SESSION_STARTS
  value: {{ $ui.session.maxStarts | int64 | quote }}
- name: KOPIUR_UI_MAX_EXEC_PER_IDENTITY
  value: {{ $ui.session.maxExecPerIdentity | int64 | quote }}
- name: KOPIUR_UI_MAX_EXEC_GLOBAL
  value: {{ $ui.session.maxExecGlobal | int64 | quote }}
- name: KOPIUR_UI_MAX_DOWNLOAD_BYTES
  value: {{ $ui.download.maxBytes | int64 | quote }}
- name: KOPIUR_UI_DOWNLOAD_CHUNK_TIMEOUT
  value: {{ $ui.download.chunkTimeout | quote }}
- name: KOPIUR_UI_MAX_MANIFEST_BYTES
  value: {{ $ui.limits.maxManifestBytes | int64 | quote }}
- name: KOPIUR_UI_SNAPSHOT_LIST_CAP
  value: {{ $ui.limits.snapshotListCap | int64 | quote }}
- name: KOPIUR_UI_CLIENT_CACHE_SIZE
  value: {{ $ui.limits.clientCacheSize | int64 | quote }}
- name: KOPIUR_UI_CLIENT_CACHE_TTL
  value: {{ $ui.limits.clientCacheTtl | quote }}
- name: KOPIUR_UI_SAR_TTL
  value: {{ $ui.limits.sarTtl | quote }}
- name: KOPIUR_UI_SAR_CACHE_SIZE
  value: {{ $ui.limits.sarCacheSize | int64 | quote }}
{{- if $ui.tls.existingSecret }}
- name: KOPIUR_UI_TLS_CERT
  value: /etc/kopiur-ui/tls/tls.crt
- name: KOPIUR_UI_TLS_KEY
  value: /etc/kopiur-ui/tls/tls.key
{{- end }}
{{- include "kopiur.otlpEnv" . }}
{{- with $ui.extraEnv }}
{{- toYaml . }}
{{- end }}
{{- end }}

{{/*
Logging environment for the controller + webhook (and, via the controller, mover
Jobs — the controller forwards RUST_LOG + KOPIUR_LOG_FORMAT from its own env).
RUST_LOG comes from logging.level; KOPIUR_LOG_FORMAT selects text|json. The env
var NAMES match crates/telemetry/src/env.rs.
Usage: {{- include "kopiur.loggingEnv" . | nindent 12 }}
*/}}
{{- define "kopiur.loggingEnv" -}}
- name: RUST_LOG
  value: {{ .Values.logging.level | default "info" | quote }}
- name: KOPIUR_LOG_FORMAT
  value: {{ .Values.logging.format | default "text" | quote }}
{{- end }}
