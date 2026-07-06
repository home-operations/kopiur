//! Generated CRD field reference (`docs/field-reference.md`).
//!
//! Walks each CRD's structural schema — the exact `T::crd()` output that ships
//! in `deploy/crds/` — and renders one Markdown page: every spec/status field
//! with its type, schema `default:`, constraints, and description (from the
//! Rust doc comments). Because it consumes the shipped schema, the page cannot
//! drift from the CRDs, and the `gen-all --check` guard proves it stays in sync.
//!
//! Design notes:
//! - **Opaque leaves.** kube inlines embedded `k8s-openapi` core/v1 types
//!   (`securityContext`, `resources`, `affinity`, …) into hundreds of schema
//!   lines each. We render those as a single `core/v1 X` row and do NOT descend,
//!   matching the hand-written page and keeping the output readable.
//! - **Two union shapes.** Externally-tagged enums surface as either a `oneOf`
//!   of single-`required` branches (`backend`, hooks, restore source/target) or,
//!   when the union also carries common fields, a flat object whose exclusivity
//!   is a CEL rule (`source`). Both are detected and labelled "set exactly one".
//! - **Arrays of objects** (`sources[]`, `hooks.*[]`) get their own sub-section.
//! - **Determinism.** Property maps are `BTreeMap` (alphabetical); the whole
//!   page is a pure function of the schemas, so two runs are byte-identical.

use anyhow::Result;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
    CustomResourceDefinition, JSONSchemaProps, JSONSchemaPropsOrArray, JSONSchemaPropsOrBool,
};
use kube::core::CustomResourceExt;

use crate::artifact::{Artifact, DOCS_GEN_HEADER};

/// Generate the single `docs/field-reference.md` artifact for all 8 CRDs.
pub fn artifacts() -> Result<Vec<Artifact>> {
    // Same order as `crds::artifacts()` so the page reads spec-first, simple to
    // complex, and stays stable.
    let crds = [
        kopiur_api::Repository::crd(),
        kopiur_api::ClusterRepository::crd(),
        kopiur_api::SnapshotPolicy::crd(),
        kopiur_api::Snapshot::crd(),
        kopiur_api::SnapshotSchedule::crd(),
        kopiur_api::Restore::crd(),
        kopiur_api::Maintenance::crd(),
        kopiur_api::RepositoryReplication::crd(),
    ];

    let mut page = String::new();
    page.push_str(DOCS_GEN_HEADER);
    page.push_str(INTRO);
    for crd in &crds {
        page.push_str(&render_crd(crd));
    }

    Ok(vec![Artifact::docs("field-reference.md".to_string(), page)])
}

/// The hand-authored page preamble (title + how it is generated + conventions).
const INTRO: &str = "# Field reference\n\
\n\
Every spec and status field of all eight CRDs in \
`kopiur.home-operations.com/v1alpha1`, with its type, schema default, and a \
one-line meaning taken straight from the Rust doc comments.\n\
\n\
This page is **generated** from the `kopiur-api` CRD schemas by `cargo xtask \
gen-docs` (run `mise run gen`); it is drift-checked in CI, so it can never go \
stale against the shipped CRDs. To change an entry, edit the doc comment on the \
field in `crates/api` — not this file. For the task-oriented explanations \
(mental model, which knobs you actually change), see the per-CRD pages under \
[CRD reference](reference/crds/index.md).\n\
\n\
/// info | Conventions\n\
\n\
- **Type** uses the CRD/YAML shape. `enum: A \\| B` lists the allowed values; \
`[]T` is an array, `map[string]T` a map, `object (free-form)` an \
unvalidated object, and `core/v1 X` an embedded Kubernetes type (see the \
Kubernetes API reference for its fields).\n\
- **Default** is the schema `default:` the API server materializes when the \
field is absent. `—` means no schema default — an optional field that is simply \
unset, or one whose effective default depends on context (the description says \
which).\n\
- **required** marks a field with no default that must be present, or admission \
fails.\n\
- Externally-tagged unions select a variant by **which key you set**, never a \
`kind:` field; each is flagged \"set exactly one of …\".\n\
- Validation rules enforced by the API server (`x-kubernetes-validations`, \
including immutability) are listed under the table they apply to.\n\
\n\
///\n";

