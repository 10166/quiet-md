use anyhow::{Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait BlobStore: Send + Sync + 'static {
    fn exists<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<bool>>;
    fn put<'a>(&'a self, hash: &'a str, data: Vec<u8>) -> BoxFuture<'a, Result<()>>;
    fn get<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>>;
    fn delete<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<()>>;
    fn list_hashes<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>>;
}

pub type DynBlobStore = Arc<dyn BlobStore>;

pub fn validate_hash(hash: &str) -> Result<()> {
    anyhow::ensure!(
        hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "invalid sha256 blob hash"
    );
    Ok(())
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[derive(Clone)]
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, hash: &str) -> Result<PathBuf> {
        validate_hash(hash)?;
        Ok(self.root.join(&hash[0..2]).join(&hash[2..4]).join(hash))
    }
}

impl BlobStore for FsBlobStore {
    fn exists<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let p = self.path_for(hash)?;
            Ok(tokio::fs::try_exists(p).await.unwrap_or(false))
        })
    }

    fn put<'a>(&'a self, hash: &'a str, data: Vec<u8>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p = self.path_for(hash)?;
            if tokio::fs::try_exists(&p).await.unwrap_or(false) {
                return Ok(());
            }
            if let Some(dir) = p.parent() {
                tokio::fs::create_dir_all(dir).await?;
            }
            let tmp = p.with_extension(format!("tmp.{}", Uuid::new_v4()));
            tokio::fs::write(&tmp, &data).await?;
            tokio::fs::rename(&tmp, &p).await?;
            Ok(())
        })
    }

    fn get<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let p = self.path_for(hash)?;
            match tokio::fs::read(&p).await {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn delete<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p = self.path_for(hash)?;
            match tokio::fs::remove_file(&p).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn list_hashes<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        let root = self.root.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                walk_hashes(&root, &mut out);
                Ok(out)
            })
            .await
            .map_err(anyhow::Error::from)?
        })
    }
}

fn walk_hashes(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_hashes(&p, out);
        } else if let Some(name) = p.file_name().and_then(|s| s.to_str())
            && validate_hash(name).is_ok()
        {
            out.push(name.to_string());
        }
    }
}

#[derive(Clone)]
pub struct S3BlobStore {
    client: reqwest::Client,
    endpoint: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
    prefix: String,
}

impl S3BlobStore {
    pub fn new(endpoint: String, bucket: String, region: String, access_key: String, secret_key: String, prefix: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket,
            region,
            access_key,
            secret_key,
            prefix: prefix.trim_matches('/').to_string(),
        }
    }

    fn object_path(&self, hash: &str) -> String {
        if self.prefix.is_empty() {
            format!("/{}/{hash}", self.bucket)
        } else {
            format!("/{}/{}/{hash}", self.bucket, self.prefix)
        }
    }

    fn signed_headers(&self, method: &str, uri_path: &str, canonical_query: &str, payload_hash: &str) -> Result<reqwest::header::HeaderMap> {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let host = self
            .endpoint
            .split("://")
            .nth(1)
            .unwrap_or(&self.endpoint)
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();

        let auth = sigv4_authorization(
            method,
            uri_path,
            canonical_query,
            &host,
            &self.region,
            &self.access_key,
            &self.secret_key,
            payload_hash,
            &amz_date,
            &date_stamp,
        )?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("host", host.parse().context("invalid host header")?);
        headers.insert("x-amz-date", amz_date.parse()?);
        headers.insert("x-amz-content-sha256", payload_hash.parse()?);
        headers.insert(reqwest::header::AUTHORIZATION, auth.parse()?);
        Ok(headers)
    }
}

