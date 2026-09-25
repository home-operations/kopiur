//! A compact, one-sided membership set of snapshot ids (issue #476).
//!
//! The bootstrap mover lists the WHOLE repository but can only ship a capped
//! window of entries back through its result `ConfigMap` (etcd's ~1 MiB object
//! limit — see [`crate::bootstrap::MAX_RETURNED_SNAPSHOTS`]). Without more, the
//! controller cannot tell a snapshot that fell outside the window from one that
//! was deleted repository-side, so it had to disable absence expiry on every
//! large repository and stale discovered `Snapshot` CRs piled up forever.
//!
//! [`SnapshotIdDigest`] answers the missing question — "is this id still in the
//! repository?" — for every listed id in a few bytes each: a salted FNV-1a-64
//! hash of the id, truncated to `width` bytes, sorted, packed and base64'd.
//!
//! ## Why the error is safe
//!
//! Truncated hashes can collide, so [`DecodedDigest::contains`] may answer
//! `true` for an id that is NOT listed (a false positive), but it can never
//! answer `false` for one that IS. The controller only ever acts on `false`
//! (expire a row whose snapshot is gone), so a collision can only *keep* a row
//! one scan longer — never delete one wrongly. The salt re-rolls every run, so a
//! collision does not persist across scans.
//!
//! That guarantee only holds for a digest that decodes to exactly what the mover
//! built. A digest that decodes but is wrong (unsorted, short, skewed) would
//! produce false NEGATIVES — the unsafe direction — so [`SnapshotIdDigest::decode`]
//! validates the shape strictly and the controller falls back to "membership
//! unknown" on any error rather than trusting it.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Byte budget for the encoded `hashes` string. Reserved inside the result
/// `ConfigMap` budget ([`crate::bootstrap::RESULT_SIZE_BUDGET_BYTES`]) ahead of
/// the materialization entries, which are the part that gets trimmed.
pub const DIGEST_BUDGET_BYTES: usize = 448 * 1024;

/// Allowed hash widths in bytes, widest first. [`SnapshotIdDigest::build`] picks
/// the widest that fits the budget: 8 bytes holds ≈43k ids, 4 bytes ≈86k.
///
/// There is deliberately no 3-byte width: with `catalog.periodicRefresh` off
/// (the default) a false positive persists until the next spec-change or
/// on-demand scan, and at 3 bytes a repository near the ceiling would keep
/// hundreds of stale rows that way. Past the 4-byte ceiling the digest is
/// omitted and the controller surfaces the repository as `Partial`.
pub const DIGEST_WIDTHS: [u8; 3] = [8, 6, 4];

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The hash construction a digest was built with. Carried on the wire so a
/// future algorithm change can never be silently mis-read as the old one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestAlgo {
    /// FNV-1a-64 over `salt.to_le_bytes() ‖ id`, big-endian, truncated to `width`.
    Fnv1a64V1,
    /// An algorithm this build does not know (a newer mover). Never decodable.
    Unknown(String),
}

impl DigestAlgo {
    const FNV1A64_V1: &'static str = "fnv1a64-v1";

    fn label(&self) -> &str {
        match self {
            DigestAlgo::Fnv1a64V1 => Self::FNV1A64_V1,
            DigestAlgo::Unknown(raw) => raw,
        }
    }
}

impl Serialize for DigestAlgo {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for DigestAlgo {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            Self::FNV1A64_V1 => DigestAlgo::Fnv1a64V1,
            _ => DigestAlgo::Unknown(raw),
        })
    }
}

/// The wire form: a membership set over every snapshot id the mover listed
/// (after the foreign-suffix prefilter, before the materialization cap).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotIdDigest {
    /// Hash construction.
    pub algo: DigestAlgo,
    /// Per-run salt mixed into every hash.
    pub salt: u64,
    /// Bytes kept per hash (one of [`DIGEST_WIDTHS`]).
    pub width: u8,
    /// Number of ids hashed. Duplicate hashes are kept, so the packed length is
    /// exactly `count * width` — the controller cross-checks this against the
    /// listing's own count before trusting the digest.
    pub count: u64,
    /// Base64 (standard, padded) of the sorted, concatenated hash prefixes.
    pub hashes: String,
}