/// Render one CRD: a `##` heading + metadata line, then spec and status sections.
fn render_crd(crd: &CustomResourceDefinition) -> String {
    let kind = &crd.spec.names.kind;
    let version = &crd.spec.versions[0];
    let mut out = String::new();

    out.push_str("\n---\n\n");
    out.push_str(&format!("## {kind} {{ #{} }}\n\n", slug(kind)));

    // Metadata line: scope, short names, print columns.
    let mut meta: Vec<String> = vec![format!("**Scope:** {}", crd.spec.scope)];
    if let Some(short) = &crd.spec.names.short_names
        && !short.is_empty()
    {
        let names = short
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(", ");
        meta.push(format!("**Short names:** {names}"));
    }
    if let Some(cols) = &version.additional_printer_columns
        && !cols.is_empty()
    {
        let names = cols
            .iter()
            .map(|c| format!("`{}`", c.name))
            .collect::<Vec<_>>()
            .join(", ");
        meta.push(format!("**Print columns:** {names}"));
    }
    out.push_str(&meta.join(" · "));
    out.push('\n');

    let Some(schema) = version
        .schema
        .as_ref()
        .and_then(|s| s.open_api_v3_schema.as_ref())
    else {
        return out;
    };
    let props = match &schema.properties {
        Some(p) => p,
        None => return out,
    };

    // Collect sections depth-first for spec then status, so the page reads
    // top-level object first, then each sub-object below it.
    let mut sections: Vec<Section> = Vec::new();
    for top in ["spec", "status"] {
        if let Some(node) = props.get(top) {
            walk(kind, top, node, &mut sections);
        }
    }
    for s in &sections {
        out.push_str(&s.render(kind));
    }
    out
}

/// One rendered table: a dotted path within a CRD, its rows, and any CEL rules.
struct Section {
    /// Dotted path from the CRD root, e.g. `spec.backend` or `spec.sources[]`.
    path: String,
    /// Optional intro line (union "set exactly one of …").
    intro: Option<String>,
    rows: Vec<Row>,
    /// `x-kubernetes-validations` attached at this object node.
    validations: Vec<(Option<String>, String)>,
}

/// One field row in a section table.
struct Row {
    name: String,
    type_str: String,
    /// Rendered default cell (already backticked, or `**required**`, or `—`).
    default: String,
    description: String,
}

impl Section {
    fn render(&self, kind: &str) -> String {
        let depth = self.path.matches('.').count(); // spec=0 → ###, deeper → more #
        let hashes = "#".repeat((depth + 3).min(6));
        let mut out = format!(
            "\n{hashes} `{}` {{ #{} }}\n\n",
            self.path,
            anchor(kind, &self.path)
        );
        if let Some(intro) = &self.intro {
            out.push_str(intro);
            out.push_str("\n\n");
        }
        out.push_str("| Field | Type | Default | Description |\n| --- | --- | --- | --- |\n");
        for r in &self.rows {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                r.name,
                escape_cell(&r.type_str),
                r.default,
                escape_cell(&r.description),
            ));
        }
        if !self.validations.is_empty() {
            out.push_str("\n**Validation rules** (enforced at admission):\n\n");
            for (msg, rule) in &self.validations {
                // Prefer the human message (the actual contract, e.g. "create.hash
                // is immutable after creation"); fall back to the raw CEL only when
                // a rule carries no message. The long `oldSelf` transition rules are
                // otherwise unreadable in a reference table.
                match msg {
                    Some(m) if !m.trim().is_empty() => {
                        out.push_str(&format!("- {}\n", escape_prose(m)))
                    }
                    _ => out.push_str(&format!("- `{}`\n", rule.replace('\n', " "))),
                }
            }
        }
        out
    }
}

