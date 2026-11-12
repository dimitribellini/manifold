//! Live protocol drivers for Amazon S3 (SigV4 REST XML), WebDAV (`PROPFIND`/`GET`/`PUT`/`MKCOL`/`DELETE`/`MOVE`/`COPY`),
//! FTP/FTPS, and SMB (`GAP-01`).
//!
//! All HTTP/FTP credentials and authorization headers are passed to `curl --config -` via piped `stdin`
//! (or a `0600` temp authfile for `smbclient`), ensuring secrets never appear in process `argv` (`SEC-06`/`SEC-07`).

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use manifold_config::{ConnectionProfile, ProtocolType};
use manifold_vfs::{
    create_secure_temp_file, infer_file_kind, system_time_to_utc_civil, ColorLabel, FileMetadata,
    VfsError,
};
use sha2::{Digest, Sha256};

use crate::parse_ls_aln_output;

/// Computes `HMAC-SHA256(key, message)` in pure Rust using `sha2::Sha256`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut k_block = [0u8; 64];
    if key.len() > 64 {
        let hashed = Sha256::digest(key);
        k_block[..32].copy_from_slice(&hashed);
    } else {
        k_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k_block[i];
        opad[i] ^= k_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().into()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex_encode(&digest)
}

fn uri_encode_s3(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for b in input.bytes() {
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if is_unreserved || (!encode_slash && b == b'/') {
            out.push(b as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

fn escape_curl_config_val(val: &str) -> String {
    let mut out = String::with_capacity(val.len() + 2);
    out.push('"');
    for ch in val.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' | '\n' | '\0' => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn run_curl_with_stdin_config(
    config_lines: &[String],
    extra_args: &[&str],
) -> Result<Vec<u8>, VfsError> {
    let mut child = Command::new("curl")
        .args(["-sS", "--fail-with-body", "--connect-timeout", "10"])
        .args(extra_args)
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| VfsError::NetworkError(format!("Failed to execute curl: {e}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        let payload = config_lines.join("\n") + "\n";
        let _ = stdin.write_all(payload.as_bytes());
    }
    let out = child
        .wait_output_or_wait()
        .map_err(|e| VfsError::NetworkError(format!("curl execution failed: {e}")))?;
    if !out.status.success() {
        let stderr_txt = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout_txt = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr_txt.is_empty() {
            stderr_txt
        } else if !stdout_txt.is_empty() {
            stdout_txt
        } else {
            format!("HTTP/transfer failed with status {}", out.status)
        };
        return Err(VfsError::NetworkError(detail));
    }
    Ok(out.stdout)
}

trait ChildWaitHelper {
    fn wait_output_or_wait(self) -> std::io::Result<std::process::Output>;
}

impl ChildWaitHelper for std::process::Child {
    fn wait_output_or_wait(self) -> std::io::Result<std::process::Output> {
        self.wait_with_output()
    }
}

// ============================================================================
// 1. Amazon S3 & S3-Compatible (AWS Signature Version 4)
// ============================================================================

struct S3EndpointSpec {
    host_header: String,
    base_url: String,
    bucket_prefix: String,
    region: String,
}

fn resolve_s3_endpoint(profile: &ConnectionProfile) -> Result<S3EndpointSpec, VfsError> {
    let bucket = profile
        .bucket
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            profile
                .base_path
                .trim_matches('/')
                .split('/')
                .next()
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| VfsError::InvalidPath("Missing S3 bucket name".to_string()))?;
    let region = profile
        .region
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("us-east-1")
        .to_string();

    let raw_ep = profile
        .endpoint
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| profile.host.as_deref().filter(|s| !s.trim().is_empty()))
        .unwrap_or("");

    if raw_ep.is_empty() || raw_ep.contains("amazonaws.com") {
        let host = format!("{bucket}.s3.{region}.amazonaws.com");
        Ok(S3EndpointSpec {
            base_url: format!("https://{host}"),
            host_header: host,
            bucket_prefix: String::new(),
            region,
        })
    } else {
        let (scheme, rest) = if let Some(r) = raw_ep.strip_prefix("https://") {
            ("https", r)
        } else if let Some(r) = raw_ep.strip_prefix("http://") {
            ("http", r)
        } else {
            ("https", raw_ep)
        };
        let host = rest.trim_end_matches('/');
        if host.contains('/') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            let msg = format!("Invalid S3 endpoint host: {raw_ep}");
            return Err(VfsError::InvalidPath(msg));
        }
        Ok(S3EndpointSpec {
            base_url: format!("{scheme}://{host}"),
            host_header: host.to_string(),
            bucket_prefix: format!("/{bucket}"),
            region,
        })
    }
}

/// Parameters for computing an AWS Signature Version 4 header block.
pub struct S3SigV4Params<'a> {
    pub method: &'a str,
    pub host_header: &'a str,
    pub canonical_uri: &'a str,
    pub canonical_query: &'a str,
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub payload_sha256_hex: &'a str,
    pub amz_date: &'a str,
}

/// Builds AWS SigV4 headers (`Authorization`, `x-amz-date`, `x-amz-content-sha256`) for an S3 request.
pub fn build_s3_sigv4_headers(p: &S3SigV4Params<'_>) -> Vec<String> {
    let method = p.method;
    let host_header = p.host_header;
    let canonical_uri = p.canonical_uri;
    let canonical_query = p.canonical_query;
    let region = p.region;
    let access_key = p.access_key;
    let secret_key = p.secret_key;
    let payload_sha256_hex = p.payload_sha256_hex;
    let amz_date = p.amz_date;
    let date_stamp = &amz_date[..8.min(amz_date.len())];
    let canonical_headers = format!(
        "host:{host_header}\nx-amz-content-sha256:{payload_sha256_hex}\nx-amz-date:{amz_date}\n"
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let req_top = format!("{method}\n{canonical_uri}\n{canonical_query}");
    let req_bot = format!("{canonical_headers}\n{signed_headers}\n{payload_sha256_hex}");
    let canonical_request = format!("{req_top}\n{req_bot}");
    let canon_hash = sha256_hex(canonical_request.as_bytes());
    let credential_scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canon_hash}");

    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let sig = hex_encode(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let cred_part = format!("Credential={access_key}/{credential_scope}");
    let auth_val =
        format!("AWS4-HMAC-SHA256 {cred_part}, SignedHeaders={signed_headers}, Signature={sig}");
    vec![
        format!(
            "header = {}",
            escape_curl_config_val(&format!("Host: {host_header}"))
        ),
        format!(
            "header = {}",
            escape_curl_config_val(&format!("x-amz-date: {amz_date}"))
        ),
        format!(
            "header = {}",
            escape_curl_config_val(&format!("x-amz-content-sha256: {payload_sha256_hex}"))
        ),
        format!(
            "header = {}",
            escape_curl_config_val(&format!("Authorization: {auth_val}"))
        ),
    ]
}

fn current_amz_date() -> String {
    let now = SystemTime::now();
    let (y, m, d, hh, mm) = system_time_to_utc_civil(now);
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| (dur.as_secs() % 60) as u32)
        .unwrap_or(0);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{secs:02}Z")
}

pub fn s3_list_directory(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<FileMetadata>, VfsError> {
    let ep = resolve_s3_endpoint(profile)?;
    let access_key = profile.username.trim();
    let secret_key = secret.unwrap_or("").trim();
    if access_key.is_empty() || secret_key.is_empty() {
        return Err(VfsError::AuthFailed(
            "S3 connection requires Access Key ID and Secret Access Key".to_string(),
        ));
    }

    let prefix_rel = norm_path.trim_matches('/');
    let s3_prefix = if prefix_rel.is_empty() {
        String::new()
    } else {
        format!("{prefix_rel}/")
    };
    let canonical_uri = if ep.bucket_prefix.is_empty() {
        "/".to_string()
    } else {
        format!("{}/", ep.bucket_prefix)
    };
    let encoded_prefix = uri_encode_s3(&s3_prefix, true);
    let canonical_query = format!("delimiter=%2F&list-type=2&prefix={encoded_prefix}");
    let url = format!("{}{canonical_uri}?{canonical_query}", ep.base_url);
    let empty_sha = sha256_hex(b"");
    let amz_date = current_amz_date();

    let mut cfg = build_s3_sigv4_headers(&S3SigV4Params {
        method: "GET",
        host_header: &ep.host_header,
        canonical_uri: &canonical_uri,
        canonical_query: &canonical_query,
        region: &ep.region,
        access_key,
        secret_key,
        payload_sha256_hex: &empty_sha,
        amz_date: &amz_date,
    });
    cfg.push(format!("url = {}", escape_curl_config_val(&url)));
    let xml_bytes = run_curl_with_stdin_config(&cfg, &[])?;
    let xml_str = String::from_utf8_lossy(&xml_bytes);
    Ok(parse_s3_list_xml(
        &xml_str, norm_path, &s3_prefix, access_key,
    ))
}

/// Parses an AWS S3 `ListObjectsV2` XML response into [`FileMetadata`] entries.
pub fn parse_s3_list_xml(
    xml: &str,
    dir_norm: &str,
    s3_prefix: &str,
    owner: &str,
) -> Vec<FileMetadata> {
    let mut out = Vec::new();
    let trimmed_dir = dir_norm.trim_end_matches('/');

    // 1. Extract CommonPrefixes -> Folders
    let mut rem = xml;
    while let Some(start) = rem.find("<CommonPrefixes>") {
        let after = &rem[start + 16..];
        let Some(end) = after.find("</CommonPrefixes>") else {
            break;
        };
        let block = &after[..end];
        if let Some(pref) = extract_xml_tag_text(block, "Prefix") {
            let rel = pref.strip_prefix(s3_prefix).unwrap_or(&pref);
            let name = rel.trim_matches('/').to_string();
            if !name.is_empty() && !name.contains('/') {
                let full_path = format!("{trimmed_dir}/{name}");
                out.push(FileMetadata {
                    is_hidden: name.starts_with('.'),
                    name,
                    path: full_path,
                    extension: None,
                    kind: "Folder".to_string(),
                    size: None,
                    is_dir: true,
                    is_symlink: false,
                    is_locked: false,
                    hide_extension: false,
                    color_label: ColorLabel::None,
                    modified: Some(SystemTime::now()),
                    created: Some(SystemTime::now()),
                    added: Some(SystemTime::now()),
                    permissions: 0o755,
                    owner: Some(owner.to_string()),
                    group: Some("s3".to_string()),
                });
            }
        }
        rem = &after[end + 17..];
    }

    // 2. Extract Contents -> Files
    let mut rem_c = xml;
    while let Some(start) = rem_c.find("<Contents>") {
        let after = &rem_c[start + 10..];
        let Some(end) = after.find("</Contents>") else {
            break;
        };
        let block = &after[..end];
        if let Some(key) = extract_xml_tag_text(block, "Key") {
            let rel = key.strip_prefix(s3_prefix).unwrap_or(&key);
            if !rel.is_empty() && !rel.ends_with('/') && !rel.contains('/') {
                let name = rel.to_string();
                let size = extract_xml_tag_text(block, "Size")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let (extension, kind) = infer_file_kind(&name, false);
                let full_path = format!("{trimmed_dir}/{name}");
                out.push(FileMetadata {
                    is_hidden: name.starts_with('.'),
                    name,
                    path: full_path,
                    extension,
                    kind,
                    size: Some(size),
                    is_dir: false,
                    is_symlink: false,
                    is_locked: false,
                    hide_extension: false,
                    color_label: ColorLabel::None,
                    modified: Some(SystemTime::now()),
                    created: Some(SystemTime::now()),
                    added: Some(SystemTime::now()),
                    permissions: 0o644,
                    owner: Some(owner.to_string()),
                    group: Some("s3".to_string()),
                });
            }
        }
        rem_c = &after[end + 11..];
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

pub fn s3_read_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<u8>, VfsError> {
    let ep = resolve_s3_endpoint(profile)?;
    let key = norm_path.trim_start_matches('/');
    let encoded_key = uri_encode_s3(key, false);
    let canonical_uri = format!("{}/{encoded_key}", ep.bucket_prefix);
    let url = format!("{}{canonical_uri}", ep.base_url);
    let empty_sha = sha256_hex(b"");
    let amz_date = current_amz_date();
    let mut cfg = build_s3_sigv4_headers(&S3SigV4Params {
        method: "GET",
        host_header: &ep.host_header,
        canonical_uri: &canonical_uri,
        canonical_query: "",
        region: &ep.region,
        access_key: profile.username.trim(),
        secret_key: secret.unwrap_or("").trim(),
        payload_sha256_hex: &empty_sha,
        amz_date: &amz_date,
    });
    cfg.push(format!("url = {}", escape_curl_config_val(&url)));
    run_curl_with_stdin_config(&cfg, &[])
}

pub fn s3_write_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    data: &[u8],
) -> Result<(), VfsError> {
    let (tmp_path, mut tmp_file) = create_secure_temp_file("s3_io", "s3-put", ".bin")?;
    tmp_file.write_all(data)?;
    tmp_file.sync_all()?;
    drop(tmp_file);
    let res = s3_upload_file(profile, secret, &tmp_path, norm_path);
    let _ = fs::remove_file(&tmp_path);
    res
}

pub fn s3_upload_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    local_path: &Path,
    norm_path: &str,
) -> Result<(), VfsError> {
    let ep = resolve_s3_endpoint(profile)?;
    let key = norm_path.trim_start_matches('/');
    let encoded_key = uri_encode_s3(key, false);
    let canonical_uri = format!("{}/{encoded_key}", ep.bucket_prefix);
    let url = format!("{}{canonical_uri}", ep.base_url);
    let amz_date = current_amz_date();
    let payload_sha = "UNSIGNED-PAYLOAD";
    let mut cfg = build_s3_sigv4_headers(&S3SigV4Params {
        method: "PUT",
        host_header: &ep.host_header,
        canonical_uri: &canonical_uri,
        canonical_query: "",
        region: &ep.region,
        access_key: profile.username.trim(),
        secret_key: secret.unwrap_or("").trim(),
        payload_sha256_hex: payload_sha,
        amz_date: &amz_date,
    });
    cfg.push(format!("url = {}", escape_curl_config_val(&url)));
    cfg.push(format!(
        "upload-file = {}",
        escape_curl_config_val(&local_path.to_string_lossy())
    ));
    let _ = run_curl_with_stdin_config(&cfg, &["-X", "PUT"])?;
    Ok(())
}

pub fn s3_download_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    local_path: &Path,
) -> Result<(), VfsError> {
    let ep = resolve_s3_endpoint(profile)?;
    let key = norm_path.trim_start_matches('/');
    let encoded_key = uri_encode_s3(key, false);
    let canonical_uri = format!("{}/{encoded_key}", ep.bucket_prefix);
    let url = format!("{}{canonical_uri}", ep.base_url);
    let empty_sha = sha256_hex(b"");
    let amz_date = current_amz_date();
    let mut cfg = build_s3_sigv4_headers(&S3SigV4Params {
        method: "GET",
        host_header: &ep.host_header,
        canonical_uri: &canonical_uri,
        canonical_query: "",
        region: &ep.region,
        access_key: profile.username.trim(),
        secret_key: secret.unwrap_or("").trim(),
        payload_sha256_hex: &empty_sha,
        amz_date: &amz_date,
    });
    cfg.push(format!("url = {}", escape_curl_config_val(&url)));
    cfg.push(format!(
        "output = {}",
        escape_curl_config_val(&local_path.to_string_lossy())
    ));
    let _ = run_curl_with_stdin_config(&cfg, &[])?;
    Ok(())
}

pub fn s3_create_dir(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<(), VfsError> {
    let folder_key = format!("{}/", norm_path.trim_matches('/'));
    s3_write_bytes(profile, secret, &folder_key, b"")
}

pub fn s3_remove(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<(), VfsError> {
    let ep = resolve_s3_endpoint(profile)?;
    let key = norm_path.trim_start_matches('/');
    let encoded_key = uri_encode_s3(key, false);
    let canonical_uri = format!("{}/{encoded_key}", ep.bucket_prefix);
    let url = format!("{}{canonical_uri}", ep.base_url);
    let empty_sha = sha256_hex(b"");
    let amz_date = current_amz_date();
    let mut cfg = build_s3_sigv4_headers(&S3SigV4Params {
        method: "DELETE",
        host_header: &ep.host_header,
        canonical_uri: &canonical_uri,
        canonical_query: "",
        region: &ep.region,
        access_key: profile.username.trim(),
        secret_key: secret.unwrap_or("").trim(),
        payload_sha256_hex: &empty_sha,
        amz_date: &amz_date,
    });
    cfg.push(format!("url = {}", escape_curl_config_val(&url)));
    let _ = run_curl_with_stdin_config(&cfg, &["-X", "DELETE"])?;
    Ok(())
}

// ============================================================================
// 2. WebDAV (`PROPFIND`, `GET`, `PUT`, `MKCOL`, `DELETE`, `MOVE`, `COPY`)
// ============================================================================

fn resolve_webdav_url(profile: &ConnectionProfile, norm_path: &str) -> Result<String, VfsError> {
    let raw_host = profile
        .endpoint
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| profile.host.as_deref().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| VfsError::InvalidPath("Missing WebDAV server host/URL".to_string()))?
        .trim();
    let base = if raw_host.starts_with("http://") || raw_host.starts_with("https://") {
        raw_host.trim_end_matches('/').to_string()
    } else {
        let port_part = profile
            .port
            .filter(|&p| p != 80 && p != 443)
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        let scheme = if profile.port == Some(80) {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{}{port_part}", raw_host.trim_end_matches('/'))
    };
    let clean_rel = norm_path.trim_start_matches('/');
    if clean_rel.is_empty() {
        Ok(format!("{base}/"))
    } else {
        let encoded = uri_encode_s3(clean_rel, false);
        Ok(format!("{base}/{encoded}"))
    }
}

fn webdav_auth_config(profile: &ConnectionProfile, secret: Option<&str>, url: &str) -> Vec<String> {
    let mut cfg = vec![format!("url = {}", escape_curl_config_val(url))];
    if !profile.username.trim().is_empty() {
        let user_pass = format!("{}:{}", profile.username.trim(), secret.unwrap_or(""));
        cfg.push(format!("user = {}", escape_curl_config_val(&user_pass)));
    }
    cfg
}

pub fn webdav_list_directory(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<FileMetadata>, VfsError> {
    let url = resolve_webdav_url(profile, norm_path)?;
    let mut cfg = webdav_auth_config(profile, secret, &url);
    cfg.push("header = \"Depth: 1\"".to_string());
    cfg.push("header = \"Content-Type: application/xml; charset=utf-8\"".to_string());
    let xml_bytes = run_curl_with_stdin_config(&cfg, &["-X", "PROPFIND"])?;
    let xml_str = String::from_utf8_lossy(&xml_bytes);
    Ok(parse_webdav_propfind_xml(
        &xml_str,
        norm_path,
        &profile.username,
    ))
}

/// Parses a WebDAV `PROPFIND` `207 Multi-Status` XML response into [`FileMetadata`] items.
pub fn parse_webdav_propfind_xml(xml: &str, dir_norm: &str, owner: &str) -> Vec<FileMetadata> {
    let mut out = Vec::new();
    let trimmed_dir = dir_norm.trim_end_matches('/');
    let mut first = true;

    // Tag-agnostic scanning across `<d:response>`, `<D:response>`, or `<response>` blocks
    let mut cursor = 0usize;
    while let Some(rel_idx) = xml[cursor..].find("response>") {
        let tag_end = cursor + rel_idx + 9;
        // Check if it was an opening tag (not `</...response>`)
        let prefix_slice = &xml[cursor..cursor + rel_idx];
        let is_closing = prefix_slice.ends_with("</")
            || prefix_slice.ends_with("</d:")
            || prefix_slice.ends_with("</D:");
        if is_closing {
            cursor = tag_end;
            continue;
        }
        let Some(close_rel) = xml[tag_end..].find("response>") else {
            break;
        };
        let block = &xml[tag_end..tag_end + close_rel];
        cursor = tag_end + close_rel + 9;

        let Some(href) = extract_xml_tag_text_ns(block, "href") else {
            continue;
        };
        let decoded_href = percent_decode_str(&href);
        let clean_href = decoded_href.trim_end_matches('/');
        if first {
            first = false;
            if clean_href.ends_with(trimmed_dir) || trimmed_dir.is_empty() {
                continue;
            }
        }
        let Some(name_seg) = clean_href.rsplit('/').next() else {
            continue;
        };
        if name_seg.is_empty() || name_seg == "." || name_seg == ".." {
            continue;
        }
        let is_dir = block.contains("collection/>")
            || block.contains("collection />")
            || block.contains(":collection>");
        let size = extract_xml_tag_text_ns(block, "getcontentlength")
            .and_then(|s| s.trim().parse::<u64>().ok());
        let name = name_seg.to_string();
        let (extension, kind) = infer_file_kind(&name, is_dir);
        let full_path = format!("{trimmed_dir}/{name}");
        out.push(FileMetadata {
            is_hidden: name.starts_with('.'),
            name,
            path: full_path,
            extension,
            kind,
            size: if is_dir { None } else { size.or(Some(0)) },
            is_dir,
            is_symlink: false,
            is_locked: false,
            hide_extension: false,
            color_label: ColorLabel::None,
            modified: Some(SystemTime::now()),
            created: Some(SystemTime::now()),
            added: Some(SystemTime::now()),
            permissions: if is_dir { 0o755 } else { 0o644 },
            owner: Some(owner.to_string()),
            group: Some("webdav".to_string()),
        });
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

pub fn webdav_read_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<u8>, VfsError> {
    let url = resolve_webdav_url(profile, norm_path)?;
    let cfg = webdav_auth_config(profile, secret, &url);
    run_curl_with_stdin_config(&cfg, &[])
}

pub fn webdav_upload_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    local_path: &Path,
    norm_path: &str,
) -> Result<(), VfsError> {
    let url = resolve_webdav_url(profile, norm_path)?;
    let mut cfg = webdav_auth_config(profile, secret, &url);
    cfg.push(format!(
        "upload-file = {}",
        escape_curl_config_val(&local_path.to_string_lossy())
    ));
    let _ = run_curl_with_stdin_config(&cfg, &["-X", "PUT"])?;
    Ok(())
}

pub fn webdav_download_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    local_path: &Path,
) -> Result<(), VfsError> {
    let url = resolve_webdav_url(profile, norm_path)?;
    let mut cfg = webdav_auth_config(profile, secret, &url);
    cfg.push(format!(
        "output = {}",
        escape_curl_config_val(&local_path.to_string_lossy())
    ));
    let _ = run_curl_with_stdin_config(&cfg, &[])?;
    Ok(())
}

pub fn webdav_write_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    data: &[u8],
) -> Result<(), VfsError> {
    let (tmp_path, mut tmp_file) = create_secure_temp_file("webdav_io", "dav-put", ".bin")?;
    tmp_file.write_all(data)?;
    tmp_file.sync_all()?;
    drop(tmp_file);
    let res = webdav_upload_file(profile, secret, &tmp_path, norm_path);
    let _ = fs::remove_file(&tmp_path);
    res
}

pub fn webdav_create_dir(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<(), VfsError> {
    let url = resolve_webdav_url(profile, norm_path)?;
    let cfg = webdav_auth_config(profile, secret, &url);
    let _ = run_curl_with_stdin_config(&cfg, &["-X", "MKCOL"])?;
    Ok(())
}

pub fn webdav_remove(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<(), VfsError> {
    let url = resolve_webdav_url(profile, norm_path)?;
    let cfg = webdav_auth_config(profile, secret, &url);
    let _ = run_curl_with_stdin_config(&cfg, &["-X", "DELETE"])?;
    Ok(())
}

pub fn webdav_move_or_copy(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    from_norm: &str,
    to_norm: &str,
    is_move: bool,
) -> Result<(), VfsError> {
    let src_url = resolve_webdav_url(profile, from_norm)?;
    let dst_url = resolve_webdav_url(profile, to_norm)?;
    let mut cfg = webdav_auth_config(profile, secret, &src_url);
    cfg.push(format!(
        "header = {}",
        escape_curl_config_val(&format!("Destination: {dst_url}"))
    ));
    cfg.push("header = \"Overwrite: T\"".to_string());
    let method = if is_move { "MOVE" } else { "COPY" };
    let _ = run_curl_with_stdin_config(&cfg, &["-X", method])?;
    Ok(())
}

// ============================================================================
// 3. FTP & FTPS (`curl` passive FTP/FTPS with stdin credentials)
// ============================================================================

fn resolve_ftp_url(
    profile: &ConnectionProfile,
    norm_path: &str,
    is_dir: bool,
) -> Result<String, VfsError> {
    let host = profile
        .host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| VfsError::InvalidPath("Missing FTP host".to_string()))?;
    if host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(VfsError::InvalidPath(format!("Invalid FTP host: {host}")));
    }
    let scheme = if profile.protocol == ProtocolType::Ftps {
        "ftps"
    } else {
        "ftp"
    };
    let default_port = if profile.protocol == ProtocolType::Ftps {
        990
    } else {
        21
    };
    let port = profile.port.unwrap_or(default_port);
    let rel = norm_path.trim_matches('/');
    let encoded = uri_encode_s3(rel, false);
    let trailing = if is_dir || rel.is_empty() { "/" } else { "" };
    Ok(format!("{scheme}://{host}:{port}/{encoded}{trailing}"))
}

fn ftp_auth_config(profile: &ConnectionProfile, secret: Option<&str>, url: &str) -> Vec<String> {
    let mut cfg = vec![format!("url = {}", escape_curl_config_val(url))];
    let user = if profile.username.trim().is_empty() {
        "anonymous"
    } else {
        profile.username.trim()
    };
    let user_pass = format!("{user}:{}", secret.unwrap_or(""));
    cfg.push(format!("user = {}", escape_curl_config_val(&user_pass)));
    cfg
}

fn ftp_extra_args(profile: &ConnectionProfile, create_dirs: bool) -> Vec<&'static str> {
    let mut extra = vec!["--ftp-pasv"];
    if create_dirs {
        extra.push("--ftp-create-dirs");
    }
    if profile.protocol == ProtocolType::Ftps {
        extra.push("--ssl-reqd");
    }
    extra
}

pub fn ftp_list_directory(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<FileMetadata>, VfsError> {
    let url = resolve_ftp_url(profile, norm_path, true)?;
    let cfg = ftp_auth_config(profile, secret, &url);
    let extra = ftp_extra_args(profile, false);
    let raw = run_curl_with_stdin_config(&cfg, &extra)?;
    let text = String::from_utf8_lossy(&raw);
    let mut list = parse_ls_aln_output(norm_path, &text, &profile.username, None);
    list.sort_by_key(|a| a.name.to_lowercase());
    Ok(list)
}

pub fn ftp_read_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<u8>, VfsError> {
    let url = resolve_ftp_url(profile, norm_path, false)?;
    let cfg = ftp_auth_config(profile, secret, &url);
    let extra = ftp_extra_args(profile, false);
    run_curl_with_stdin_config(&cfg, &extra)
}

pub fn ftp_upload_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    local_path: &Path,
    norm_path: &str,
) -> Result<(), VfsError> {
    let url = resolve_ftp_url(profile, norm_path, false)?;
    let mut cfg = ftp_auth_config(profile, secret, &url);
    cfg.push(format!(
        "upload-file = {}",
        escape_curl_config_val(&local_path.to_string_lossy())
    ));
    let extra = ftp_extra_args(profile, true);
    let _ = run_curl_with_stdin_config(&cfg, &extra)?;
    Ok(())
}

pub fn ftp_download_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    local_path: &Path,
) -> Result<(), VfsError> {
    let url = resolve_ftp_url(profile, norm_path, false)?;
    let mut cfg = ftp_auth_config(profile, secret, &url);
    cfg.push(format!(
        "output = {}",
        escape_curl_config_val(&local_path.to_string_lossy())
    ));
    let extra = ftp_extra_args(profile, false);
    let _ = run_curl_with_stdin_config(&cfg, &extra)?;
    Ok(())
}

pub fn ftp_write_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    data: &[u8],
) -> Result<(), VfsError> {
    let (tmp_path, mut tmp_file) = create_secure_temp_file("ftp_io", "ftp-put", ".bin")?;
    tmp_file.write_all(data)?;
    tmp_file.sync_all()?;
    drop(tmp_file);
    let res = ftp_upload_file(profile, secret, &tmp_path, norm_path);
    let _ = fs::remove_file(&tmp_path);
    res
}