/// Why a digest could not be trusted. Every variant means "membership unknown":
/// the controller keeps absence expiry off rather than risk a false "absent".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    /// Written by a mover with a hash construction this controller doesn't know.
    #[error("unknown digest algorithm {0:?} (mover newer than controller?)")]
    UnknownAlgo(String),
    /// Width outside [`DIGEST_WIDTHS`].
    #[error("unsupported digest width {0}")]
    BadWidth(u8),
    /// `hashes` is not valid base64.
    #[error("digest hashes are not valid base64: {0}")]
    Base64(String),
    /// The packed length disagrees with `count * width`.
    #[error("digest holds {bytes} bytes, expected {count} ids x {width} bytes")]
    LengthMismatch {
        /// Decoded byte length.
        bytes: usize,
        /// Claimed id count.
        count: u64,
        /// Claimed width.
        width: u8,
    },
    /// Prefixes are not in ascending order, so binary search would lie.
    #[error("digest hashes are not sorted")]
    Unsorted,
    /// `listedIds` was present but was not a digest at all ([`ListedIds::Invalid`]).
    #[error("listedIds is not a well-formed digest: {0}")]
    Malformed(String),
}

/// The salted hash of one id, big-endian so byte order == numeric order.
fn salted_hash(salt: u64, id: &str) -> [u8; 8] {
    let mut h = FNV_OFFSET;
    for b in salt.to_le_bytes().iter().chain(id.as_bytes()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h.to_be_bytes()
}

/// Encoded (base64, padded) length of `count` hashes at `width` bytes.
fn encoded_len(count: usize, width: u8) -> usize {
    (count * usize::from(width)).div_ceil(3) * 4
}

impl SnapshotIdDigest {
    /// Build a digest over `ids` with the widest width in [`DIGEST_WIDTHS`] whose
    /// encoding fits `budget_bytes`; `None` when even the narrowest does not fit.
    /// Pure — the caller supplies the per-run `salt`.
    pub fn build<'a>(
        ids: impl IntoIterator<Item = &'a str>,
        salt: u64,
        budget_bytes: usize,
    ) -> Option<Self> {
        let mut full: Vec<[u8; 8]> = ids.into_iter().map(|id| salted_hash(salt, id)).collect();
        let width = DIGEST_WIDTHS
            .into_iter()
            .find(|w| encoded_len(full.len(), *w) <= budget_bytes)?;
        // Sorting the full 8-byte hashes also sorts every prefix of them.
        full.sort_unstable();
        let w = usize::from(width);
        let packed: Vec<u8> = full.iter().flat_map(|h| h[..w].iter().copied()).collect();
        Some(SnapshotIdDigest {
            algo: DigestAlgo::Fnv1a64V1,
            salt,
            width,
            count: full.len() as u64,
            hashes: STANDARD.encode(packed),
        })
    }

    /// Validate and unpack for membership queries. Strict on purpose: any shape
    /// error is returned rather than tolerated, because a corrupt digest could
    /// answer "absent" for a listed id (see the module docs).
    pub fn decode(&self) -> Result<DecodedDigest, DigestError> {
        match &self.algo {
            DigestAlgo::Fnv1a64V1 => {}
            DigestAlgo::Unknown(raw) => return Err(DigestError::UnknownAlgo(raw.clone())),
        }
        if !DIGEST_WIDTHS.contains(&self.width) {
            return Err(DigestError::BadWidth(self.width));
        }
        let bytes = STANDARD
            .decode(&self.hashes)
            .map_err(|e| DigestError::Base64(e.to_string()))?;
        let w = usize::from(self.width);
        let expected = usize::try_from(self.count)
            .ok()
            .and_then(|c| c.checked_mul(w));
        if expected != Some(bytes.len()) {
            return Err(DigestError::LengthMismatch {
                bytes: bytes.len(),
                count: self.count,
                width: self.width,
            });
        }
        if bytes
            .chunks_exact(w)
            .zip(bytes.chunks_exact(w).skip(1))
            .any(|(a, b)| a > b)
        {
            return Err(DigestError::Unsorted);
        }
        Ok(DecodedDigest {
            salt: self.salt,
            width: w,
            bytes,
        })
    }
}

/// A validated digest, ready for O(log n) membership queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedDigest {
    salt: u64,
    width: usize,
    bytes: Vec<u8>,
}

impl DecodedDigest {
    /// Number of ids in the set.
    pub fn len(&self) -> usize {
        self.bytes.len() / self.width
    }