/// Recursively walk an object node, appending a [`Section`] for it and every
/// sub-object / union / array-of-object it contains.
fn walk(kind: &str, path: &str, node: &JSONSchemaProps, sections: &mut Vec<Section>) {
    let props = match &node.properties {
        Some(p) if !p.is_empty() => p,
        _ => return,
    };
    let required: std::collections::BTreeSet<&str> = node
        .required
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(String::as_str)
        .collect();

    // Two stable passes: required fields first, then optional — each alphabetical
    // (BTreeMap iteration). Answers "required vs optional" by ordering + the cell.
    let mut ordered: Vec<(&String, &JSONSchemaProps)> = props.iter().collect();
    ordered.sort_by_key(|(name, _)| (!required.contains(name.as_str()), (*name).clone()));

    let mut rows = Vec::new();
    // Sub-objects to recurse into AFTER this section is pushed (keeps output
    // ordered parent-then-children).
    let mut children: Vec<(String, JSONSchemaProps)> = Vec::new();

    for (name, prop) in ordered {
        let child_path = format!("{path}.{name}");
        let is_required = required.contains(name.as_str());

        let (type_str, recurse) = classify(kind, name, prop, &child_path);
        if let Some((sub_path, sub_schema)) = recurse {
            children.push((sub_path, sub_schema));
        }
        rows.push(Row {
            name: name.clone(),
            type_str,
            default: default_cell(prop, is_required),
            description: strip_rustdoc_links(&prop.description.clone().unwrap_or_default()),
        });
    }

    sections.push(Section {
        path: path.to_string(),
        intro: union_intro(node),
        rows,
        validations: collect_validations(node),
    });

    for (sub_path, sub_schema) in children {
        walk(kind, &sub_path, &sub_schema, sections);
    }
}

/// Decide how a field renders: its type string, and (if it breaks out) the
/// sub-section path + schema to recurse into.
fn classify(
    kind: &str,
    name: &str,
    prop: &JSONSchemaProps,
    child_path: &str,
) -> (String, Option<(String, JSONSchemaProps)>) {
    // 1. Opaque embedded types — one row, never descend.
    if let Some(t) = opaque_leaf(name, prop) {
        return (t, None);
    }
    // 2. Externally-tagged `oneOf` union — link to a section; each variant recurses.
    if oneof_variants(prop).is_some() {
        let link = format!("[union](#{})", anchor(kind, child_path));
        return (link, Some((child_path.to_string(), prop.clone())));
    }
    // 3. Array of objects — `[]link`, recurse into the element type at `path[]`.
    if prop.type_.as_deref() == Some("array")
        && let Some(JSONSchemaPropsOrArray::Schema(item)) = &prop.items
        && item.properties.as_ref().is_some_and(|p| !p.is_empty())
        && opaque_leaf(name, item).is_none()
    {
        let elem_path = format!("{child_path}[]");
        let link = format!("[][object](#{})", anchor(kind, &elem_path));
        return (link, Some((elem_path, (**item).clone())));
    }
    // 4. Object with its own fields — link + recurse.
    if prop.type_.as_deref() == Some("object")
        && prop.properties.as_ref().is_some_and(|p| !p.is_empty())
    {
        let link = format!("[object](#{})", anchor(kind, child_path));
        return (link, Some((child_path.to_string(), prop.clone())));
    }
    // 5. Everything else is an inline scalar / enum / scalar-array / map.
    (type_name(prop), None)
}

/// A single-row "opaque" type name for embedded `k8s-openapi` types and
/// free-form objects — the walker does NOT descend into these.
fn opaque_leaf(name: &str, s: &JSONSchemaProps) -> Option<String> {
    // Free-form object (preserve-unknown-fields with no declared properties):
    // the SnapshotPolicy hooks `jobSpec` PodSpec, collapsed in the schema.
    if s.x_kubernetes_preserve_unknown_fields == Some(true)
        && s.properties.as_ref().is_none_or(|p| p.is_empty())
    {
        return Some("object (free-form)".into());
    }
    // Label selector by structural signature (matchLabels + matchExpressions).
    if let Some(props) = &s.properties
        && props.contains_key("matchExpressions")
        && props.contains_key("matchLabels")
        && props.len() <= 2
    {
        return Some("core/v1 LabelSelector".into());
    }
    // Embedded core/v1 types, matched by the conventional field name.
    let t = match name {
        "securityContext" => "core/v1 SecurityContext",
        "podSecurityContext" => "core/v1 PodSecurityContext",
        "resources" => "core/v1 ResourceRequirements",
        "affinity" => "core/v1 Affinity",
        "tolerations" => "[]core/v1 Toleration",
        "nodeSelector" | "podLabels" | "podAnnotations" => "map[string]string",
        _ => return None,
    };
    Some(t.into())
}

