//! The in-cluster session transport.
//!
//! The session Job, its readiness wait, and the read-only exec plane live in
//! [`kopiur_ops::browse::session`] (the web UI opens the same session), and are
//! re-exported here so every CLI call site keeps reading `browse::session::…`.
//! What stays CLI-shaped is the progress reporting: a person at a terminal
//! wants to see that a cold session is being started and to watch it connect.

pub use kopiur_ops::browse::session::*;

/// Reports session progress on **stderr**, so it never contaminates the piped
/// stdout that carries listings and file bytes. Holds the repository name
/// because the start line names it — `ensure` only knows the Job.
pub struct StderrProgress {
    repository: String,
}

impl StderrProgress {
    /// Report progress for a session opened against `repository`.
    pub fn for_repository(repository: &str) -> Self {
        StderrProgress {
            repository: repository.to_string(),
        }
    }
}

impl SessionProgress for StderrProgress {
    fn reusing(&self, job: &str) {
        eprintln!("reusing warm browse session {job}");
    }

    fn starting(&self, job: &str) {
        eprintln!(
            "starting browse session {job} (repository {})…",
            self.repository
        );
    }

    fn joining(&self, _job: &str) {
        eprintln!("a concurrent command already started this session; joining it");
    }

    fn waiting(&self, _job: &str) {
        // No newline: the readiness ticks below draw a progress line.
        eprint!("waiting for the session pod to connect (read-only)");
    }

    fn waiting_tick(&self) {
        eprint!(".");
    }

    fn waiting_done(&self) {
        eprintln!();
    }
}
