//! Recorded snapshot metadata — the `kopiur-meta` kopia tag.
//!
//! Every produced backup records the mover identity it ran as (uid/gid/fsGroup
//! plus its provenance) as a compact JSON value under the [`KOPIUR_META_TAG`]
//! snapshot tag, so a later restore — possibly on a rebuilt cluster where the
//! workload no longer exists — can reproduce the identity the data expects.
//! The catalog scan decodes the tag back into `Snapshot.status.recorded`.
//!
//! ## Schema write policy
//!
//! Writers emit the **lowest** schema number that represents the data
//! ([`KOPIUR_META_SCHEMA_V1`] today); the schema is bumped only for a semantic
//! change an old reader would misinterpret, never for additive fields (readers
//! accept unknown JSON fields). An older operator reading a newer schema
//! degrades to recorded-absent ([`MetaTagDecode::UnsupportedSchema`]) — it never
//! errors. This matters for shared-repository multi-cluster topologies running
//! mixed operator versions.
//!
//! ## Decode is graceful, always
//!
//! [`decode_meta_tag`] never returns an error and never panics: the tag value is
//! repository data, writable by anyone holding repository credentials (a foreign
//! cluster, a NAS admin, a compromised replication peer). A malformed value must
//! degrade to [`MetaTagDecode::Malformed`] — one aggregated per-scan count, never
//! a poisoned catalog scan or a per-entry log line a foreign writer could
//! amplify.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The kopia snapshot-tag key kopiur records its metadata under. Deliberately
/// colon-free: kopia splits `--tags` on the FIRST colon, so a colon-free key is
/// collision-proof against the legacy `kopiur:config:<name>` tag (stored by
/// kopia as manifest key `tag:kopiur`, value `config:<name>`) and can never trip
/// kopia's duplicate-tag-key create failure.
pub const KOPIUR_META_TAG: &str = "kopiur-meta";

/// The current (and only) `kopiur-meta` schema version. See the module docs for
/// the write policy: writers emit the lowest schema representing the data.
pub const KOPIUR_META_SCHEMA_V1: i64 = 1;

/// Where the recorded mover identity came from — which layer of the
/// security-context ladder pinned the effective UID this run recorded.
///
/// Only [`RecordedSrc::Inherited`] means the identity actually **tracked the
/// workload** (it was read from a live workload pod and survived the merge). A
/// restore consuming recorded metadata keys its honesty on this: an
/// `explicit`/`defaults` identity is reproduced faithfully but was never
/// workload-derived.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RecordedSrc {
    /// The identity was inherited from a live workload pod
    /// (`inheritSecurityContextFrom`) and is the one the mover ran as.
    Inherited,
    /// The recipe's explicit `mover.securityContext`/`podSecurityContext`
    /// pinned the identity (including the inherit-fallback case, where the
    /// explicit context stood in for an unresolvable workload).
    Explicit,
    /// The identity came from lower layers (the repository's `moverDefaults`
    /// or the hardened base), or nothing pinned one at all.
    Defaults,
    /// A provenance string this operator version does not know (written by a
    /// newer kopiur). Decodes gracefully instead of poisoning a catalog scan;
    /// never written by this version.
    #[serde(other)]
    Unknown,
}

/// The mover identity recorded on a kopia snapshot at backup time — the decoded
/// value of the [`KOPIUR_META_TAG`] snapshot tag.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordedSnapshotMeta {
    /// The `kopiur-meta` schema version this value was written under. Writers
    /// emit the lowest schema that represents the data (currently
    /// [`KOPIUR_META_SCHEMA_V1`]); readers reject only a schema NEWER than they
    /// understand (degrading to recorded-absent, never an error).
    pub schema: i64,
    /// Which layer pinned the recorded identity. Only `inherited` means the
    /// identity tracked the workload.
    pub src: RecordedSrc,
    /// The resolved effective `runAsUser` the mover ran as at backup time
    /// (container `runAsUser`, else pod). Absent = no layer pinned a UID, so
    /// the mover's UID was image-determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<i64>,
    /// The resolved effective `runAsGroup` the mover ran as at backup time
    /// (container `runAsGroup`, else pod). Absent = image-determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<i64>,
    /// The resolved pod-level `fsGroup` the mover ran with (the hardened
    /// default is `65532`). Absent = no layer set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_group: Option<i64>,
}

