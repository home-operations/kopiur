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
