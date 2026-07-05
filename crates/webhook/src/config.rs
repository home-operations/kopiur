//! The webhook's runtime configuration: the clap surface ([`WebhookArgs`])
//! plus the names of every environment variable it reads. Every knob is a
//! `--flag` with its env var as fallback (flag > env > default); the env names
//! are the chart contract (`webhook-deployment.tpl`) and must never change.

use std::net::SocketAddr;

use clap::Parser;

/// Address the webhook server binds to.
pub const WEBHOOK_ADDR_ENV: &str = "KOPIUR_WEBHOOK_ADDR";
/// PEM cert chain path; presence (with the key) enables TLS.
pub const WEBHOOK_TLS_CERT_ENV: &str = "KOPIUR_WEBHOOK_TLS_CERT";
/// PEM private key path.
pub const WEBHOOK_TLS_KEY_ENV: &str = "KOPIUR_WEBHOOK_TLS_KEY";

/// Default bind address when [`WEBHOOK_ADDR_ENV`] is unset (k8s requires HTTPS
/// for admission; the chart maps Service 443 → this container port).
pub const DEFAULT_ADDR: &str = "0.0.0.0:8443";

/// How often the TLS server re-reads its cert/key files so an operator-rotated
/// serving leaf (the `webhook.tls.mode: self` path — the controller rewrites the
/// mounted Secret) is picked up without a pod restart. Rotation is rare and the
/// reload is a cheap PEM read, so a calm cadence is plenty.
pub const TLS_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// The webhook's command-line/environment surface (flag > env > default).
///
/// No empty-string filtering is needed here: the chart hardcodes the TLS paths
/// (`/tls/tls.crt`, `/tls/tls.key`), and an empty path fails at
/// `RustlsConfig::from_pem_file` exactly like the pre-clap env reads did.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "kopiur-webhook",
    version,
    about = "Kopiur admission webhook server"
)]
pub struct WebhookArgs {
    /// Bind address for the admission server.
    #[arg(long, env = WEBHOOK_ADDR_ENV, default_value = DEFAULT_ADDR, value_parser = parse_webhook_addr)]
    pub addr: SocketAddr,

    /// PEM cert chain path; together with --tls-key, enables HTTPS.
    #[arg(long, env = WEBHOOK_TLS_CERT_ENV)]
    pub tls_cert: Option<String>,

    /// PEM private key path; together with --tls-cert, enables HTTPS.
    #[arg(long, env = WEBHOOK_TLS_KEY_ENV)]
    pub tls_key: Option<String>,
}

/// Value parser for [`WEBHOOK_ADDR_ENV`]/`--addr`. A typo'd bind address must
/// fail loudly at startup with the what/why/fix, mirroring the controller's
/// `KOPIUR_HTTP_ADDR` contract — never silently bind the default.
fn parse_webhook_addr(value: &str) -> Result<SocketAddr, String> {
    value.parse::<SocketAddr>().map_err(|_| {
        format!(
            "KOPIUR_WEBHOOK_ADDR='{value}' is not a valid socket address; use host:port, e.g. \
             0.0.0.0:8443 (IPv4), [::]:8443 (IPv6/dual-stack); unset it to use the default \
             0.0.0.0:8443"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Every parsing test is `#[serial]`: clap consults the process env for
    // every `env = ...` field on every parse, so a test that mutates the env
    // could poison a concurrently-running parse (the repo's env-test idiom).

    fn parse(args: &[&str]) -> WebhookArgs {
        WebhookArgs::try_parse_from(std::iter::once("kopiur-webhook").chain(args.iter().copied()))
            .expect("args must parse")
    }

    #[test]
    #[serial]
    fn defaults_serve_plain_http_on_the_chart_port() {
        let args = parse(&[]);
        assert_eq!(args.addr, DEFAULT_ADDR.parse().unwrap());
        assert_eq!(args.tls_cert, None);
        assert_eq!(args.tls_key, None);
    }

    #[test]
    #[serial]
    fn flags_round_trip() {
        let args = parse(&[
            "--addr",
            "[::]:9443",
            "--tls-cert",
            "/tls/tls.crt",
            "--tls-key",
            "/tls/tls.key",
        ]);
        assert_eq!(args.addr, "[::]:9443".parse().unwrap());
        assert_eq!(args.tls_cert.as_deref(), Some("/tls/tls.crt"));
        assert_eq!(args.tls_key.as_deref(), Some("/tls/tls.key"));
    }

    #[test]
    #[serial]
    fn addr_invalid_value_fails_loudly_with_an_actionable_message() {
        let err = WebhookArgs::try_parse_from(["kopiur-webhook", "--addr", "not-an-addr"])
            .expect_err("garbage KOPIUR_WEBHOOK_ADDR must not silently fall back");
        let msg = err.to_string();
        assert!(msg.contains("KOPIUR_WEBHOOK_ADDR='not-an-addr'"), "{msg}");
        assert!(msg.contains("is not a valid socket address"), "{msg}");
        assert!(msg.contains("0.0.0.0:8443"), "{msg}");
        assert!(msg.contains("[::]:8443"), "{msg}");
        assert!(msg.contains("unset it to use the default"), "{msg}");
    }

    #[test]
    #[serial]
    fn env_value_is_used_when_flag_is_absent() {
        // SAFETY: serialized by #[serial] against every other test in this module.
        unsafe { std::env::set_var(WEBHOOK_ADDR_ENV, "[::]:8443") };
        let args = parse(&[]);
        unsafe { std::env::remove_var(WEBHOOK_ADDR_ENV) };
        assert_eq!(args.addr, "[::]:8443".parse().unwrap());
    }

    #[test]
    #[serial]
    fn flag_beats_env() {
        // SAFETY: serialized by #[serial] against every other test in this module.
        unsafe { std::env::set_var(WEBHOOK_ADDR_ENV, "[::]:8443") };
        let args = parse(&["--addr", "127.0.0.1:9999"]);
        unsafe { std::env::remove_var(WEBHOOK_ADDR_ENV) };
        assert_eq!(args.addr, "127.0.0.1:9999".parse().unwrap());
    }

    // clap derive self-check: catches attribute mistakes (conflicting ids,
    // bad defaults) that only surface at runtime otherwise.
    #[test]
    fn clap_debug_assert() {
        use clap::CommandFactory as _;
        WebhookArgs::command().debug_assert();
    }
}
