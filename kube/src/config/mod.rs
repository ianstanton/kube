//! In cluster or out of cluster kubeconfig to be used by an api client
//!
//! You primarily want to interact with `Configuration`,
//! and its associated load functions.
//!
//! The full `Config` and child-objects are exposed here for convenience only.

mod apis;
mod exec;
pub mod incluster_config;
pub(crate) mod kube_config;
pub(crate) mod utils;

use crate::config::{self, kube_config::Der};
use crate::{Error, Result};

pub use self::kube_config::ConfigLoader;

/// Configuration stores kubernetes path and client for requests.
#[derive(Clone, Debug)]
pub struct Configuration {
    pub cluster_url: reqwest::Url,
    /// The current default namespace. This will be "default" while running outside of a cluster,
    /// and will be the namespace of the pod while running inside a cluster.
    pub default_ns: String,
}

impl Configuration {
    /// Returns a new configuration based on the provided cluster url
    /// and sets the default namespace to "default"
    pub fn new(cluster_url: reqwest::Url) -> Self {
        Self::new_with_default_namespace(cluster_url, String::from("default"))
    }

    /// Create a new `Configuration` based on a cluster url and default namespace
    pub fn new_with_default_namespace(cluster_url: reqwest::Url, default_ns: String) -> Self {
        Self {
            cluster_url,
            default_ns,
        }
    }

    /// Infer the config type and return it
    ///
    /// Done by attempting to load in-cluster evars first,
    /// then if that fails, try the full local kube config.
    pub async fn infer() -> Result<Self> {
        match Self::new_from_cluster_env() {
            Err(e) => {
                trace!("No in-cluster config found: {}", e);
                trace!("Falling back to local kube config");
                Ok(Self::new_from_kube_config(&ConfigOptions::default()).await?)
            }
            Ok(o) => Ok(o),
        }
    }

    /// Returns a config from kube config file.
    ///
    /// This file is typically found in `~/.kube/config`
    pub async fn new_from_kube_config(options: &ConfigOptions) -> Result<Self> {
        let loader = ConfigLoader::new_from_options(options).await?;
        let url = reqwest::Url::parse(&loader.cluster.server)
            .map_err(|e| Error::KubeConfig(format!("malformed url: {}", e)))?;
        Ok(Self::new(url))
    }

    /// Returns a config which is used by clients within pods on kubernetes
    /// by reading configuration from the environment.
    ///
    /// It will return an error if called from out of kubernetes cluster.
    pub fn new_from_cluster_env() -> Result<Self> {
        let server = incluster_config::kube_server().ok_or_else(|| {
            Error::KubeConfig(format!(
                "Unable to load incluster config, {} and {} must be defined",
                incluster_config::SERVICE_HOSTENV,
                incluster_config::SERVICE_PORTENV
            ))
        })?;

        let url =
            reqwest::Url::parse(&server).map_err(|e| Error::KubeConfig(format!("malformed url: {}", e)))?;

        let default_ns = incluster_config::load_default_ns()
            .map_err(|e| Error::KubeConfig(format!("Unable to load incluster default namespace: {}", e)))?;

        Ok(Self::new_with_default_namespace(url, default_ns))
    }
}

/// ConfigOptions stores options used when loading kubeconfig file.
#[derive(Default, Clone)]
pub struct ConfigOptions {
    pub context: Option<String>,
    pub cluster: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug)]
pub struct ClientConfig {
    pub(crate) cluster_url: reqwest::Url,
    pub(crate) root_cert: Option<reqwest::Certificate>,
    pub(crate) headers: reqwest::header::HeaderMap,
    pub(crate) timeout: Option<std::time::Duration>,
    pub(crate) accept_invalid_certs: bool,
    pub(crate) identity: Option<reqwest::Identity>,
}

impl ClientConfig {
    pub async fn infer() -> Result<Self> {
        let config = Configuration::infer().await?;
        match Self::new_from_cluster_env(config) {
            Err(e) => {
                trace!("No in-cluster config found: {}", e);
                trace!("Falling back to local kube config");
                Ok(Self::new_from_kube_config(&ConfigOptions::default()).await?)
            }
            Ok(o) => Ok(o),
        }
    }