pub fn ftp_create_dir(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<(), VfsError> {
    let url = resolve_ftp_url(profile, "/", true)?;
    let mut cfg = ftp_auth_config(profile, secret, &url);
    let cmd = format!("MKD {}", norm_path.trim_start_matches('/'));
    cfg.push(format!("quote = {}", escape_curl_config_val(&cmd)));
    let extra = ftp_extra_args(profile, false);
    let _ = run_curl_with_stdin_config(&cfg, &extra)?;
    Ok(())
}

pub fn ftp_remove(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    is_dir: bool,
) -> Result<(), VfsError> {
    let url = resolve_ftp_url(profile, "/", true)?;
    let mut cfg = ftp_auth_config(profile, secret, &url);
    let verb = if is_dir { "RMD" } else { "DELE" };
    let cmd = format!("{verb} {}", norm_path.trim_start_matches('/'));
    cfg.push(format!("quote = {}", escape_curl_config_val(&cmd)));
    let extra = ftp_extra_args(profile, false);
    let _ = run_curl_with_stdin_config(&cfg, &extra)?;
    Ok(())
}

pub fn ftp_rename(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    from_norm: &str,
    to_norm: &str,
) -> Result<(), VfsError> {
    let url = resolve_ftp_url(profile, "/", true)?;
    let mut cfg = ftp_auth_config(profile, secret, &url);
    let rnfr = format!("RNFR {}", from_norm.trim_start_matches('/'));
    let rnto = format!("RNTO {}", to_norm.trim_start_matches('/'));
    cfg.push(format!("quote = {}", escape_curl_config_val(&rnfr)));
    cfg.push(format!("quote = {}", escape_curl_config_val(&rnto)));
    let extra = ftp_extra_args(profile, false);
    let _ = run_curl_with_stdin_config(&cfg, &extra)?;
    Ok(())
}

// ============================================================================
// 4. SMB (`smbclient` with 0600 authfile on Unix / UNC path on Windows)
// ============================================================================

fn parse_smb_share_and_subpath(
    profile: &ConnectionProfile,
    norm_path: &str,
) -> Result<(String, String, String), VfsError> {
    let host = profile
        .host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| VfsError::InvalidPath("Missing SMB server host".to_string()))?;
    if host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(VfsError::InvalidPath(format!("Invalid SMB host: {host}")));
    }
    let combined = norm_path.trim_matches('/');
    let base_clean = profile.base_path.trim_matches('/');
    let (share, sub) = if let Some((s, rest)) = combined.split_once('/') {
        (s.to_string(), rest.to_string())
    } else if !combined.is_empty() {
        (combined.to_string(), String::new())
    } else if !base_clean.is_empty() {
        let s = base_clean.split('/').next().unwrap_or(base_clean);
        (s.to_string(), String::new())
    } else {
        return Err(VfsError::InvalidPath(
            "SMB connection requires a share name in Path (e.g. /ShareName)".to_string(),
        ));
    };
    Ok((host.to_string(), share, sub))
}