/// If `s` is an externally-tagged `oneOf` union (every branch is a single
/// `required` key present in `properties`), return the variant keys (sorted).
fn oneof_variants(s: &JSONSchemaProps) -> Option<Vec<String>> {
    let one_of = s.one_of.as_ref()?;
    let props = s.properties.as_ref()?;
    if one_of.is_empty() {
        return None;
    }
    let mut variants = Vec::new();
    for branch in one_of {
        let req = branch.required.as_ref()?;
        if req.len() != 1 || !props.contains_key(&req[0]) {
            return None;
        }
        variants.push(req[0].clone());
    }
    variants.sort();
    variants.dedup();
    Some(variants)
}

/// If `s` carries a CEL "exactly one of" rule (`(has(self.a)?1:0)+… == 1`),
/// return the `self.<key>` targets (sorted). This is the flat-union shape used
/// by `source`, which also has common fields.
fn cel_exclusive_variants(s: &JSONSchemaProps) -> Option<Vec<String>> {
    let vals = s.x_kubernetes_validations.as_ref()?;
    for v in vals {
        if v.rule.contains("? 1 : 0)") && v.rule.contains("== 1") {
            let mut keys = Vec::new();
            for part in v.rule.split("has(self.").skip(1) {
                if let Some(end) = part.find(')') {
                    keys.push(part[..end].to_string());
                }
            }
            keys.sort();
            keys.dedup();
            if keys.len() >= 2 {
                return Some(keys);
            }
        }
    }
    None
}

/// The "set exactly one of …" intro for a union node (either shape), if any.
fn union_intro(node: &JSONSchemaProps) -> Option<String> {
    let variants = oneof_variants(node).or_else(|| cel_exclusive_variants(node))?;
    let list = variants
        .iter()
        .map(|v| format!("`{v}`"))
        .collect::<Vec<_>>()
        .join(" · ");
    Some(format!(
        "Externally tagged — set **exactly one** of: {list}."
    ))
}

