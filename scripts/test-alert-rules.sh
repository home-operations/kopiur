#!/usr/bin/env bash
# test-alert-rules.sh — semantic regression tests for the bundled PrometheusRule.
#
# helm-unittest pins the rendered TEXT of the alert exprs; only `promtool test
# rules` pins their BEHAVIOR — and issue #280 was a semantic bug (a retained
# Failed Snapshot CR paged forever even after its policy recovered). This script
# renders the chart, extracts the rule groups, and drives them through promtool
# against the scenarios in deploy/helm/kopiur/tests/alerts/kopiur-rules.test.yaml.
#
# Runs fully offline; wired into `mise run helm-test` so CI exercises it too.
# Requires helm, yq, and promtool on PATH (all mise-pinned in .mise/config.toml).
set -euo pipefail

cd "$(dirname "$0")/.."

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT

helm template kopiur deploy/helm/kopiur \
  --set monitoring.prometheusRule.enabled=true \
  --show-only templates/prometheusrule.tpl > "$out/rendered.yaml"

# promtool's rule_files want a bare {groups: [...]} document, not the whole
# PrometheusRule object — project spec.groups out of the rendered manifest.
yq '{"groups": .spec.groups}' "$out/rendered.yaml" > "$out/kopiur-rules.yaml"

cp deploy/helm/kopiur/tests/alerts/kopiur-rules.test.yaml "$out/"

(cd "$out" && promtool test rules kopiur-rules.test.yaml)
