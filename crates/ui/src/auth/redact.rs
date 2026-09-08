//! Keeping credentials out of what the UI hands back or logs.
//!
//! Inbound `Cookie`, `Authorization` and `X-Forwarded-Access-Token` are stripped
//! before any handler sees them — a handler that cannot read a credential cannot
//! leak one — and secret-shaped tokens are masked out of the pod log tails quoted
//! into problem responses.
