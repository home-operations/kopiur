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
