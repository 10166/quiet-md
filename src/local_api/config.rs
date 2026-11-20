//! Persistent local-only configuration for the built-in MCP HTTP server.
//! Stored in `<data_local_dir>/quiet-md/local-api.json` (0600 on Unix),
//! outside the user's library directory so tokens are never synced or committed.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 27123;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalApiConfig {
    pub enabled: bool,
    pub port: u16,
    pub token: String,
    pub read_only: bool,
    pub snapshot_before_edit: bool,
}

impl Default for LocalApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_PORT,
            token: String::new(),
            read_only: false,
            snapshot_before_edit: true,
        }
    }
}

impl LocalApiConfig {
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn ensure_token(&mut self) -> bool {
        if self.token.trim().is_empty() {
            self.token = generate_token();
            true
        } else {
            false
        }
    }

    pub fn client_config_json(&self) -> String {
        let token = if self.token.is_empty() { "<token>" } else { self.token.as_str() };
        self.client_config_json_with_token(token)
    }

    pub fn client_config_json_masked(&self) -> String {
        let token = if self.token.is_empty() { "<token>" } else { "••••••••••••••••" };
        self.client_config_json_with_token(token)
    }

    fn client_config_json_with_token(&self, token: &str) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "quiet-md": {
                    "url": self.endpoint(),
                    "headers": {
                        "Authorization": format!("Bearer {token}")
                    }
                }
            }
        }))
        .unwrap_or_default()
    }
}

pub fn generate_token() -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    format!("{:032x}{:032x}", a.as_u128(), b.as_u128())
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("QUIET_MD_LOCAL_API_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::data_local_dir()
        .unwrap_or_else(fdl_core::vfs::temp_dir)
        .join("quiet-md")
        .join("local-api.json")
}

pub fn load() -> LocalApiConfig {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> LocalApiConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return LocalApiConfig::default();
    };
    let mut cfg: LocalApiConfig = serde_json::from_str(&text).unwrap_or_default();
    if cfg.port < 1024 {
        cfg.port = DEFAULT_PORT;
    }
    cfg
}

pub fn save(cfg: &LocalApiConfig) -> Result<()> {
    save_to(&config_path(), cfg)
}

pub fn save_to(path: &Path, cfg: &LocalApiConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        let existed = parent.exists();
        if !existed {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    let tmp = path.with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
    let data = serde_json::to_vec_pretty(cfg)?;
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_generates_256bit_token() {
        let dir = std::env::temp_dir().join(format!("quiet-local-api-cfg-{}", uuid::Uuid::new_v4()));
        let path = dir.join("local-api.json");
        let mut cfg = LocalApiConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.read_only);
        assert!(cfg.snapshot_before_edit);
        assert!(cfg.ensure_token());
        assert_eq!(cfg.token.len(), 64);
        cfg.enabled = true;
        cfg.port = 28123;
        save_to(&path, &cfg).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded, cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