#[cfg(windows)]
fn windows_connect_unc_path(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    host: &str,
    share: &str,
    sub: &str,
) -> Result<std::path::PathBuf, VfsError> {
    #[repr(C)]
    #[allow(dead_code)]
    struct NetResourceW {
        dw_scope: u32,
        dw_type: u32,
        dw_display_type: u32,
        dw_usage: u32,
        lp_local_name: *const u16,
        lp_remote_name: *const u16,
        lp_comment: *const u16,
        lp_provider: *const u16,
    }
    #[link(name = "mpr")]
    extern "system" {
        fn WNetAddConnection2W(
            lp_net_resource: *const NetResourceW,
            lp_password: *const u16,
            lp_user_name: *const u16,
            dw_flags: u32,
        ) -> u32;
    }

    let unc_share = format!("\\\\{host}\\{share}");
    let wide_unc: Vec<u16> = unc_share.encode_utf16().chain(std::iter::once(0)).collect();
    let user_trimmed = profile.username.trim();
    let wide_user: Vec<u16> = user_trimmed
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let pass_str = secret.unwrap_or("");
    let wide_pass: Vec<u16> = pass_str.encode_utf16().chain(std::iter::once(0)).collect();

    if !user_trimmed.is_empty() || !pass_str.is_empty() {
        let nr = NetResourceW {
            dw_scope: 0,
            dw_type: 1, // RESOURCETYPE_DISK
            dw_display_type: 0,
            dw_usage: 0,
            lp_local_name: std::ptr::null(),
            lp_remote_name: wide_unc.as_ptr(),
            lp_comment: std::ptr::null(),
            lp_provider: std::ptr::null(),
        };
        let u_ptr = if user_trimmed.is_empty() {
            std::ptr::null()
        } else {
            wide_user.as_ptr()
        };
        let p_ptr = if pass_str.is_empty() {
            std::ptr::null()
        } else {
            wide_pass.as_ptr()
        };
        // SAFETY: All UTF-16 buffers are NUL-terminated and live across the FFI call.
        let rc = unsafe { WNetAddConnection2W(&nr, p_ptr, u_ptr, 0) };
        // 0 = NO_ERROR, 85 = ERROR_ALREADY_ASSIGNED, 1219 = ERROR_SESSION_CREDENTIAL_CONFLICT
        if rc != 0 && rc != 85 && rc != 1219 {
            let msg = format!("Windows SMB authentication failed (Win32 error {rc})");
            return Err(VfsError::AuthFailed(msg));
        }
    }

    let mut full = std::path::PathBuf::from(unc_share);
    let clean_sub = sub.trim_matches(['/', '\\']);
    if !clean_sub.is_empty() {
        for seg in clean_sub.split(['/', '\\']) {
            if !seg.is_empty() {
                full.push(seg);
            }
        }
    }
    Ok(full)
}

