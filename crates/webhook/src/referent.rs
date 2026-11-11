//! The single choke point for every point-read the webhook makes of an object
//! the admitted CR **references** (issue #677).
//!
//! Admission must not enforce that a referent exists. GitOps tools apply a
//! bundle in an order of their choosing (Flux sorts by kind, a rename lands the
//! new consumer before the new repository), and `--dry-run=server` runs the
//! webhook against a cluster that has none of the bundle yet. The Kubernetes
//! convention — and kopiur's — is to **admit** a dangling reference and let the
//! controller report it (`MissingDependency`) and retry until the referent
//! appears; the controllers already watch every referent kind and re-enqueue
//! the dependents when one is created.
//!
//! So every lookup returns a three-way [`Referent`], and each caller decides
//! exhaustively what `Missing` means for ITS check — never "deny":
//!
//! - **[`Referent::Found`]**: run the check against the live object.
//! - **[`Referent::Missing`]**: the API server answered 404. Admit; checks that
//!   need the referent are deferred to the controller (tenancy is re-enforced
//!   there — see `kopiur_controller::io::ensure_consumer_tenancy`),
//!   and the caller may attach [`missing_referent_warning`] so `kubectl apply`
//!   tells the user what will happen.
//! - **[`Referent::Unreadable`]**: any OTHER failure (403, 5xx, timeout, no
//!   transport). The caller keeps its pre-existing posture for that check:
//!   fail-closed for the tenancy gate (an RBAC/apiserver fault must not open a
//!   security boundary), fail-open for best-effort checks.
//!
//! ## The ratchet
//!
//! The `no_referent_point_read_bypasses_this_module` test scans every webhook
//! source file and fails the build if any non-test code awaits a kube point-read
//! (`.get(..).await`, `.get_opt(..).await`, …) outside this module. A new lookup
//! site therefore cannot silently reintroduce "NotFound ⇒ deny": it has to come
//! through [`fetch`] and handle `Missing` explicitly.

use std::fmt::Debug;

use kube::Api;
use serde::de::DeserializeOwned;

/// The outcome of reading a referenced object at admission.
#[derive(Debug)]
pub enum Referent<T> {
    /// The object exists; here it is.
    Found(T),
    /// The API server answered `404 NotFound`: the object does not exist (yet).
    Missing,
    /// Any other failure (RBAC, apiserver error, transport). Carries the error so a
    /// fail-closed caller can surface it verbatim.
    Unreadable(kube::Error),
}

impl<T> Referent<T> {
    /// Classify the result of a kube `get`. **Pure** — the one place the
    /// "is this a NotFound?" decision is made, so it is unit-tested directly.
    ///
    /// ```
    /// use kopiur_webhook::referent::Referent;
    /// let missing: Result<(), kube::Error> = Err(kube::Error::Api(
    ///     kube::core::Status::failure("gone", "NotFound").with_code(404).boxed(),
    /// ));
    /// assert!(matches!(Referent::from_get(missing), Referent::Missing));
    /// assert!(matches!(Referent::from_get(Ok::<_, kube::Error>(7)), Referent::Found(7)));
    /// ```
    pub fn from_get(result: Result<T, kube::Error>) -> Self {
        match result {
            Ok(obj) => Referent::Found(obj),
            Err(kube::Error::Api(status)) if status.is_not_found() => Referent::Missing,
            Err(e) => Referent::Unreadable(e),
        }
    }

    /// The object if it was found. For best-effort checks that skip on BOTH
    /// `Missing` and `Unreadable` (their historical `get_opt(..).ok().flatten()`
    /// posture); callers that must tell the two apart `match` instead.
    pub fn found(self) -> Option<T> {
        match self {
            Referent::Found(obj) => Some(obj),
            Referent::Missing | Referent::Unreadable(_) => None,
        }
    }

    /// `true` iff the API server confirmed the object does not exist.
    pub fn is_missing(&self) -> bool {
        matches!(self, Referent::Missing)
    }

    /// Map the found object, preserving `Missing`/`Unreadable`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Referent<U> {
        match self {
            Referent::Found(obj) => Referent::Found(f(obj)),
            Referent::Missing => Referent::Missing,
            Referent::Unreadable(e) => Referent::Unreadable(e),
        }
    }
}

/// Point-read `name` through `api`, classified by [`Referent::from_get`].
///
/// This is the ONLY function in the webhook allowed to await a kube point-read
/// (enforced by the ratchet test below).
pub async fn fetch<K>(api: &Api<K>, name: &str) -> Referent<K>
where
    K: Clone + DeserializeOwned + Debug,
{
    Referent::from_get(api.get(name).await)
}