    /// `true` when the set holds no ids.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// `false` is exact: the id was NOT in the listing. `true` means it was —
    /// or, rarely, that its truncated hash collides with one that was.
    pub fn contains(&self, id: &str) -> bool {
        let needle = salted_hash(self.salt, id);
        let needle = &needle[..self.width];
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let at = &self.bytes[mid * self.width..(mid + 1) * self.width];
            match at.cmp(needle) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }
}

/// [`crate::bootstrap::BootstrapResult::listed_ids`] as the controller reads it.
///
/// Deserialization is lenient: a value that is not a well-formed
/// [`SnapshotIdDigest`] becomes [`ListedIds::Invalid`] instead of failing the
/// WHOLE bootstrap result (which the controller treats as an invariant breach
/// and would wedge the repository). `Invalid` then decodes to "membership
/// unknown", exactly like a digest that fails [`SnapshotIdDigest::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListedIds {
    /// A structurally well-formed digest (still subject to `decode`).
    Digest(SnapshotIdDigest),
    /// The field was present but not a digest; carries the parse error. The
    /// mover never writes this — it only exists on the read side. It
    /// serializes as that error string, which re-reads as `Invalid` again.
    Invalid(String),
}

impl ListedIds {
    /// Validate for membership queries; `Invalid` is an error like any other.
    pub fn decode(&self) -> Result<DecodedDigest, DigestError> {
        match self {
            ListedIds::Digest(d) => d.decode(),
            ListedIds::Invalid(reason) => Err(DigestError::Malformed(reason.clone())),
        }
    }
}

impl Serialize for ListedIds {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ListedIds::Digest(d) => d.serialize(serializer),
            ListedIds::Invalid(reason) => serializer.serialize_str(reason),
        }
    }
}

impl<'de> Deserialize<'de> for ListedIds {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match SnapshotIdDigest::deserialize(value) {
            Ok(d) => ListedIds::Digest(d),
            Err(e) => ListedIds::Invalid(e.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        // Kopia manifest ids are 16 random bytes, hex — mimic the shape.
        (0..n)
            .map(|i| {
                format!(
                    "{:032x}",
                    (i as u128).wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835)
                )
            })
            .collect()
    }

    fn built(n: usize, salt: u64, budget: usize) -> (Vec<String>, SnapshotIdDigest) {
        let v = ids(n);
        let d = SnapshotIdDigest::build(v.iter().map(String::as_str), salt, budget)
            .expect("fits the budget");
        (v, d)
    }

    #[test]
    fn every_listed_id_is_contained() {
        let (v, d) = built(5_000, 7, DIGEST_BUDGET_BYTES);
        let dec = d.decode().expect("decodes");
        assert_eq!(dec.len(), 5_000);
        assert!(
            v.iter().all(|id| dec.contains(id)),
            "no false negatives, ever"
        );
    }

    #[test]
    fn unlisted_ids_are_absent_at_full_width() {
        let (v, d) = built(5_000, 7, DIGEST_BUDGET_BYTES);
        assert_eq!(d.width, 8);
        let dec = d.decode().unwrap();
        let others: Vec<String> = (0..5_000).map(|i| format!("{i:032x}-gone")).collect();
        assert!(others.iter().all(|id| !v.contains(id)));
        assert_eq!(others.iter().filter(|id| dec.contains(id)).count(), 0);
    }

    #[test]
    fn empty_digest_contains_nothing() {
        let d = SnapshotIdDigest::build(std::iter::empty(), 1, DIGEST_BUDGET_BYTES).unwrap();
        assert_eq!(d.count, 0);
        let dec = d.decode().unwrap();
        assert!(dec.is_empty());
        assert!(!dec.contains("6aa8bb06201863273b08f37977182fbd"));
    }

    #[test]
    fn width_narrows_to_fit_the_budget_then_gives_up() {
        let n = 1_000;
        assert_eq!(built(n, 1, encoded_len(n, 8)).1.width, 8);
        assert_eq!(built(n, 1, encoded_len(n, 8) - 1).1.width, 6);
        assert_eq!(built(n, 1, encoded_len(n, 6) - 1).1.width, 4);
        let v = ids(n);
        assert!(
            SnapshotIdDigest::build(v.iter().map(String::as_str), 1, encoded_len(n, 4) - 1)
                .is_none(),
            "below the narrowest width the digest is omitted, never lossy"
        );
    }

