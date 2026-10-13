//! The one YAML emitter every generated artifact goes through, so CRDs and RBAC
//! share a single formatting policy.

/// Serialize `value` as YAML the way `kubectl` does: one physical line per single-line string, never wrapped into a folded
/// `>-` block. Multi-line strings still render as `|` literal blocks.
pub fn to_string<T: serde::Serialize>(value: &T) -> Result<String, serde_saphyr::SerializeError> {
    let mut options = serde_saphyr::SerializerOptions::default();
    options.folded_wrap_chars = usize::MAX;
    serde_saphyr::to_string_with_options(value, options)
}