/// The admission warning for a reference whose target does not exist yet.
///
/// `field` is the spec path of the reference (`spec.sourceRef`), `what` the
/// human name of the target (`ClusterRepository "onsite"`). The text says what
/// was found, that the object was admitted anyway, and what happens next — the
/// what/why/fix rule applied to a non-blocking surface.
///
/// ```
/// let w = kopiur_webhook::referent::missing_referent_warning(
///     "spec.sourceRef", "ClusterRepository \"onsite\"",
/// );
/// assert!(w.starts_with("spec.sourceRef: ClusterRepository \"onsite\" does not exist yet"));
/// assert!(w.contains("MissingDependency"));
/// ```
pub fn missing_referent_warning(field: &str, what: &str) -> String {
    format!(
        "{field}: {what} does not exist yet. Admitted anyway: kopiur reports it as \
         MissingDependency and starts automatically once it is created (checks that need it, \
         including ClusterRepository allowedNamespaces, run then). If this is a typo, fix the \
         reference"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_err(code: u16, reason: &str) -> kube::Error {
        kube::Error::Api(
            kube::core::Status::failure("boom", reason)
                .with_code(code)
                .boxed(),
        )
    }

    #[test]
    fn a_404_is_missing_not_unreadable() {
        assert!(Referent::<()>::from_get(Err(api_err(404, "NotFound"))).is_missing());
        // The reason alone (an apiserver that omits the code) also counts.
        assert!(Referent::<()>::from_get(Err(api_err(0, "NotFound"))).is_missing());
    }

    #[test]
    fn every_other_api_failure_is_unreadable() {
        for (code, reason) in [
            (403, "Forbidden"),
            (500, "InternalError"),
            (503, "ServiceUnavailable"),
            (401, "Unauthorized"),
        ] {
            match Referent::<()>::from_get(Err(api_err(code, reason))) {
                Referent::Unreadable(_) => {}
                other => panic!("{code} {reason} must be Unreadable, got {other:?}"),
            }
        }
    }

    #[test]
    fn found_and_map_preserve_the_three_states() {
        assert_eq!(Referent::Found(2).map(|n| n * 2).found(), Some(4));
        assert!(Referent::<i32>::Missing.map(|n| n * 2).is_missing());
        assert!(
            Referent::<i32>::from_get(Err(api_err(500, "InternalError")))
                .found()
                .is_none()
        );
    }

    // --- the ratchet ------------------------------------------------------------

    /// Byte offset of the first top-level test module, so test code (which
    /// legitimately builds mock clients) is not scanned.
    fn production_part(src: &str) -> &str {
        match src.find("#[cfg(test)]\nmod tests") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// Every `.get*(..)` call in `src` that is immediately awaited — the shape of
    /// a kube point-read (`api.get_opt(&name).await`). Map/JSON `.get(..)` calls are
    /// never awaited, so they are not matched. Returns the offending snippets.
    fn awaited_point_reads(src: &str) -> Vec<String> {
        const METHODS: [&str; 6] = [
            ".get(",
            ".get_opt(",
            ".get_with(",
            ".get_metadata(",
            ".get_metadata_opt(",
            ".get_status(",
        ];
        let bytes = src.as_bytes();
        let mut hits = Vec::new();
        for method in METHODS {
            let mut from = 0;
            while let Some(rel) = src[from..].find(method) {
                let start = from + rel;
                // Walk to the matching close paren.
                let mut depth = 0usize;
                let mut i = start + method.len() - 1; // at '('
                let mut end = None;
                while i < bytes.len() {
                    match bytes[i] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(i + 1);
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if let Some(end) = end
                    && src[end..].trim_start().starts_with(".await")
                {
                    hits.push(src[start..end].to_string());
                }
                from = start + method.len();
            }
        }
        hits
    }

    #[test]
    fn the_ratchet_detector_finds_point_reads_and_ignores_map_gets() {
        let src = "let a = api.get_opt(&repo.name)\n    .await;\n\
                   let b = crepos.get(repo_name).await;\n\
                   let c = labels.get(&k).map(|v| v == x);\n\
                   let d = data.get(\"spec\").cloned();";
        let hits = awaited_point_reads(src);
        assert_eq!(hits, vec![".get(repo_name)", ".get_opt(&repo.name)"]);
    }

    /// THE RATCHET (issue #677): no webhook code may await a kube point-read except
    /// through [`fetch`], which forces the caller to handle [`Referent::Missing`]
    /// explicitly. Before this, the tenancy gate's bare `crepos.get(..)` turned a
    /// not-yet-applied `ClusterRepository` into a hard admission denial and broke
    /// every GitOps apply that ordered the consumer first.
    #[test]
    fn no_referent_point_read_bypasses_this_module() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut scanned = 0;
        for entry in std::fs::read_dir(&dir).expect("read crates/webhook/src") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|f| f.to_str()) == Some("referent.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read source");
            scanned += 1;
            for hit in awaited_point_reads(production_part(&src)) {
                offenders.push(format!("{}: {hit}", path.display()));
            }
        }
        assert!(
            scanned >= 10,
            "the scan must actually cover the webhook sources"
        );
        assert!(
            offenders.is_empty(),
            "kube point-reads must go through `referent::fetch` so a NotFound referent is \
             admitted, not denied (issue #677). Offenders:\n{}",
            offenders.join("\n")
        );
    }
}
