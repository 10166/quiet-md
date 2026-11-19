# quiet-sync-server

Self-hostable real-time sync server for **Quiet.md** (Axum + WebSocket + SQLite WAL + local or S3-compatible blob storage), with a built-in **Admin Portal** (`/portal`) and **Primary ↔ Backup replication**.

## Quick Start (Binary)

```sh
cargo run --release -p quiet_sync_server -- \
  --bind 127.0.0.1:8787 \
  --data-dir ./sync-data
```

Open `http://127.0.0.1:8787/portal` in a browser to initialize the local admin password and manage users, storage, and primary-backup replication.

## Quick Start (Docker)

```sh
cd crates/sync-server
docker compose up -d --build
```

Put a reverse proxy (Caddy or Nginx) in front of `:8787` to terminate HTTPS / WSS TLS in production.

### Example Caddyfile

```caddyfile
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

## Primary & Backup Replication

Backup servers (including nodes behind NAT / on an intranet) connect outbound to the Primary server over HTTPS / WSS:

1. Initialize the admin password on both Primary and Backup Portals (`/portal`).
2. On the Backup server's Portal, go to **主从备份 (Replication)** and enter the Primary server's URL and Primary admin password.
3. The Primary verifies the password once and issues a dedicated revocable replica token (the Backup never stores the Primary's admin password).
4. The Backup automatically performs a lock-free WAL snapshot bootstrap, streams real-time row-level binlog events over WebSocket, and downloads missing content-addressed blobs with SHA-256 verification.
5. Both Portals display each other's live status and provide one-click jump links to the peer's Portal. Primary admins can evict replicas at any time; Backup admins can promote a Backup to a writable Primary for disaster recovery.

## Configuration

All flags can also be set via environment variables:

| Flag | Env | Default | Description |
|---|---|---|---|
| `--bind` | `QUIET_SYNC_BIND` | `127.0.0.1:8787` | Listen address |
| `--data-dir` | `QUIET_SYNC_DATA_DIR` | `./sync-data` | SQLite DB, `jwt.secret`, local blobs |
| `--jwt-secret` | `QUIET_SYNC_JWT_SECRET` | auto-generated | HS256 JWT signing secret |
| `--node-name` | `QUIET_SYNC_NODE_NAME` | hostname | Display name shown in Admin Portal and peer nodes |
| `--public-url` | `QUIET_SYNC_PUBLIC_URL` | none | Public/LAN base URL of this node's Portal for peer quick-jump |
| `--admin-password` | `QUIET_SYNC_ADMIN_PASSWORD` | none | Optional initial/override Admin Portal password (if omitted, set via `/portal` on first visit) |
| `--binlog-retention-days` | `QUIET_SYNC_BINLOG_RETENTION_DAYS` | `7` | Retention window for primary replication binlog entries |
| `--allow-register` | `QUIET_SYNC_ALLOW_REGISTER` | `true` | Allow new user signups (can also be toggled live in Portal) |
| `--invite-code` | `QUIET_SYNC_INVITE_CODE` | none | Optional invite code required on signup |
| `--max-blob-mb` | `QUIET_SYNC_MAX_BLOB_MB` | `100` | Max upload size per attachment blob |
| `--tombstone-days` | `QUIET_SYNC_TOMBSTONE_DAYS` | `90` | Retention window for deleted entity op-logs |
| `--s3-endpoint` | `QUIET_SYNC_S3_ENDPOINT` | none | S3-compatible endpoint (R2 / MinIO / S3) |
| `--s3-bucket` | `QUIET_SYNC_S3_BUCKET` | none | S3 bucket name |
| `--s3-region` | `QUIET_SYNC_S3_REGION` | `auto` | S3 region |
| `--s3-access-key` | `QUIET_SYNC_S3_ACCESS_KEY` | none | S3 access key |
| `--s3-secret-key` | `QUIET_SYNC_S3_SECRET_KEY` | none | S3 secret key |
| `--s3-prefix` | `QUIET_SYNC_S3_PREFIX` | `quiet-sync/` | Key prefix inside bucket |