    pub fn new_from_cluster_env(config: Configuration) -> Result<Self> {
        let root_cert = incluster_config::load_cert()?;

        let token = incluster_config::load_token()
            .map_err(|e| Error::KubeConfig(format!("Unable to load in cluster token: {}", e)))?;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
                .map_err(|e| Error::KubeConfig(format!("Invalid bearer token: {}", e)))?,
        );

        Ok(Self {
            cluster_url: config.cluster_url,
            root_cert: Some(root_cert),
            headers,
            timeout: None,
            accept_invalid_certs: false,
            identity: None,
        })
    }

    /// Returns a client builder based on the cluster information from the kubeconfig file.
    ///
    /// This allows to create your custom reqwest client for using with the cluster API.
    pub async fn new_from_kube_config(options: &ConfigOptions) -> Result<Self> {
        let configuration = Configuration::new_from_kube_config(&options).await?;
        let loader = ConfigLoader::new_from_options(&options).await?;

        let token = match &loader.user.token {
            Some(token) => Some(token.clone()),
            None => {
                if let Some(exec) = &loader.user.exec {
                    let creds = exec::auth_exec(exec)?;
                    let status = creds.status.ok_or_else(|| {
                        Error::KubeConfig("exec-plugin response did not contain a status".into())
                    })?;
                    status.token
                } else {
                    None
                }
            }
        };

        let timeout = std::time::Duration::new(295, 0);
        let mut accept_invalid_certs = false;
        let mut root_cert = None;
        let mut identity = None;

        if let Some(ca_bundle) = loader.ca_bundle()? {
            use std::convert::TryInto;
            for ca in ca_bundle {
                accept_invalid_certs = hacky_cert_lifetime_for_macos(&ca);
                root_cert = Some(ca.try_into()?);
            }
        }

        match loader.identity(" ") {
            Ok(id) => identity = Some(id),
            Err(e) => {
                debug!("failed to load client identity from kube config: {}", e);
                // last resort only if configs ask for it, and no client certs
                if let Some(true) = loader.cluster.insecure_skip_tls_verify {
                    accept_invalid_certs = true;
                }
            }
        }

        let mut headers = reqwest::header::HeaderMap::new();

        match (
            config::utils::data_or_file(&token, &loader.user.token_file),
            (&loader.user.username, &loader.user.password),
        ) {
            (Ok(token), _) => {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
                        .map_err(|e| Error::KubeConfig(format!("Invalid bearer token: {}", e)))?,
                );
            }
            (_, (Some(u), Some(p))) => {
                let encoded = base64::encode(&format!("{}:{}", u, p));
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Basic {}", encoded))
                        .map_err(|e| Error::KubeConfig(format!("Invalid bearer token: {}", e)))?,
                );
            }
            _ => {}
        }

        Ok(Self {
            cluster_url: configuration.cluster_url,
            root_cert,
            headers,
            timeout: Some(timeout),
            accept_invalid_certs,
            identity,
        })
    }
}

// temporary catalina hack for openssl only
#[cfg(all(target_os = "macos", feature = "native-tls"))]
fn hacky_cert_lifetime_for_macos(ca: &Der) -> bool {
    use openssl::x509::X509;
    let ca = X509::from_der(&ca.0).expect("valid der is a der");
    ca.not_before()
        .diff(ca.not_after())
        .map(|d| d.days.abs() > 824)
        .unwrap_or(false)
}

#[cfg(any(not(target_os = "macos"), not(feature = "native-tls")))]
fn hacky_cert_lifetime_for_macos(_: &Der) -> bool {
    false
}

// Expose raw config structs
pub use apis::{
    AuthInfo, AuthProviderConfig, Cluster, Config, Context, ExecConfig, NamedCluster, NamedContext,
    NamedExtension, Preferences,
};
