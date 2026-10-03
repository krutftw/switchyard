use crate::{AppError, Result};
use std::path::{Path, PathBuf};
use switchyard_agent::{AppEngine, EngineOptions};
use switchyard_core::config::resolve_secret;
use switchyard_gateway::{ClientIdentity, Gateway, GatewayOptions, PresentedCredentials};

/// Shared desktop/CLI options. The default listener is an allocated loopback port.
#[derive(Clone, Debug)]
pub struct AppOptions {
    pub gateway_config: PathBuf,
    pub data_dir: PathBuf,
    pub client_key_name: Option<String>,
    pub port: u16,
}

impl AppOptions {
    pub fn new(gateway_config: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            gateway_config: gateway_config.into(),
            data_dir: data_dir.into(),
            client_key_name: None,
            port: 0,
        }
    }
}

/// OS-appropriate private app state, independent of a standalone gateway config.
pub fn default_paths() -> Result<(PathBuf, PathBuf)> {
    let variable = |name: &str| {
        std::env::var_os(name)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    #[cfg(windows)]
    let root = variable("LOCALAPPDATA")
        .or_else(|| variable("APPDATA"))
        .ok_or_else(|| {
            AppError::invalid(
                "Set --config and --data-dir; no local application-data directory is available.",
            )
        })?
        .join("Switchya");
    #[cfg(target_os = "macos")]
    let root = variable("HOME")
        .ok_or_else(|| AppError::invalid("Set --config and --data-dir; HOME is unavailable."))?
        .join("Library/Application Support/Switchya");
    #[cfg(not(any(windows, target_os = "macos")))]
    let root = variable("XDG_DATA_HOME")
        .filter(|path| path.is_absolute())
        .or_else(|| variable("HOME").map(|home| home.join(".local/share")))
        .ok_or_else(|| {
            AppError::invalid(
                "Set --config and --data-dir; no local application-data directory is available.",
            )
        })?
        .join("switchya");
    if !root.is_absolute() {
        return Err(AppError::invalid(
            "The application-data directory must be absolute; set --config and --data-dir.",
        ));
    }
    Ok((root.join("gateway.toml"), root.join("sessions")))
}

/// Create one managed private directory, or verify an existing one without
/// changing permissions on it or its descendants. The parent must exist.
pub(crate) fn private_directory(path: &Path) -> Result<()> {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    let created = match builder.create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => {
            return Err(AppError::local(
                "Could not create the private account directory.",
            ));
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
        };
        let access = if created {
            READ_CONTROL | WRITE_DAC | WRITE_OWNER
        } else {
            READ_CONTROL
        };
        let sharing = if created {
            0
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        };
        let directory = std::fs::OpenOptions::new().access_mode(access).share_mode(sharing)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).open(path)
            .map_err(|_| AppError::invalid("Could not open the private account directory without following a reparse point."))?;
        if created {
            crate::launch_acl::restrict_directory(&directory)?;
        }
        crate::launch_acl::verify_directory(&directory)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let _ = created;
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::DIRECTORY).bits() as i32,
            )
            .open(path)
            .map_err(|_| {
                AppError::invalid(
                    "Could not open the private account directory without following a symlink.",
                )
            })?;
        let metadata = directory
            .metadata()
            .map_err(|_| AppError::invalid("Could not inspect the account directory."))?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(AppError::invalid(
                "Account directories must belong to this user and allow access only to their owner.",
            ));
        }
    }
    Ok(())
}

fn make_private_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) else {
        return Ok(());
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .map_err(|_| AppError::local("Could not create the gateway configuration directory."))
}

