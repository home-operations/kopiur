//! Unit tests for the field-reference generator's pure helpers.

use super::*;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
    JSON, JSONSchemaProps, ValidationRule,
};

/// A bare object schema with the given properties (names only, empty leaves).
fn obj(props: &[&str]) -> JSONSchemaProps {
    JSONSchemaProps {
        type_: Some("object".into()),
        properties: Some(
            props
                .iter()
                .map(|p| ((*p).to_string(), JSONSchemaProps::default()))
                .collect(),
        ),
        ..Default::default()
    }
}

#[test]
fn escape_cell_neutralizes_table_breakers() {
    // Pipes must be escaped or they split the row; angle brackets (status.server
    // endpoints like `<svc>.<ns>.svc:<port>`) become entities; a hard-wrapped
    // multi-line doc comment collapses to one line, paragraph breaks to <br>.
    assert_eq!(escape_cell("a | b"), "a \\| b");
    assert_eq!(escape_cell("<svc>.<ns>"), "&lt;svc&gt;.&lt;ns&gt;");
    assert_eq!(escape_cell("line one\nstill one"), "line one still one");
    assert_eq!(escape_cell("para one\n\npara two"), "para one<br>para two");
    // Backticks are preserved (descriptions use them heavily).
    assert_eq!(escape_cell("set `pvc`"), "set `pvc`");
}

#[test]
fn opaque_leaf_collapses_embedded_k8s_types() {
    assert_eq!(
        opaque_leaf("securityContext", &JSONSchemaProps::default()).as_deref(),
        Some("core/v1 SecurityContext")
    );
    assert_eq!(
        opaque_leaf("resources", &JSONSchemaProps::default()).as_deref(),
        Some("core/v1 ResourceRequirements")
    );
    assert_eq!(
        opaque_leaf("tolerations", &JSONSchemaProps::default()).as_deref(),
        Some("[]core/v1 Toleration")
    );
    // A kopiur-authored field name is NOT collapsed.
    assert_eq!(opaque_leaf("backend", &obj(&["s3"])), None);
}

#[test]
fn opaque_leaf_detects_label_selectors_and_free_form_objects() {
    let selector = obj(&["matchLabels", "matchExpressions"]);
    assert_eq!(
        opaque_leaf("inheritSecurityContextFrom", &selector).as_deref(),
        Some("core/v1 LabelSelector")
    );
    let free_form = JSONSchemaProps {
        x_kubernetes_preserve_unknown_fields: Some(true),
        ..Default::default()
    };
    assert_eq!(
        opaque_leaf("jobSpec", &free_form).as_deref(),
        Some("object (free-form)")
    );
}

#[test]
fn oneof_variants_detects_externally_tagged_unions() {
    let mut backend = obj(&["s3", "filesystem", "azure"]);
    backend.one_of = Some(vec![
        JSONSchemaProps {
            required: Some(vec!["s3".into()]),
            ..Default::default()
        },
        JSONSchemaProps {
            required: Some(vec!["filesystem".into()]),
            ..Default::default()
        },
        JSONSchemaProps {
            required: Some(vec!["azure".into()]),
            ..Default::default()
        },
    ]);
    // Sorted alphabetically, deterministic.
    assert_eq!(
        oneof_variants(&backend),
        Some(vec!["azure".into(), "filesystem".into(), "s3".into()])
    );
    // A oneOf branch that requires two keys is NOT this shape.
    let mut not_union = obj(&["a", "b"]);
    not_union.one_of = Some(vec![JSONSchemaProps {
        required: Some(vec!["a".into(), "b".into()]),
        ..Default::default()
    }]);
    assert_eq!(oneof_variants(&not_union), None);
}

