use anyhow::Result;
use clap::Parser;
use rand::RngCore;
use std::path::PathBuf;

#[derive(Clone, Debug, Parser)]
#[command(name = "quiet-sync-server", about = "Self-hostable real-time sync server for Quiet.md")]
pub struct ServerConfig {
    #[arg(long, env = "QUIET_SYNC_BIND", default_value = "127.0.0.1:8787")]
    pub bind: String,

    #[arg(long, env = "QUIET_SYNC_DATA_DIR", default_value = "./sync-data")]
    pub data_dir: PathBuf,

    #[arg(long, env = "QUIET_SYNC_JWT_SECRET")]
    pub jwt_secret: Option<String>,

    #[arg(long, env = "QUIET_SYNC_ALLOW_REGISTER", default_value_t = true)]
    pub allow_register: bool,

    #[arg(long, env = "QUIET_SYNC_INVITE_CODE")]
    pub invite_code: Option<String>,

    #[arg(long, env = "QUIET_SYNC_MAX_BLOB_MB", default_value_t = 100)]
    pub max_blob_mb: u64,

    #[arg(long, env = "QUIET_SYNC_MAX_USER_MB")]
    pub max_user_mb: Option<u64>,

    #[arg(long, env = "QUIET_SYNC_TOMBSTONE_DAYS", default_value_t = 90)]
    pub tombstone_days: u64,

    #[arg(long, env = "QUIET_SYNC_NODE_NAME")]
    pub node_name: Option<String>,

    #[arg(long, env = "QUIET_SYNC_PUBLIC_URL")]
    pub public_url: Option<String>,

    #[arg(long, env = "QUIET_SYNC_ADMIN_PASSWORD")]
    pub admin_password: Option<String>,

    #[arg(long, env = "QUIET_SYNC_BINLOG_RETENTION_DAYS", default_value_t = 7)]
    pub binlog_retention_days: u64,

    #[arg(long, env = "QUIET_SYNC_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    #[arg(long, env = "QUIET_SYNC_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    #[arg(long, env = "QUIET_SYNC_S3_REGION", default_value = "auto")]
    pub s3_region: String,

    #[arg(long, env = "QUIET_SYNC_S3_ACCESS_KEY")]
    pub s3_access_key: Option<String>,

    #[arg(long, env = "QUIET_SYNC_S3_SECRET_KEY")]
    pub s3_secret_key: Option<String>,

    #[arg(long, env = "QUIET_SYNC_S3_PREFIX", default_value = "quiet-sync/")]
    pub s3_prefix: String,
}

impl ServerConfig {
    pub fn for_test(data_dir: PathBuf) -> Self {
        Self {
            bind: "127.0.0.1:0".into(),
            data_dir,
            jwt_secret: Some("test-jwt-secret-key-for-quiet-sync".into()),
            allow_register: true,
            invite_code: None,
            max_blob_mb: 32,
            max_user_mb: None,
            tombstone_days: 30,
            node_name: None,
            public_url: None,
            admin_password: None,
            binlog_retention_days: 7,
            s3_endpoint: None,
            s3_bucket: None,
            s3_region: "auto".into(),
            s3_access_key: None,
            s3_secret_key: None,
            s3_prefix: "quiet-sync/".into(),
        }
    }

    pub fn resolve_jwt_secret(&self) -> Result<Vec<u8>> {
        if let Some(s) = &self.jwt_secret {
            anyhow::ensure!(!s.is_empty(), "QUIET_SYNC_JWT_SECRET cannot be empty");
            return Ok(s.as_bytes().to_vec());
        }
        std::fs::create_dir_all(&self.data_dir)?;
        let file = self.data_dir.join("jwt.secret");
        if file.exists() {
            let s = std::fs::read_to_string(&file)?;
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.as_bytes().to_vec());
            }
        }
        let mut raw = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut raw);
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(&file, &hex)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(hex.into_bytes())
    }
}