/// `x-kubernetes-validations` on a node, minus the exclusivity rule already
/// surfaced by [`union_intro`], as `(message, rule)` pairs.
fn collect_validations(node: &JSONSchemaProps) -> Vec<(Option<String>, String)> {
    let is_union = oneof_variants(node).is_some() || cel_exclusive_variants(node).is_some();
    node.x_kubernetes_validations
        .as_ref()
        .map(|vs| {
            vs.iter()
                .filter(|v| !(is_union && v.rule.contains("? 1 : 0)") && v.rule.contains("== 1")))
                .map(|v| (v.message.clone(), v.rule.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Human-readable type string for an inline (non-breakout) field.
fn type_name(s: &JSONSchemaProps) -> String {
    if s.x_kubernetes_int_or_string == Some(true) {
        return "int-or-string".into();
    }
    if let Some(values) = &s.enum_ {
        let rendered = values
            .iter()
            .map(|v| match &v.0 {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" | ");
        return format!("enum: {rendered}");
    }
    match s.type_.as_deref() {
        Some("string") => "string".into(),
        Some("boolean") => "boolean".into(),
        Some("integer") => "integer".into(),
        Some("number") => "number".into(),
        Some("array") => {
            let elem = match &s.items {
                Some(JSONSchemaPropsOrArray::Schema(item)) => type_name(item),
                _ => "object".into(),
            };
            format!("[]{elem}")
        }
        Some("object") => match &s.additional_properties {
            Some(JSONSchemaPropsOrBool::Schema(v)) => format!("map[string]{}", type_name(v)),
            _ => "object".into(),
        },
        _ if s.x_kubernetes_preserve_unknown_fields == Some(true) => "object (free-form)".into(),
        _ => "object".into(),
    }
}

/// The Default cell: the schema `default:` (backticked), `**required**`, or `—`,
/// plus any numeric/length/pattern constraints folded into the same cell.
fn default_cell(s: &JSONSchemaProps, required: bool) -> String {
    let mut cell = if let Some(d) = &s.default {
        format!("`{}`", render_json(&d.0))
    } else if required {
        "**required**".into()
    } else {
        "—".into()
    };
    let mut constraints = Vec::new();
    if let Some(m) = s.minimum {
        constraints.push(format!("min {}", num(m)));
    }
    if let Some(m) = s.maximum {
        constraints.push(format!("max {}", num(m)));
    }
    if let Some(m) = s.min_length {
        constraints.push(format!("minLength {m}"));
    }
    if let Some(m) = s.max_length {
        constraints.push(format!("maxLength {m}"));
    }
    if let Some(m) = s.min_items {
        constraints.push(format!("minItems {m}"));
    }
    if let Some(m) = s.max_items {
        constraints.push(format!("maxItems {m}"));
    }
    if !constraints.is_empty() {
        cell.push_str(&format!("<br><sub>{}</sub>", constraints.join("; ")));
    }
    cell
}

/// Compactly render a JSON default: strings bare, everything else canonical.
fn render_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Format an `f64` bound as an integer when it is whole (`0` not `0.0`).
fn num(f: f64) -> String {
    if f.fract() == 0.0 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// Escape a value for a Markdown table cell: pipes, newlines, angle brackets.
/// `<br>` inserts are re-emitted after escaping so multi-line descriptions and
/// `<svc>.<ns>` text never break the table.
fn escape_cell(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    // Collapse hard-wrapped doc-comment lines into single spaces, then real
    // paragraph breaks (blank line) into <br>.
    escaped
        .replace("\n\n", "<br>")
        .replace('\n', " ")
        .trim()
        .to_string()
}

/// Escape prose (validation messages) that renders outside a table cell.
fn escape_prose(s: &str) -> String {
    s.replace('\n', " ").trim().to_string()
}

/// Rewrite rustdoc intra-doc links to plain Markdown. Doc comments are written
/// for rustdoc, so descriptions carry `[`Type`]`, `[`field`](Self::field)`, and
/// `[`crate::consts::X`]` — which render as broken links in MkDocs. Keep the
/// label text (a code span), drop the brackets and any `(target)`.
fn strip_rustdoc_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find(']') else {
            // No closing bracket — emit the '[' literally and stop scanning.
            out.push('[');
            rest = after_open;
            continue;
        };
        let label = &after_open[..close];
        // Only rewrite when the label is a rustdoc-ish token (a code span or a
        // bare path/identifier) — never a real prose `[word]`.
        let looks_rustdoc = label.starts_with('`')
            || (!label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '`')));
        if !looks_rustdoc {
            // Leave the bracket as prose and keep scanning after it.
            out.push('[');
            rest = after_open;
            continue;
        }
        // Rustdoc link: keep the label, drop the brackets and any `(target)`.
        let mut tail = &after_open[close + 1..];
        if tail.starts_with('(')
            && let Some(cp) = tail.find(')')
        {
            tail = &tail[cp + 1..];
        }
        out.push_str(label);
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// A stable, collision-free anchor id for a CRD path, e.g.
/// `repository-spec-backend`. Array markers `[]` are dropped and the whole id is
/// lowercased to match MkDocs slug conventions (all kopiur field paths are
/// unique case-insensitively, so no collisions).
fn anchor(kind: &str, path: &str) -> String {
    format!(
        "{}-{}",
        slug(kind),
        path.replace("[]", "").replace('.', "-")
    )
    .to_lowercase()
}

/// Lowercase kebab slug of a CRD kind for anchors.
fn slug(kind: &str) -> String {
    kind.to_lowercase()
}

#[cfg(test)]
mod tests;