/// What [`decode_meta_tag`] found in a snapshot's tags map. Never an `Err`: a
/// bad tag value is repository data and must degrade, not fail a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetaTagDecode {
    /// No `kopiur-meta` tag on the snapshot (pre-feature or foreign backup).
    Absent,
    /// A well-formed, supported-schema value.
    Decoded(RecordedSnapshotMeta),
    /// A well-formed value written under a schema newer than this reader
    /// understands — degrade to recorded-absent (aggregate-count the event).
    UnsupportedSchema {
        /// The schema number the writer declared.
        schema: i64,
    },
    /// The tag value is present but not decodable (bad JSON, missing/invalid
    /// `schema` or `src`). Degrade to recorded-absent (aggregate-count it).
    Malformed {
        /// A short, bounded reason for the scan's aggregated diagnostics.
        reason: String,
    },
}

/// Encode recorded metadata as the compact single-line JSON value stored under
/// the [`KOPIUR_META_TAG`] snapshot tag.
pub fn encode_meta_tag(meta: &RecordedSnapshotMeta) -> String {
    serde_json::to_string(meta).unwrap_or_else(|e| {
        // Serialization of a plain scalar struct cannot fail; defensive only.
        unreachable!("RecordedSnapshotMeta serialization failed: {e}")
    })
}

/// Decode the [`KOPIUR_META_TAG`] value out of a snapshot `tags` map.
///
/// Accepts BOTH the manifest key shape kopia stores (`tag:kopiur-meta`, see
/// [`kopiur-kopia`'s `user_tags`]) and the bare `kopiur-meta` key (already
/// prefix-stripped, or normalized by the mover's result wire) — so every read
/// path funnels through this one decoder regardless of which wire the entry
/// rode. Unknown extra JSON fields are accepted (forward compatibility); a
/// schema newer than [`KOPIUR_META_SCHEMA_V1`] degrades to
/// [`MetaTagDecode::UnsupportedSchema`]; anything undecodable degrades to
/// [`MetaTagDecode::Malformed`]. Never an error, never a panic.
pub fn decode_meta_tag(tags: &BTreeMap<String, String>) -> MetaTagDecode {
    // Prefer the raw manifest key; fall back to the bare (stripped/normalized)
    // key. Both present and disagreeing cannot happen from kopia's own output.
    let prefixed = format!("tag:{KOPIUR_META_TAG}");
    let Some(value) = tags.get(&prefixed).or_else(|| tags.get(KOPIUR_META_TAG)) else {
        return MetaTagDecode::Absent;
    };
    let parsed: serde_json::Value = match serde_json::from_str(value) {
        Ok(v) => v,
        Err(e) => {
            return MetaTagDecode::Malformed {
                reason: format!("invalid JSON: {e}"),
            };
        }
    };
    let Some(schema) = parsed.get("schema").and_then(serde_json::Value::as_i64) else {
        return MetaTagDecode::Malformed {
            reason: "missing or non-integer `schema` field".to_string(),
        };
    };
    if schema > KOPIUR_META_SCHEMA_V1 {
        return MetaTagDecode::UnsupportedSchema { schema };
    }
    match serde_json::from_value::<RecordedSnapshotMeta>(parsed) {
        Ok(meta) => MetaTagDecode::Decoded(meta),
        Err(e) => MetaTagDecode::Malformed {
            reason: format!("schema {schema} value did not decode: {e}"),
        },
    }
}

