#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod adoption;
pub mod cache;
pub mod catalog;
pub mod cluster_repository;
pub mod config;
pub mod consts;
pub mod context;
mod controllers;
pub mod error;
pub mod expand;
pub mod health;
pub mod hooks;
mod http;
pub mod io;
pub mod jobs;
pub mod kube_metrics;
pub mod leader;
pub mod maintenance;
pub mod metrics;
pub mod naming;
pub mod pool;
pub mod replication_run;
pub mod repo_seed;
pub mod repository;
pub mod repository_replication;
pub mod restore;
pub mod server;
pub mod snapshot;
pub mod snapshot_policy;
pub mod snapshot_replication;
pub mod snapshot_schedule;
mod startup;
pub mod sweep;
pub mod verification;
pub mod watch;
pub mod webhook_tls;

pub use startup::run;

/// A `stream` source fixture shared by the #451 identity-site tests across
/// modules (`snapshot_policy`, `verification`, `snapshot::build`). Built through
/// the YAML→JSON→typed bridge the API server uses, so the schema defaults
/// (`readOnly`, `sourcePathStrategy`) are materialized exactly as a real cluster
/// delivers them — a hand-built literal cannot reproduce that.
#[cfg(test)]
pub(crate) fn testutil_stream_source() -> kopiur_api::snapshot_policy::Source {
    serde_json::from_value(serde_json::json!({
        "stream": {
            "fileName": "postgres.sql",
            "workloadExec": {
                "podSelector": { "matchLabels": { "app": "postgres" } },
                "command": ["sh", "-ec", "pg_dumpall"],
            },
        },
        "readOnly": true,
        "sourcePathStrategy": "PvcName",
    }))
    .expect("a valid stream Source")
}