fn client_identity(gateway: &Gateway, selected: Option<&str>) -> Result<ClientIdentity> {
    let config = gateway.config();
    let enabled: Vec<_> = config.auth.keys.iter().filter(|key| key.enabled).collect();
    let candidates: Vec<_> = match selected {
        Some(name) => enabled.into_iter().filter(|key| key.name == name).collect(),
        None if enabled.len() == 1 => enabled,
        None => enabled
            .into_iter()
            .filter(|key| key.name == "app")
            .collect(),
    };
    if candidates.len() != 1 {
        return Err(AppError::invalid(
            "Choose exactly one enabled gateway client key with --client-key-name; the app cannot use an admin identity.",
        ));
    }
    let key = resolve_secret(&candidates[0].key).map_err(|_| {
        AppError::invalid(
            "The selected client key could not be resolved; check its environment reference.",
        )
    })?;
    if key.is_empty() {
        return Err(AppError::invalid(
            "The selected gateway client key is empty.",
        ));
    }
    let identity = gateway
        .authenticate(&PresentedCredentials {
            x_api_key: Some(key),
            ..Default::default()
        })
        .map_err(|_| {
            AppError::invalid("The selected gateway client key could not be authenticated.")
        })?;
    if identity.anonymous || identity.internal || identity.key_id.is_none() {
        return Err(AppError::invalid(
            "The app requires an authenticated client key with its own model permissions.",
        ));
    }
    Ok(identity)
}

pub(crate) struct AppRuntime {
    pub(crate) gateway: Gateway,
    pub(crate) engine: AppEngine,
    pub(crate) config_path: PathBuf,
}

impl AppRuntime {
    pub(crate) async fn start(options: &AppOptions) -> Result<Self> {
        crate::initialize_tls();
        let path = std::path::absolute(&options.gateway_config)
            .map_err(|_| AppError::invalid("Could not resolve the gateway configuration path."))?;
        match path.try_exists() {
            Ok(false) => {
                make_private_parent(&path)?;
                // create_new underneath: never overwrite an existing gateway file,
                // including a file created by another process during this check.
                switchyard::starter::create(&path, false)
                    .map_err(|_| AppError::local("Could not create a private gateway configuration; an existing file is never overwritten."))?;
            }
            Ok(true) => {}
            Err(_) => {
                return Err(AppError::local(
                    "Could not inspect the gateway configuration file.",
                ));
            }
        }
        let gateway = Gateway::start(GatewayOptions::new(&path)).await
            .map_err(|_| AppError::invalid("The gateway configuration could not start. Use `switchyard check --config <path>` to review it."))?;
        let opened =
            client_identity(&gateway, options.client_key_name.as_deref()).and_then(|identity| {
                AppEngine::open(
                    gateway.clone(),
                    identity,
                    EngineOptions::new(&options.data_dir),
                )
                .map_err(AppError::from)
            });
        match opened {
            Ok(engine) => Ok(Self {
                gateway,
                engine,
                config_path: path,
            }),
            Err(error) => {
                gateway.shutdown().await;
                Err(error)
            }
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let result = self.engine.shutdown().await.map_err(AppError::from);
        self.gateway.shutdown().await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_directory_is_private_and_existing_permissions_are_only_verified() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("managed");
        private_directory(&path).unwrap();
        private_directory(&path).unwrap();
        assert!(path.is_dir());
        let ordinary = temp.path().join("ordinary");
        std::fs::create_dir(&ordinary).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ordinary, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(private_directory(&ordinary).is_err());
            assert_eq!(
                std::fs::metadata(&ordinary).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        #[cfg(windows)]
        assert!(private_directory(&ordinary).is_err());
    }

    #[tokio::test]
    async fn app_uses_selected_client_model_permissions_and_preserves_existing_config() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("gateway.toml");
        let config = r#"
[[auth.keys]]
name = "app"
key = "sy-app-test-client-key-0123456789"
models = ["mock-echo"]
[[auth.keys]]
name = "restricted"
key = "sy-other-test-client-key-0123456789"
models = ["mock-lorem"]
[[providers]]
name = "mock"
kind = "mock"
"#;
        std::fs::write(&config_path, config).unwrap();
        let options = AppOptions::new(&config_path, temp.path().join("state-app"));
        let runtime = AppRuntime::start(&options).await.unwrap();
        let models = runtime.engine.models().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "mock-echo");
        runtime.shutdown().await.unwrap();
        drop(runtime);

        let mut options = AppOptions::new(&config_path, temp.path().join("state-restricted"));
        options.client_key_name = Some("restricted".into());
        let runtime = AppRuntime::start(&options).await.unwrap();
        let models = runtime.engine.models().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "mock-lorem");
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), config);
        runtime.shutdown().await.unwrap();
    }
}