    #[test]
    fn the_budget_holds_the_documented_ceilings() {
        assert!(encoded_len(43_000, 8) <= DIGEST_BUDGET_BYTES);
        assert!(encoded_len(86_000, 4) <= DIGEST_BUDGET_BYTES);
        assert!(encoded_len(90_000, 4) > DIGEST_BUDGET_BYTES);
    }

    #[test]
    fn narrow_width_keeps_every_member_and_count() {
        let (v, d) = built(2_000, 3, encoded_len(2_000, 4));
        assert_eq!(d.width, 4);
        assert_eq!(d.count, 2_000);
        let dec = d.decode().unwrap();
        assert!(v.iter().all(|id| dec.contains(id)));
    }

    #[test]
    fn salt_changes_the_hashes() {
        let (_, a) = built(100, 1, DIGEST_BUDGET_BYTES);
        let (_, b) = built(100, 2, DIGEST_BUDGET_BYTES);
        assert_ne!(a.hashes, b.hashes);
    }

    #[test]
    fn wire_round_trip_is_camel_case_and_lossless() {
        let (_, d) = built(10, 42, DIGEST_BUDGET_BYTES);
        let json = serde_json::to_value(ListedIds::Digest(d.clone())).unwrap();
        assert_eq!(json["algo"], "fnv1a64-v1");
        assert_eq!(json["salt"], 42);
        let back: ListedIds = serde_json::from_value(json).unwrap();
        assert_eq!(back, ListedIds::Digest(d));
    }

    #[test]
    fn a_malformed_value_decodes_as_invalid_not_a_parse_error() {
        let back: ListedIds = serde_json::from_value(serde_json::json!({"algo": 7})).unwrap();
        assert!(matches!(back, ListedIds::Invalid(_)), "{back:?}");
        assert!(
            matches!(back.decode(), Err(DigestError::Malformed(_))),
            "an invalid listedIds names itself, not a base64 fault"
        );
        let back: ListedIds = serde_json::from_value(serde_json::json!("nonsense")).unwrap();
        assert!(matches!(back, ListedIds::Invalid(_)));
    }

    #[test]
    fn unknown_algo_round_trips_and_refuses_to_decode() {
        let (_, mut d) = built(10, 1, DIGEST_BUDGET_BYTES);
        d.algo = DigestAlgo::Unknown("xxh3-v9".into());
        let back: SnapshotIdDigest =
            serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert_eq!(back.algo, DigestAlgo::Unknown("xxh3-v9".into()));
        assert_eq!(
            back.decode(),
            Err(DigestError::UnknownAlgo("xxh3-v9".into()))
        );
    }

    #[test]
    fn decode_rejects_every_malformed_shape() {
        let (_, good) = built(50, 1, DIGEST_BUDGET_BYTES);

        let mut d = good.clone();
        d.width = 5;
        assert_eq!(d.decode(), Err(DigestError::BadWidth(5)));

        let mut d = good.clone();
        d.hashes = "!!!not base64".into();
        assert!(matches!(d.decode(), Err(DigestError::Base64(_))));

        let mut d = good.clone();
        d.count = 49;
        assert!(matches!(
            d.decode(),
            Err(DigestError::LengthMismatch { .. })
        ));

        let mut d = good.clone();
        let mut bytes = STANDARD.decode(&d.hashes).unwrap();
        bytes.truncate(bytes.len() - 3);
        d.hashes = STANDARD.encode(bytes);
        assert!(matches!(
            d.decode(),
            Err(DigestError::LengthMismatch { .. })
        ));

        let mut d = good;
        let mut bytes = STANDARD.decode(&d.hashes).unwrap();
        let chunks: Vec<Vec<u8>> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .rev()
            .map(|c| c.to_vec())
            .collect();
        bytes = chunks.concat();
        d.hashes = STANDARD.encode(bytes);
        assert_eq!(d.decode(), Err(DigestError::Unsorted));
    }

    #[test]
    fn duplicate_ids_are_counted_not_collapsed() {
        let d = SnapshotIdDigest::build(["a", "a", "b"], 9, DIGEST_BUDGET_BYTES).unwrap();
        assert_eq!(d.count, 3);
        let dec = d.decode().expect("equal neighbours are sorted");
        assert!(dec.contains("a") && dec.contains("b"));
    }
}
