//! Real-kopia pin for the catalog window + membership digest (issue #476).
//!
//! Gated like `crates/kopia/tests/integration_roundtrip.rs`: `#[ignore]` unless
//! the `integration` feature is on, so hermetic `cargo test` never invokes the
//! real binary. Needs only a `kopia` on `PATH` (mise pins 0.23.1) — no cluster.
//!
//! ```text
//! cargo test -p kopiur-mover --features integration --test integration_catalog
//! ```
//!
//! It pins the two facts `prepare_catalog_entries` is built on:
//! (a) `snapshot list --json --all` groups by source and lists each source
//!     OLDEST first — which is why a bare truncation kept the oldest history;
//! (b) against that real order, a small cap still returns every identity's
//!     newest snapshots, and the digest holds every listed id, including those
//!     written under a different `user@host`.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};

use kopiur_kopia::{ConnectSpec, KopiaClient, SnapshotListEntry};
use kopiur_mover::bootstrap::prepare_catalog_entries;
use kopiur_mover::digest::ListedIds;

/// A client whose env isolates kopia state inside `config_dir`, so the test
/// never touches the user's real `~/.config/kopia`.
fn isolated_client(config_dir: &std::path::Path) -> KopiaClient {
    KopiaClient::builder()
        .binary("kopia")
        .env("KOPIA_PASSWORD", "test1234")
        .env(
            "KOPIA_CONFIG_PATH",
            config_dir.join("repository.config").display().to_string(),
        )
        .env(
            "KOPIA_CACHE_DIRECTORY",
            config_dir.join("cache").display().to_string(),
        )
        .env(
            "KOPIA_LOG_DIR",
            config_dir.join("logs").display().to_string(),
        )
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .build()
}

const IDENTITIES: [&str; 2] = ["zed@host-b:/data", "amy@host-a:/data"];
const PER_IDENTITY: usize = 3;

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn real_listing_order_and_the_fair_window_with_digest() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let client = isolated_client(config_dir.path());
    client
        .repository_create(
            &ConnectSpec::Filesystem {
                path: repo_dir.path().to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("repository create");

    // Interleave the two identities in time, changing the content every run
    // so each snapshot is a distinct manifest.
    let mut created: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for round in 0..PER_IDENTITY {
        for ident in IDENTITIES {
            std::fs::write(
                source_dir.path().join("f.txt"),
                format!("{ident} round {round}\n"),
            )
            .unwrap();
            let snap = client
                .snapshot_create(
                    source_dir.path().to_str().unwrap(),
                    &BTreeMap::new(),
                    Some(ident),
                )
                .await
                .expect("snapshot create");
            created.entry(ident).or_default().push(snap.id);
        }
    }

    // (a) The real order: grouped by source, ascending end time within one.
    let listing = client
        .snapshot_list_all()
        .await
        .expect("snapshot list --all");
    assert_eq!(listing.len(), IDENTITIES.len() * PER_IDENTITY);
    let mut seen_sources: Vec<String> = Vec::new();
    for pair in listing.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.source == b.source {
            assert!(
                a.end_time <= b.end_time,
                "within a source kopia lists oldest first: {} then {}",
                a.id,
                b.id
            );
        }
    }
    for e in &listing {
        let ident = e.source.identity();
        if seen_sources.last() != Some(&ident) {
            assert!(
                !seen_sources.contains(&ident),
                "kopia groups by source; {ident} reappeared after another source"
            );
            seen_sources.push(ident);
        }
    }
    for ident in IDENTITIES {
        let ids: Vec<&str> = listing
            .iter()
            .filter(|e| e.source.identity() == ident)
            .map(|e| e.id.as_str())
            .collect();
        let want: Vec<&str> = created[ident].iter().map(String::as_str).collect();
        assert_eq!(ids, want, "{ident}: listed oldest first, in creation order");
    }

    // (b) A cap of 2 over the real order returns each identity's NEWEST — a
    // bare truncation would have returned the first-listed source's two oldest.
    let prepared = prepare_catalog_entries(listing.clone(), None, IDENTITIES.len(), 42);
    assert!(prepared.truncated);
    let window: BTreeSet<&str> = prepared.entries.iter().map(|e| e.id.as_str()).collect();
    let newest: BTreeSet<&str> = IDENTITIES
        .iter()
        .map(|ident| created[ident].last().unwrap().as_str())
        .collect();
    assert_eq!(window, newest);

    let Some(ListedIds::Digest(digest)) = &prepared.listed_ids else {
        panic!("a digest is built on every scan: {:?}", prepared.listed_ids)
    };
    assert_eq!(digest.count, listing.len() as u64);
    let decoded = digest.decode().expect("the mover's own digest decodes");
    assert!(
        listing
            .iter()
            .all(|e: &SnapshotListEntry| decoded.contains(&e.id)),
        "every listed id — both identities, in the window or not — is a member"
    );
    assert!(!decoded.contains("k0000000000000000000000000000dead"));
}
