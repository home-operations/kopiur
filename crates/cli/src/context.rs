//! Kubernetes client construction honoring the same configuration sources
//! kubectl uses: `--kubeconfig`/`--context` flags, `$KUBECONFIG`, the default
//! kubeconfig file, or in-cluster service-account credentials.
//!
//! The context type itself lives in `kopiur-ops` (the web UI builds one from
//! in-cluster credentials instead); only the kubeconfig path is CLI-specific.

use kube::Client;
use kube::config::{KubeConfigOptions, Kubeconfig};

use crate::cli::GlobalArgs;
use crate::error::CliError;

pub use kopiur_ops::ctx::{OpsCtx as KubeCtx, Scope};

/// Build the client from the global flags. Resolution order matches kubectl:
/// an explicit `--kubeconfig`/`--context` wins, then `Config::infer()` covers
/// `$KUBECONFIG` → `~/.kube/config` → in-cluster.
pub async fn connect(global: &GlobalArgs) -> Result<KubeCtx, CliError> {
    // The webhook/mover do the same: ring may already be installed; ignore the error.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = match (&global.kubeconfig, &global.context) {
        (None, None) => kube::Config::infer()
            .await
            .map_err(|e| CliError::KubeConfig { source: e.into() })?,
        (path, context) => {
            let kubeconfig = match path {
                Some(p) => Kubeconfig::read_from(p)
                    .map_err(|e| CliError::KubeConfig { source: e.into() })?,
                None => {
                    Kubeconfig::read().map_err(|e| CliError::KubeConfig { source: e.into() })?
                }
            };
            let options = KubeConfigOptions {
                context: context.clone(),
                cluster: None,
                user: None,
            };
            kube::Config::from_custom_kubeconfig(kubeconfig, &options)
                .await
                .map_err(|e| CliError::KubeConfig { source: e.into() })?
        }
    };

    let default_namespace = config.default_namespace.clone();
    let client = Client::try_from(config).map_err(|e| CliError::KubeConfig { source: e.into() })?;

    let namespace = global.namespace.clone().unwrap_or(default_namespace);
    let scope = if global.all_namespaces {
        Scope::All
    } else {
        Scope::Namespace(namespace.clone())
    };

    Ok(KubeCtx {
        client,
        namespace,
        scope,
    })
}