#[test]
fn cel_exclusive_variants_detects_the_source_shape() {
    let mut source = obj(&["pvc", "pvcSelector", "nfs", "sourcePathOverride"]);
    source.x_kubernetes_validations = Some(vec![ValidationRule {
        rule: "(has(self.pvc) ? 1 : 0) + (has(self.pvcSelector) ? 1 : 0) + \
               (has(self.nfs) ? 1 : 0) == 1"
            .into(),
        message: Some("set exactly one source".into()),
        ..Default::default()
    }]);
    assert_eq!(
        cel_exclusive_variants(&source),
        Some(vec!["nfs".into(), "pvc".into(), "pvcSelector".into()])
    );
    // union_intro surfaces it; collect_validations then hides that same rule.
    assert!(union_intro(&source).unwrap().contains("exactly one"));
    assert!(collect_validations(&source).is_empty());
}

#[test]
fn type_name_covers_scalars_enums_arrays_and_maps() {
    let s = |t: &str| JSONSchemaProps {
        type_: Some(t.into()),
        ..Default::default()
    };
    assert_eq!(type_name(&s("string")), "string");
    assert_eq!(type_name(&s("boolean")), "boolean");
    assert_eq!(type_name(&s("integer")), "integer");

    let mut e = s("string");
    e.enum_ = Some(vec![
        JSON(serde_json::json!("Delete")),
        JSON(serde_json::json!("Retain")),
    ]);
    assert_eq!(type_name(&e), "enum: Delete | Retain");

    let int_or_string = JSONSchemaProps {
        x_kubernetes_int_or_string: Some(true),
        ..Default::default()
    };
    assert_eq!(type_name(&int_or_string), "int-or-string");
}

#[test]
fn default_cell_shows_default_required_or_dash_with_constraints() {
    let with_default = JSONSchemaProps {
        default: Some(JSON(serde_json::json!(1000))),
        ..Default::default()
    };
    assert_eq!(default_cell(&with_default, false), "`1000`");

    let string_default = JSONSchemaProps {
        default: Some(JSON(serde_json::json!("30m"))),
        ..Default::default()
    };
    assert_eq!(default_cell(&string_default, false), "`30m`");

    assert_eq!(
        default_cell(&JSONSchemaProps::default(), true),
        "**required**"
    );
    assert_eq!(default_cell(&JSONSchemaProps::default(), false), "—");

    let bounded = JSONSchemaProps {
        minimum: Some(0.0),
        maximum: Some(30.0),
        ..Default::default()
    };
    assert_eq!(
        default_cell(&bounded, false),
        "—<br><sub>min 0; max 30</sub>"
    );
}

#[test]
fn anchors_are_stable_and_drop_array_markers() {
    assert_eq!(
        anchor("Repository", "spec.backend"),
        "repository-spec-backend"
    );
    assert_eq!(
        anchor("SnapshotPolicy", "spec.sources[]"),
        "snapshotpolicy-spec-sources"
    );
    // camelCase path segments are lowercased for the anchor id.
    assert_eq!(
        anchor("Repository", "spec.moverDefaults"),
        "repository-spec-moverdefaults"
    );
}

#[test]
fn strip_rustdoc_links_keeps_labels_drops_targets() {
    // Bare intra-doc link.
    assert_eq!(
        strip_rustdoc_links("Defaults to [`DEFAULT_SERVER_PORT`] when omitted."),
        "Defaults to `DEFAULT_SERVER_PORT` when omitted."
    );
    // Link with a rustdoc target.
    assert_eq!(
        strip_rustdoc_links("re-probe every [`interval`](Self::interval)."),
        "re-probe every `interval`."
    );
    // A path-style link without backticks.
    assert_eq!(
        strip_rustdoc_links("see [crate::consts::X] for details"),
        "see crate::consts::X for details"
    );
    // Real prose brackets are left alone.
    assert_eq!(
        strip_rustdoc_links("a value [in brackets] stays"),
        "a value [in brackets] stays"
    );
    // Non-ASCII around a link survives (UTF-8 safety).
    assert_eq!(
        strip_rustdoc_links("— uses [`X`] · done"),
        "— uses `X` · done"
    );
}

#[test]
fn generated_page_is_deterministic() {
    // The whole page is a pure function of the schemas; two runs must be byte
    // identical (guards a future HashMap/ordering regression).
    let a = artifacts().unwrap();
    let b = artifacts().unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].content, b[0].content);
    assert!(a[0].content.contains("# Field reference"));
}
