//! `GET /api/v1/doctor` — the same checks `kubectl kopiur doctor` runs, served
//! out of `kopiur_ops::doctor` so the CLI and the UI can never disagree about
//! whether a cluster is healthy.
