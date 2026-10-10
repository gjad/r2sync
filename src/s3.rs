use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use quick_xml::de::from_str;
use reqwest::{header, Body, Client, Method, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct RemoteObject {
    pub key: String,
    pub size: u64,
    pub etag: String,
}

struct SigningTarget<'a> {
    url: &'a str,
    uri: &'a str,
    query: &'a str,
}

#[derive(Clone)]
pub struct S3Client {
    config: Config,
    http: Client,
    host: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "ListBucketResult")]
struct ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<ListEntry>,
    #[serde(rename = "IsTruncated", default)]
    is_truncated: String,
    #[serde(rename = "NextContinuationToken")]
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "ETag", default)]
    etag: String,
}

impl S3Client {
    pub fn new(config: Config) -> Result<Self> {
        let endpoint = reqwest::Url::parse(&config.endpoint).context("invalid R2 endpoint URL")?;
        if endpoint.host_str().is_none()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || (endpoint.path() != "/" && !endpoint.path().is_empty())
        {
            anyhow::bail!("endpoint must be a scheme and host only, e.g. https://<account-id>.r2.cloudflarestorage.com");
        }
        let host = match endpoint.port() {
            Some(port) => format!("{}:{port}", endpoint.host_str().unwrap()),
            None => endpoint.host_str().unwrap().to_owned(),
        };
        let http = Client::builder()
            .user_agent(concat!("r2sync/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("create HTTP client")?;
        Ok(Self { config, http, host })
    }

    fn object_url(&self, key: &str) -> (String, String) {
        let path = format!("/{}/{}", encode_path(&self.config.bucket), encode_path(key));
        (
            format!("{}{}", self.config.endpoint.trim_end_matches('/'), path),
            path,
        )
    }

    fn bucket_url(&self, query: &[(String, String)]) -> (String, String, String) {
        let path = format!("/{}", encode_path(&self.config.bucket));
        let mut query = query
            .iter()
            .map(|(k, v)| (encode_query(k), encode_query(v)))
            .collect::<Vec<_>>();
        query.sort();
        let canonical_query = query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let url = if canonical_query.is_empty() {
            format!("{}{}", self.config.endpoint.trim_end_matches('/'), path)
        } else {
            format!(
                "{}{}?{}",
                self.config.endpoint.trim_end_matches('/'),
                path,
                canonical_query
            )
        };
        (url, path, canonical_query)
    }

    fn signed_request(
        &self,
        method: Method,
        target: SigningTarget<'_>,
        payload_hash: &str,
        body: Body,
        content_length: Option<u64>,
    ) -> Result<reqwest::RequestBuilder> {
        let now = current_utc_time()?;
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let canonical_headers = format!(
            "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
            self.host, payload_hash, amz_date
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method, target.uri, target.query, canonical_headers, signed_headers, payload_hash
        );
        let scope = format!("{}/{}/s3/aws4_request", date, self.config.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac_sha256(
            format!("AWS4{}", self.config.secret_access_key).as_bytes(),
            date.as_bytes(),
        )?;
        let k_region = hmac_sha256(&k_date, self.config.region.as_bytes())?;
        let k_service = hmac_sha256(&k_region, b"s3")?;
        let k_signing = hmac_sha256(&k_service, b"aws4_request")?;
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes())?);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.config.access_key_id, scope, signed_headers, signature
        );
        let mut request = self
            .http
            .request(method, target.url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header(header::AUTHORIZATION, authorization)
            .body(body);
        if let Some(length) = content_length {
            request = request.header(header::CONTENT_LENGTH, length);
        }
        Ok(request)
    }

    async fn send_empty(
        &self,
        method: Method,
        url: &str,
        canonical_uri: &str,
        canonical_query: &str,
    ) -> Result<Response> {
        for attempt in 0..4 {
            let request = self.signed_request(
                method.clone(),
                SigningTarget {
                    url,
                    uri: canonical_uri,
                    query: canonical_query,
                },
                &sha256_hex(b""),
                Body::from(Vec::<u8>::new()),
                None,
            )?;
            match request.send().await {
                Ok(response) if response.status().is_server_error() && attempt < 3 => continue,
                Ok(response) => return Ok(response),
                Err(error) if attempt < 3 => {
                    let _ = error;
                    continue;
                }
                Err(error) => return Err(error).context("send R2 request"),
            }
        }
        unreachable!()
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>> {
        let mut result = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), prefix.to_owned()),
            ];
            if let Some(value) = &token {
                query.push(("continuation-token".to_owned(), value.clone()));
            }
            let (url, uri, canonical_query) = self.bucket_url(&query);
            let response = self
                .send_empty(Method::GET, &url, &uri, &canonical_query)
                .await?;
            let text = response_text(response, "list objects").await?;
            let page: ListBucketResult =
                from_str(&text).context("parse R2 ListObjectsV2 response")?;
            result.extend(page.contents.into_iter().map(|entry| RemoteObject {
                key: entry.key,
                size: entry.size,
                etag: entry.etag,
            }));
            if page.is_truncated != "true" {
                break;
            }
            token = page.next_token;
            if token.is_none() {
                anyhow::bail!("R2 marked results truncated but omitted NextContinuationToken");
            }
        }
        Ok(result)
    }

    pub async fn put_file(&self, key: &str, path: &Path) -> Result<Option<String>> {
        let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .with_context(|| format!("read {}", path.display()))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let payload_hash = hex::encode(hasher.finalize());
        let length = std::fs::metadata(path)?.len();
        let (url, uri) = self.object_url(key);
        for attempt in 0..4 {
            let file = tokio::fs::File::open(path)
                .await
                .with_context(|| format!("open {}", path.display()))?;
            let stream = ReaderStream::new(file);
            let body = Body::wrap_stream(stream);
            let request = self.signed_request(
                Method::PUT,
                SigningTarget {
                    url: &url,
                    uri: &uri,
                    query: "",
                },
                &payload_hash,
                body,
                Some(length),
            )?;
            match request.send().await {
                Ok(response) if response.status().is_server_error() && attempt < 3 => continue,
                Ok(response) => {
                    let response = ensure_success_response(response, "upload object").await?;
                    return Ok(response
                        .headers()
                        .get(header::ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned));
                }
                Err(error) if attempt < 3 => {
                    let _ = error;
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("upload {}", path.display()))
                }
            }
        }
        unreachable!()
    }

    pub async fn sha256_of_object(&self, key: &str) -> Result<String> {
        let (url, uri) = self.object_url(key);
        let response = self.send_empty(Method::GET, &url, &uri, "").await?;
        let response = ensure_success_response(response, "read object for checksum").await?;
        let mut hasher = Sha256::new();
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            hasher.update(&chunk.context("read R2 object while calculating SHA-256")?);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    pub async fn download_file(&self, key: &str, destination: &Path) -> Result<()> {
        let (url, uri) = self.object_url(key);
        let response = self.send_empty(Method::GET, &url, &uri, "").await?;
        let response = ensure_success_response(response, "download object").await?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut output = tokio::fs::File::create(destination)
            .await
            .with_context(|| format!("create {}", destination.display()))?;
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            output
                .write_all(&chunk?)
                .await
                .with_context(|| format!("write {}", destination.display()))?;
        }
        output.flush().await?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let (url, uri) = self.object_url(key);
        let response = self.send_empty(Method::DELETE, &url, &uri, "").await?;
        ensure_success(response, "delete object").await
    }
}