#[cfg(not(windows))]
fn run_smbclient_cmd(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    share_unc: &str,
    smb_command: &str,
) -> Result<String, VfsError> {
    let (auth_path, mut auth_file) = create_secure_temp_file("smb_auth", "smb", ".auth")?;
    let auth_body = format!(
        "username = {}\npassword = {}\n",
        profile.username.trim().replace('\n', ""),
        secret.unwrap_or("").replace('\n', "")
    );
    auth_file.write_all(auth_body.as_bytes())?;
    auth_file.sync_all()?;
    drop(auth_file);

    let mut cmd = Command::new("smbclient");
    cmd.args([share_unc, "-A"])
        .arg(&auth_path)
        .args(["-c", smb_command]);
    if let Some(port) = profile.port {
        cmd.args(["-p", &port.to_string()]);
    }
    let out_res = cmd.output();
    let _ = fs::remove_file(&auth_path);
    let out = out_res.map_err(|e| {
        VfsError::NetworkError(format!(
            "Failed to run smbclient (ensure samba-client is installed): {e}"
        ))
    })?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(VfsError::NetworkError(if !msg.is_empty() {
            msg
        } else {
            err
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parses `smbclient -c ls` directory output right-to-left so spaces in filenames are preserved.
pub fn parse_smbclient_ls_output(raw: &str, norm_path: &str, owner: &str) -> Vec<FileMetadata> {
    let trimmed_dir = norm_path.trim_end_matches('/');
    let mut list = Vec::new();
    for raw_line in raw.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.ends_with("blocks available") {
            continue;
        }
        // Right-to-left peel: `<name>  [attr]  <size>  <dow> <mon> <day> <hh:mm:ss> <year>`
        let mut rest = trimmed;
        let mut tail_tokens = Vec::with_capacity(6);
        for _ in 0..6 {
            let r = rest.trim_end();
            let Some(pos) = r.rfind(char::is_whitespace) else {
                tail_tokens.clear();
                break;
            };
            tail_tokens.push(&r[pos + 1..]);
            rest = &r[..pos];
        }
        if tail_tokens.len() < 6 {
            continue;
        }
        // tail_tokens[0]=year, [1]=hh:mm:ss, [2]=day, [3]=mon, [4]=dow, [5]=size
        let year_ok =
            tail_tokens[0].len() == 4 && tail_tokens[0].chars().all(|c| c.is_ascii_digit());
        let time_ok = tail_tokens[1].contains(':');
        let Ok(size_val) = tail_tokens[5].parse::<u64>() else {
            continue;
        };
        if !year_ok || !time_ok {
            continue;
        }
        let left_trimmed = rest.trim_end();
        let (name_raw, attr) = if let Some(pos) = left_trimmed.rfind("  ") {
            let cand_attr = left_trimmed[pos..].trim();
            let is_attr = !cand_attr.is_empty()
                && cand_attr.len() <= 8
                && cand_attr
                    .chars()
                    .all(|c| matches!(c, 'D' | 'H' | 'R' | 'A' | 'S' | 'N' | 'C' | 'E' | 'T'));
            if is_attr {
                (left_trimmed[..pos].trim_end(), cand_attr)
            } else {
                (left_trimmed, "")
            }
        } else {
            (left_trimmed, "")
        };
        let name = name_raw.trim().to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let is_dir = attr.contains('D');
        let (extension, kind) = infer_file_kind(&name, is_dir);
        let full_path = format!("{trimmed_dir}/{name}");
        list.push(FileMetadata {
            is_hidden: name.starts_with('.') || attr.contains('H'),
            name,
            path: full_path,
            extension,
            kind,
            size: if is_dir { None } else { Some(size_val) },
            is_dir,
            is_symlink: false,
            is_locked: attr.contains('R'),
            hide_extension: false,
            color_label: ColorLabel::None,
            modified: Some(SystemTime::now()),
            created: Some(SystemTime::now()),
            added: Some(SystemTime::now()),
            permissions: if is_dir { 0o755 } else { 0o644 },
            owner: Some(owner.to_string()),
            group: Some("smb".to_string()),
        });
    }
    list.sort_by_key(|a| a.name.to_lowercase());
    list
}

pub fn smb_list_directory(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<FileMetadata>, VfsError> {
    let (host, share, sub) = parse_smb_share_and_subpath(profile, norm_path)?;
    #[cfg(windows)]
    {
        let unc_dir = windows_connect_unc_path(profile, secret, &host, &share, &sub)?;
        let trimmed_dir = norm_path.trim_end_matches('/');
        let mut list = Vec::new();
        for entry_res in fs::read_dir(&unc_dir)? {
            let entry = entry_res?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            let meta = entry.metadata()?;
            let is_dir = meta.is_dir();
            let (extension, kind) = infer_file_kind(&name, is_dir);
            let modified = meta.modified().ok();
            list.push(FileMetadata {
                is_hidden: name.starts_with('.'),
                path: format!("{trimmed_dir}/{name}"),
                name,
                extension,
                kind,
                size: if is_dir { None } else { Some(meta.len()) },
                is_dir,
                is_symlink: meta.file_type().is_symlink(),
                is_locked: meta.permissions().readonly(),
                hide_extension: false,
                color_label: ColorLabel::None,
                modified,
                created: meta.created().ok().or(modified),
                added: modified,
                permissions: if is_dir { 0o755 } else { 0o644 },
                owner: Some(profile.username.clone()),
                group: Some("smb".to_string()),
            });
        }
        list.sort_by_key(|a| a.name.to_lowercase());
        Ok(list)
    }
    #[cfg(not(windows))]
    {
        let share_unc = format!("//{host}/{share}");
        let smb_sub = sub.replace('/', "\\").replace('"', "");
        let cmd = if smb_sub.is_empty() {
            "ls".to_string()
        } else {
            format!("cd \"{smb_sub}\"; ls")
        };
        let raw = run_smbclient_cmd(profile, secret, &share_unc, &cmd)?;
        Ok(parse_smbclient_ls_output(
            &raw,
            norm_path,
            &profile.username,
        ))
    }
}

pub fn smb_download_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    local_path: &Path,
) -> Result<(), VfsError> {
    let (host, share, sub) = parse_smb_share_and_subpath(profile, norm_path)?;
    #[cfg(windows)]
    {
        let unc_file = windows_connect_unc_path(profile, secret, &host, &share, &sub)?;
        if let Some(parent) = local_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&unc_file, local_path)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let share_unc = format!("//{host}/{share}");
        let smb_sub = sub.replace('/', "\\").replace('"', "");
        let local_esc = local_path.to_string_lossy().replace('"', "");
        let cmd = format!("get \"{smb_sub}\" \"{local_esc}\"");
        let _ = run_smbclient_cmd(profile, secret, &share_unc, &cmd)?;
        Ok(())
    }
}

pub fn smb_upload_file(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    local_path: &Path,
    norm_path: &str,
) -> Result<(), VfsError> {
    let (host, share, sub) = parse_smb_share_and_subpath(profile, norm_path)?;
    #[cfg(windows)]
    {
        let unc_file = windows_connect_unc_path(profile, secret, &host, &share, &sub)?;
        if let Some(parent) = unc_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::copy(local_path, &unc_file)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let share_unc = format!("//{host}/{share}");
        let smb_sub = sub.replace('/', "\\").replace('"', "");
        let local_esc = local_path.to_string_lossy().replace('"', "");
        let cmd = format!("put \"{local_esc}\" \"{smb_sub}\"");
        let _ = run_smbclient_cmd(profile, secret, &share_unc, &cmd)?;
        Ok(())
    }
}

pub fn smb_read_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<Vec<u8>, VfsError> {
    let (tmp_path, _f) = create_secure_temp_file("smb_io", "smb-get", ".bin")?;
    let res = smb_download_file(profile, secret, norm_path, &tmp_path);
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    let bytes = fs::read(&tmp_path)?;
    let _ = fs::remove_file(&tmp_path);
    Ok(bytes)
}

pub fn smb_write_bytes(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    data: &[u8],
) -> Result<(), VfsError> {
    let (tmp_path, mut f) = create_secure_temp_file("smb_io", "smb-put", ".bin")?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    let res = smb_upload_file(profile, secret, &tmp_path, norm_path);
    let _ = fs::remove_file(&tmp_path);
    res
}

pub fn smb_create_dir(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
) -> Result<(), VfsError> {
    let (host, share, sub) = parse_smb_share_and_subpath(profile, norm_path)?;
    #[cfg(windows)]
    {
        let unc_dir = windows_connect_unc_path(profile, secret, &host, &share, &sub)?;
        fs::create_dir_all(&unc_dir)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let share_unc = format!("//{host}/{share}");
        let smb_sub = sub.replace('/', "\\").replace('"', "");
        let cmd = format!("mkdir \"{smb_sub}\"");
        let _ = run_smbclient_cmd(profile, secret, &share_unc, &cmd)?;
        Ok(())
    }
}

pub fn smb_remove(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    norm_path: &str,
    recursive: bool,
) -> Result<(), VfsError> {
    let (host, share, sub) = parse_smb_share_and_subpath(profile, norm_path)?;
    #[cfg(windows)]
    {
        let unc_target = windows_connect_unc_path(profile, secret, &host, &share, &sub)?;
        if unc_target.is_dir() {
            if recursive {
                fs::remove_dir_all(&unc_target)?;
            } else {
                fs::remove_dir(&unc_target)?;
            }
        } else {
            fs::remove_file(&unc_target)?;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let share_unc = format!("//{host}/{share}");
        let smb_sub = sub.replace('/', "\\").replace('"', "");
        let verb = if recursive { "deltree" } else { "rm" };
        let cmd = format!("{verb} \"{smb_sub}\"");
        let _ = run_smbclient_cmd(profile, secret, &share_unc, &cmd)?;
        Ok(())
    }
}

pub fn smb_rename(
    profile: &ConnectionProfile,
    secret: Option<&str>,
    from_norm: &str,
    to_norm: &str,
) -> Result<(), VfsError> {
    let (host, share, sub_from) = parse_smb_share_and_subpath(profile, from_norm)?;
    let (_, _, sub_to) = parse_smb_share_and_subpath(profile, to_norm)?;
    #[cfg(windows)]
    {
        let unc_from = windows_connect_unc_path(profile, secret, &host, &share, &sub_from)?;
        let unc_to = windows_connect_unc_path(profile, secret, &host, &share, &sub_to)?;
        fs::rename(&unc_from, &unc_to)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let share_unc = format!("//{host}/{share}");
        let s1 = sub_from.replace('/', "\\").replace('"', "");
        let s2 = sub_to.replace('/', "\\").replace('"', "");
        let cmd = format!("rename \"{s1}\" \"{s2}\"");
        let _ = run_smbclient_cmd(profile, secret, &share_unc, &cmd)?;
        Ok(())
    }
}

// ============================================================================
// Shared XML & URI Helpers
// ============================================================================

fn extract_xml_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml_unescape(&xml[start..start + end]))
}

fn extract_xml_tag_text_ns(xml: &str, local_tag: &str) -> Option<String> {
    for prefix in ["d:", "D:", "lp1:", ""] {
        let tag = format!("{prefix}{local_tag}");
        if let Some(val) = extract_xml_tag_text(xml, &tag) {
            return Some(val);
        }
    }
    None
}

fn xml_unescape(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn percent_decode_str(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
