//! Turning failures into `application/problem+json`.
//!
//! Every endpoint answers errors with one shape — [`kopiur_ui_model::problem::Problem`]
//! — carrying kopiur's what/why/fix triple, so the SPA can render a remediation
//! instead of a bare status code.