impl BlobStore for S3BlobStore {
    fn exists<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            validate_hash(hash)?;
            let path = self.object_path(hash);
            let url = format!("{}{path}", self.endpoint);
            let empty_hash = sha256_hex(&[]);
            let headers = self.signed_headers("HEAD", &path, "", &empty_hash)?;
            let resp = self.client.head(&url).headers(headers).send().await?;
            if resp.status().as_u16() == 404 {
                return Ok(false);
            }
            anyhow::ensure!(resp.status().is_success(), "S3 HEAD failed: {}", resp.status());
            Ok(true)
        })
    }

    fn put<'a>(&'a self, hash: &'a str, data: Vec<u8>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            validate_hash(hash)?;
            let path = self.object_path(hash);
            let url = format!("{}{path}", self.endpoint);
            let payload_hash = sha256_hex(&data);
            let headers = self.signed_headers("PUT", &path, "", &payload_hash)?;
            let resp = self.client.put(&url).headers(headers).body(data).send().await?;
            anyhow::ensure!(resp.status().is_success(), "S3 PUT failed: {}", resp.status());
            Ok(())
        })
    }

    fn get<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            validate_hash(hash)?;
            let path = self.object_path(hash);
            let url = format!("{}{path}", self.endpoint);
            let empty_hash = sha256_hex(&[]);
            let headers = self.signed_headers("GET", &path, "", &empty_hash)?;
            let resp = self.client.get(&url).headers(headers).send().await?;
            if resp.status().as_u16() == 404 {
                return Ok(None);
            }
            anyhow::ensure!(resp.status().is_success(), "S3 GET failed: {}", resp.status());
            Ok(Some(resp.bytes().await?.to_vec()))
        })
    }

    fn delete<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            validate_hash(hash)?;
            let path = self.object_path(hash);
            let url = format!("{}{path}", self.endpoint);
            let empty_hash = sha256_hex(&[]);
            let headers = self.signed_headers("DELETE", &path, "", &empty_hash)?;
            let resp = self.client.delete(&url).headers(headers).send().await?;
            anyhow::ensure!(resp.status().is_success() || resp.status().as_u16() == 404, "S3 DELETE failed: {}", resp.status());
            Ok(())
        })
    }

    fn list_hashes<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let mut out = Vec::new();
            let mut token: Option<String> = None;
            let empty_hash = sha256_hex(&[]);
            let bucket_uri = format!("/{}", self.bucket);
            let list_prefix = if self.prefix.is_empty() { String::new() } else { format!("{}/", self.prefix) };
            for _ in 0..10_000 {
                let mut params: Vec<(&str, String)> = vec![("list-type", "2".into())];
                if !list_prefix.is_empty() {
                    params.push(("prefix", list_prefix.clone()));
                }
                if let Some(t) = &token {
                    params.push(("continuation-token", t.clone()));
                }
                params.sort_by(|a, b| a.0.cmp(b.0));
                let query = params
                    .into_iter()
                    .map(|(k, v)| format!("{}={}", sigv4_encode(k), sigv4_encode(&v)))
                    .collect::<Vec<_>>()
                    .join("&");
                let headers = self.signed_headers("GET", &bucket_uri, &query, &empty_hash)?;
                let url = format!("{}{bucket_uri}?{query}", self.endpoint);
                let resp = self.client.get(&url).headers(headers).send().await?;
                anyhow::ensure!(resp.status().is_success(), "S3 ListObjectsV2 failed: {}", resp.status());
                let body = resp.text().await?;
                let page = parse_s3_list_objects_v2(&body, &list_prefix);
                out.extend(page.hashes);
                if !page.truncated {
                    break;
                }
                token = page.next_token;
                if token.is_none() {
                    break;
                }
            }
            Ok(out)
        })
    }
}

struct S3ListPage {
    hashes: Vec<String>,
    truncated: bool,
    next_token: Option<String>,
}

fn parse_s3_list_objects_v2(xml: &str, strip_prefix: &str) -> S3ListPage {
    fn extract_tags(xml: &str, tag: &str) -> Vec<String> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let mut out = Vec::new();
        let mut rest = xml;
        while let Some(s) = rest.find(&open) {
            let after = &rest[s + open.len()..];
            let Some(e) = after.find(&close) else { break };
            out.push(unescape_xml(&after[..e]));
            rest = &after[e + close.len()..];
        }
        out
    }
    fn unescape_xml(s: &str) -> String {
        s.replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
    }
    let mut hashes = Vec::new();
    for key in extract_tags(xml, "Key") {
        let candidate = if strip_prefix.is_empty() {
            key.as_str()
        } else {
            key.strip_prefix(strip_prefix).unwrap_or(key.as_str())
        };
        if validate_hash(candidate).is_ok() {
            hashes.push(candidate.to_string());
        }
    }
    let truncated = extract_tags(xml, "IsTruncated").first().is_some_and(|s| s.eq_ignore_ascii_case("true"));
    let next_token = extract_tags(xml, "NextContinuationToken").into_iter().find(|s| !s.is_empty());
    S3ListPage { hashes, truncated, next_token }
}

fn sigv4_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(&mut out, "%{b:02X}");
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub fn sigv4_authorization(
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    host: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    payload_hash: &str,
    amz_date: &str,
    date_stamp: &str,
) -> Result<String> {
    let canonical_headers = format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!("{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date_stamp.as_bytes())?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, b"s3")?;
    let k_signing = hmac_sha256(&k_service, b"aws4_request")?;
    let sig_bytes = hmac_sha256(&k_signing, string_to_sign.as_bytes())?;
    let sig_hex: String = sig_bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={sig_hex}"
    ))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).context("hmac init")?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_signing_is_deterministic_and_well_formed() {
        let auth = sigv4_authorization(
            "PUT",
            "/my-bucket/quiet-sync/deadbeef",
            "",
            "s3.us-east-1.amazonaws.com",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "20261119T080000Z",
            "20261119",
        )
        .unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20261119/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="));
        assert_eq!(auth.split("Signature=").nth(1).unwrap().len(), 64);

        let q_auth = sigv4_authorization(
            "GET",
            "/my-bucket",
            "list-type=2&prefix=quiet-sync%2F",
            "s3.us-east-1.amazonaws.com",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "20261119T080000Z",
            "20261119",
        )
        .unwrap();
        assert_ne!(auth, q_auth);

        let h1 = "a".repeat(64);
        let xml = format!("<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>tok%2B1&amp;2</NextContinuationToken><Contents><Key>quiet-sync/{h1}</Key></Contents><Contents><Key>quiet-sync/ignore.txt</Key></Contents></ListBucketResult>");
        let parsed = parse_s3_list_objects_v2(&xml, "quiet-sync/");
        assert!(parsed.truncated);
        assert_eq!(parsed.next_token.as_deref(), Some("tok%2B1&2"));
        assert_eq!(parsed.hashes, vec![h1]);
    }
}
