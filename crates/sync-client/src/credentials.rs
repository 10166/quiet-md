use anyhow::Result;
use quiet_sync_proto::crypto::E2eeSetup;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !dir.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

pub(crate) fn harden_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub server_url: String,
    pub email: String,
    pub user_id: String,
    pub display_name: String,
    pub device_id: String,
    pub device_name: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmk_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee_setup: Option<E2eeSetup>,
}

pub trait SecretStore: Send + Sync + 'static {
    fn load(&self) -> Result<Option<StoredCredentials>>;
    fn save(&self, creds: &StoredCredentials) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

#[derive(Clone)]
pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    pub fn new(sync_dir: &Path) -> Self {
        Self { path: sync_dir.join("credentials.json") }
    }
}

impl SecretStore for FileSecretStore {
    fn load(&self) -> Result<Option<StoredCredentials>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, creds: &StoredCredentials) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            ensure_private_dir(dir)?;
        }
        let tmp = self.path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(creds)?;
        let write_res: Result<()> = (|| {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            harden_private_file(&tmp)?;
            Ok(())
        })();
        if let Err(e) = write_res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, &self.path)?;
        harden_private_file(&self.path)?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