async fn response_text(response: Response, operation: &str) -> Result<String> {
    let status = response.status();
    let text = response.text().await.context("read R2 response")?;
    if !status.is_success() {
        anyhow::bail!("{operation} failed ({status}): {}", concise(&text));
    }
    Ok(text)
}

async fn ensure_success(response: Response, operation: &str) -> Result<()> {
    ensure_success_response(response, operation).await?;
    Ok(())
}

async fn ensure_success_response(response: Response, operation: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    anyhow::bail!("{operation} failed ({status}): {}", concise(&text));
}

fn concise(text: &str) -> String {
    let text = text.trim();
    if text.len() > 500 {
        format!("{}…", &text[..500])
    } else {
        text.to_owned()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn current_utc_time() -> Result<DateTime<Utc>> {
    #[cfg(windows)]
    {
        // This API is available on Windows Server 2008. SystemTime::now and
        // Chrono's Utc::now can import GetSystemTimePreciseAsFileTime, which
        // is unavailable on that OS and prevents the executable from loading.
        #[repr(C)]
        struct FileTime {
            low_date_time: u32,
            high_date_time: u32,
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetSystemTimeAsFileTime(file_time: *mut FileTime);
        }

        const WINDOWS_TO_UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
        let mut file_time = FileTime {
            low_date_time: 0,
            high_date_time: 0,
        };
        // The API initializes the provided FILETIME structure and has no failure result.
        unsafe { GetSystemTimeAsFileTime(&mut file_time) };
        let windows_ticks =
            (u64::from(file_time.high_date_time) << 32) | u64::from(file_time.low_date_time);
        let unix_ticks = windows_ticks
            .checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS)
            .context("Windows system clock predates the Unix epoch")?;
        let seconds = i64::try_from(unix_ticks / 10_000_000)
            .context("system clock timestamp is out of range")?;
        let nanoseconds = ((unix_ticks % 10_000_000) * 100) as u32;
        DateTime::<Utc>::from_timestamp(seconds, nanoseconds)
            .context("system clock timestamp is out of range")
    }

    #[cfg(not(windows))]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock predates the Unix epoch")?;
        let seconds =
            i64::try_from(now.as_secs()).context("system clock timestamp is out of range")?;
        DateTime::<Utc>::from_timestamp(seconds, now.subsec_nanos())
            .context("system clock timestamp is out of range")
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("HMAC initialization failed: {e}"))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn encode_path(value: &str) -> String {
    encode(value, true)
}
fn encode_query(value: &str) -> String {
    encode(value, false)
}
fn encode(value: &str, preserve_slash: bool) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(*byte, b'-' | b'_' | b'.' | b'~')
            || (preserve_slash && *byte == b'/')
        {
            output.push(*byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{encode_path, encode_query, sha256_hex};

    #[test]
    fn uri_encoding_matches_rfc3986_and_preserves_key_slashes() {
        assert_eq!(encode_path("folder/a b+é.txt"), "folder/a%20b%2B%C3%A9.txt");
        assert_eq!(encode_query("a b/+"), "a%20b%2F%2B");
    }

    #[test]
    fn sha256_uses_hex_digest() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