/// Truncate `s` to at most `max_bytes` bytes on a `char` boundary (never
/// panics, never splits a multi-byte character). Shared by the catalog's
/// description cap and the mover's result-wire bound — foreign-writer-controlled
/// strings must never 4xx a CR create or inflate the result ConfigMap.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(
        src: RecordedSrc,
        uid: Option<i64>,
        gid: Option<i64>,
        fs: Option<i64>,
    ) -> RecordedSnapshotMeta {
        RecordedSnapshotMeta {
            schema: KOPIUR_META_SCHEMA_V1,
            src,
            uid,
            gid,
            fs_group: fs,
        }
    }

    fn tags_with(key: &str, value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(key.to_string(), value.to_string())])
    }

    #[test]
    fn encode_is_compact_single_line_camel_case() {
        let m = meta(RecordedSrc::Inherited, Some(1000), Some(1000), Some(65532));
        let s = encode_meta_tag(&m);
        assert!(!s.contains('\n'), "single line: {s}");
        assert!(!s.contains(' '), "compact: {s}");
        assert_eq!(
            s,
            r#"{"schema":1,"src":"inherited","uid":1000,"gid":1000,"fsGroup":65532}"#
        );
    }

    #[test]
    fn encode_elides_absent_optionals() {
        let m = meta(RecordedSrc::Defaults, None, None, None);
        assert_eq!(encode_meta_tag(&m), r#"{"schema":1,"src":"defaults"}"#);
    }

    #[test]
    fn round_trips_through_both_key_shapes() {
        let m = meta(RecordedSrc::Explicit, Some(3001), None, Some(3001));
        let value = encode_meta_tag(&m);
        // The raw manifest key shape (`tag:` prefix, kopia 0.23.1-verified).
        let prefixed = tags_with("tag:kopiur-meta", &value);
        assert_eq!(
            decode_meta_tag(&prefixed),
            MetaTagDecode::Decoded(m.clone())
        );
        // The bare key shape (prefix-stripped / mover-normalized wire).
        let bare = tags_with("kopiur-meta", &value);
        assert_eq!(decode_meta_tag(&bare), MetaTagDecode::Decoded(m));
    }

    #[test]
    fn absent_tag_decodes_absent() {
        assert_eq!(decode_meta_tag(&BTreeMap::new()), MetaTagDecode::Absent);
        // Other tags present, ours absent.
        let other = tags_with("tag:kopiur", "config:nightly");
        assert_eq!(decode_meta_tag(&other), MetaTagDecode::Absent);
    }

    #[test]
    fn minimal_v1_value_decodes() {
        let t = tags_with("kopiur-meta", r#"{"schema":1,"src":"inherited"}"#);
        assert_eq!(
            decode_meta_tag(&t),
            MetaTagDecode::Decoded(meta(RecordedSrc::Inherited, None, None, None))
        );
    }

    #[test]
    fn unknown_extra_fields_are_accepted_forward_compat() {
        let t = tags_with(
            "kopiur-meta",
            r#"{"schema":1,"src":"explicit","uid":0,"futureField":{"x":1}}"#,
        );
        assert_eq!(
            decode_meta_tag(&t),
            MetaTagDecode::Decoded(meta(RecordedSrc::Explicit, Some(0), None, None))
        );
    }

    #[test]
    fn unknown_src_string_decodes_to_unknown_not_malformed() {
        let t = tags_with("kopiur-meta", r#"{"schema":1,"src":"workload","uid":7}"#);
        assert_eq!(
            decode_meta_tag(&t),
            MetaTagDecode::Decoded(meta(RecordedSrc::Unknown, Some(7), None, None))
        );
    }

    #[test]
    fn newer_schema_degrades_to_unsupported() {
        let t = tags_with(
            "tag:kopiur-meta",
            r#"{"schema":2,"src":"inherited","uid":0}"#,
        );
        assert_eq!(
            decode_meta_tag(&t),
            MetaTagDecode::UnsupportedSchema { schema: 2 }
        );
    }

    #[test]
    fn malformed_values_degrade_with_a_reason_never_an_error() {
        for (value, why) in [
            ("not json at all", "bad JSON"),
            ("{}", "missing schema"),
            (
                r#"{"schema":"one","src":"inherited"}"#,
                "non-integer schema",
            ),
            (r#"{"schema":1}"#, "missing src"),
            (
                r#"{"schema":1,"src":"inherited","uid":"root"}"#,
                "bad uid type",
            ),
        ] {
            let t = tags_with("kopiur-meta", value);
            match decode_meta_tag(&t) {
                MetaTagDecode::Malformed { reason } => {
                    assert!(!reason.is_empty(), "{why}: reason must be actionable");
                }
                other => panic!("{why}: expected Malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn src_serializes_lowercase_and_unknown_round_trips_as_a_string() {
        assert_eq!(
            serde_json::to_value(RecordedSrc::Inherited).unwrap(),
            "inherited"
        );
        assert_eq!(
            serde_json::to_value(RecordedSrc::Explicit).unwrap(),
            "explicit"
        );
        assert_eq!(
            serde_json::to_value(RecordedSrc::Defaults).unwrap(),
            "defaults"
        );
        // A decoded-Unknown stored on status must survive the CRD schema, so it
        // serializes as its own canonical string.
        assert_eq!(
            serde_json::to_value(RecordedSrc::Unknown).unwrap(),
            "unknown"
        );
        let back: RecordedSrc = serde_json::from_value(serde_json::json!("unknown")).unwrap();
        assert_eq!(back, RecordedSrc::Unknown);
    }

    #[test]
    fn truncate_utf8_is_char_boundary_safe() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
        assert_eq!(truncate_utf8("hello", 3), "hel");
        // 'é' is 2 bytes; cutting mid-char backs off to the boundary.
        let s = "aé"; // bytes: a=1, é=2 → len 3
        assert_eq!(truncate_utf8(s, 2), "a");
        assert_eq!(truncate_utf8(s, 3), "aé");
        assert_eq!(truncate_utf8("日本語", 4), "日"); // 3-byte chars
        assert_eq!(truncate_utf8("", 0), "");
    }
}
