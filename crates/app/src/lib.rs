//! Switchya's local, authenticated workspace host shared by CLI and desktop.
#![deny(unsafe_code)]

mod accounts;
mod api;
mod assets;
pub mod cli;
mod error;
mod guard;
mod launch;
#[cfg(windows)]
#[allow(unsafe_code)]
mod launch_acl;
mod providers;
mod startup;

pub use error::{AppError, Result};
pub use launch::LaunchInfo;
pub use startup::{AppOptions, default_paths};

use std::path::Path;
use std::sync::Arc;
use switchyard_agent_adapters::AdapterManager;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Reqwest uses rustls-no-provider in this workspace. Initialize its process
/// default before creating any app client, while preserving an existing choice.
pub(crate) fn initialize_tls() {
    static INITIALIZE: std::sync::Once = std::sync::Once::new();
    INITIALIZE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Owns the server lifetime. Desktop callers should await shutdown before exiting.
pub struct RunningHost {
    launch: LaunchInfo,
    cancel: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}

impl RunningHost {
    pub fn launch_url(&self) -> &str {
        &self.launch.url
    }
    pub fn base_url(&self) -> &str {
        &self.launch.base_url
    }
    pub fn write_launch_info(&self, path: impl AsRef<Path>) -> Result<()> {
        self.launch.write_new(path)
    }
    pub async fn shutdown(mut self) -> Result<()> {
        self.cancel.cancel();
        match self.task.take() {
            Some(task) => task
                .await
                .map_err(|_| AppError::local("The local app host stopped unexpectedly."))?,
            None => Ok(()),
        }
    }
}

impl Drop for RunningHost {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Starts only a literal IPv4 loopback listener; requires an active Tokio runtime.
pub async fn start_host(options: AppOptions) -> Result<RunningHost> {
    initialize_tls();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, options.port))
        .await
        .map_err(|_| AppError::local("Could not bind the local app listener."))?;
    let port = listener
        .local_addr()
        .map_err(|_| AppError::local("Could not read the local app address."))?
        .port();
    let runtime = startup::AppRuntime::start(&options).await?;
    let accounts = match accounts::AccountManager::open(&options.data_dir) {
        Ok(accounts) => accounts,
        Err(error) => {
            let _ = runtime.shutdown().await;
            return Err(error);
        }
    };
    let launch = LaunchInfo::new(port);
    let adapters = match AdapterManager::open(&options.data_dir) {
        Ok(adapters) => adapters,
        Err(error) => {
            accounts.shutdown().await;
            let _ = runtime.shutdown().await;
            return Err(AppError::from(error));
        }
    };
    let cancel = CancellationToken::new();
    let state = Arc::new(api::AppState {
        engine: runtime.engine.clone(),
        gateway: runtime.gateway.clone(),
        config_path: runtime.config_path.clone(),
        adapters: adapters.clone(),
        accounts: accounts.clone(),
    });
    let router = api::router(state, port, launch.token.clone(), cancel.clone());
    let stopped = cancel.clone();
    let task = tokio::spawn(async move {
        use std::future::IntoFuture;
        let server = axum::serve(listener, router)
            .with_graceful_shutdown(stopped.clone().cancelled_owned())
            .into_future();
        tokio::pin!(server);
        let served = tokio::select! {
            result = &mut server => Some(result),
            () = stopped.cancelled() => None,
        };
        stopped.cancel();
        // Cancel owned runs immediately, alongside a bounded HTTP drain. A slow
        // client must never keep a command running after the app closes.
        let drain = async {
            match served {
                Some(result) => {
                    result.map_err(|_| AppError::local("The local app listener failed."))
                }
                None => match tokio::time::timeout(std::time::Duration::from_secs(3), &mut server)
                    .await
                {
                    Ok(result) => {
                        result.map_err(|_| AppError::local("The local app listener failed."))
                    }
                    Err(_) => Err(AppError::local(
                        "The local app listener did not finish draining before shutdown.",
                    )),
                },
            }
        };
        let (_, _, engine, served) = tokio::join!(
            accounts.shutdown(),
            adapters.shutdown(),
            runtime.shutdown(),
            drain
        );
        engine?;
        served
    });
    Ok(RunningHost {
        launch,
        cancel,
        task: Some(task),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_serves_bundled_ui_protects_api_and_shuts_down_listener() {
        let temp = tempfile::tempdir().unwrap();
        let host = start_host(AppOptions::new(
            temp.path().join("gateway.toml"),
            temp.path().join("sessions"),
        ))
        .await
        .unwrap();
        let base = host.base_url().to_owned();
        assert!(base.starts_with("http://127.0.0.1:"));
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let unauthorized = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(unauthorized.headers()["cache-control"], "no-store");
        let index = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(index.status(), reqwest::StatusCode::OK);
        assert!(index.headers().contains_key("content-security-policy"));
        assert!(index.text().await.unwrap().contains("Switchya workspace"));
        let status: serde_json::Value = client
            .get(format!("{base}/api/status"))
            .bearer_auth(&host.launch.token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["status"]["shell_is_sandboxed"], false);
        let launch_file = temp.path().join("launch.json");
        host.write_launch_info(&launch_file).unwrap();
        assert_eq!(
            LaunchInfo::read(&launch_file).unwrap().url,
            host.launch_url()
        );
        let port = url::Url::parse(&base).unwrap().port().unwrap();
        host.shutdown().await.unwrap();
        assert!(
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_partial_http_body_does_not_delay_host_shutdown() {
        use tokio::io::AsyncWriteExt;
        let temp = tempfile::tempdir().unwrap();
        let host = start_host(AppOptions::new(
            temp.path().join("gateway.toml"),
            temp.path().join("sessions"),
        ))
        .await
        .unwrap();
        let port = url::Url::parse(host.base_url()).unwrap().port().unwrap();
        let mut connection = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let request = format!(
            "POST /api/projects HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 10000\r\n\r\n{{\"path\":",
            host.launch.token
        );
        connection.write_all(request.as_bytes()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), host.shutdown())
            .await
            .expect("shutdown waited for an unfinished HTTP body")
            .unwrap();
    }
}
