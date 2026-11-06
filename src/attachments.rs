//! Inbound attachments are downloaded as they arrive (as signal-cli does) and served
//! by id through `getAttachment`; outbound ones are read from a path or a data URI.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::prelude::*;
use presage::proto::AttachmentPointer;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Attachments {
    dir: PathBuf,
    pub max_bytes: u64,
}

/// A stable, filesystem-safe id: a hash of the pointer's digest (or key), plus an extension.
pub fn attachment_id(pointer: &AttachmentPointer) -> String {
    let mut hasher = Sha256::new();
    if let Some(d) = &pointer.digest {
        hasher.update(d);
    } else if let Some(k) = &pointer.key {
        hasher.update(k);
    }
    if let Some(name) = &pointer.file_name {
        hasher.update(name.as_bytes());
    }
    let hash = hex::encode(&hasher.finalize()[..12]);
    format!("{hash}.{}", extension(pointer))
}

fn extension(pointer: &AttachmentPointer) -> String {
    if let Some(ext) = pointer
        .file_name
        .as_deref()
        .and_then(|n| Path::new(n).extension())
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        return ext.to_ascii_lowercase();
    }
    let mime = pointer.content_type.as_deref().unwrap_or_default();
    let known = match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "audio/aac" => "aac",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    mime_guess::get_mime_extensions_str(mime)
        .and_then(|e| e.first())
        .map(|e| e.to_string())
        .unwrap_or_else(|| "bin".into())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// An attachment to send.
#[derive(Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub content_type: String,
    pub file_name: Option<String>,
    pub data: Vec<u8>,
}

impl Attachments {
    pub fn new(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir, max_bytes })
    }

    pub fn path(&self, id: &str) -> Option<PathBuf> {
        valid_id(id).then(|| self.dir.join(id))
    }

    pub async fn save(&self, id: &str, data: &[u8]) -> std::io::Result<()> {
        let path = self
            .path(id)
            .ok_or_else(|| std::io::Error::other("invalid attachment id"))?;
        let tmp = path.with_extension("part");
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &path).await
    }

    pub async fn read(&self, id: &str) -> std::io::Result<Vec<u8>> {
        let path = self
            .path(id)
            .ok_or_else(|| std::io::Error::other("invalid attachment id"))?;
        tokio::fs::read(path).await
    }

    pub fn exists(&self, id: &str) -> bool {
        self.path(id).is_some_and(|p| p.is_file())
    }

    /// Deletes downloaded attachments older than `ttl`. Returns how many went.
    pub fn prune(&self, ttl: Duration) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let cutoff = SystemTime::now() - ttl;
        entries
            .flatten()
            .filter(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t < cutoff)
            })
            .filter(|e| std::fs::remove_file(e.path()).is_ok())
            .count()
    }

    /// Reads an attachment to send: a file path, or a `data:<mime>[;filename=<name>];base64,<data>` URI.
    pub async fn outgoing(&self, spec: &str) -> Result<Outgoing, String> {
        let out = if let Some(rest) = spec.strip_prefix("data:") {
            parse_data_uri(rest)?
        } else {
            let path = Path::new(spec.strip_prefix("file://").unwrap_or(spec));
            let data = tokio::fs::read(path)
                .await
                .map_err(|e| format!("cannot read attachment {}: {e}", path.display()))?;
            Outgoing {
                content_type: mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .to_string(),
                file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                data,
            }
        };
        if out.data.len() as u64 > self.max_bytes {
            return Err(format!(
                "attachment is {} bytes, over the {} byte limit",
                out.data.len(),
                self.max_bytes
            ));
        }
        Ok(out)
    }
}

fn parse_data_uri(rest: &str) -> Result<Outgoing, String> {
    let (meta, payload) = rest
        .split_once(',')
        .ok_or("data URI has no payload".to_string())?;
    let mut parts = meta.split(';');
    let content_type = parts
        .next()
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    let mut file_name = None;
    let mut base64 = false;
    for p in parts {
        if p == "base64" {
            base64 = true;
        } else if let Some(name) = p.strip_prefix("filename=") {
            file_name = Some(name.to_string());
        }
    }
    if !base64 {
        return Err("only base64 data URIs are supported".into());
    }
    let data = BASE64_STANDARD
        .decode(payload.trim())
        .map_err(|e| format!("bad base64 in data URI: {e}"))?;
    Ok(Outgoing {
        content_type,
        file_name,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(name: Option<&str>, mime: &str) -> AttachmentPointer {
        AttachmentPointer {
            digest: Some(vec![1, 2, 3]),
            file_name: name.map(Into::into),
            content_type: Some(mime.into()),
            ..Default::default()
        }
    }

    #[test]
    fn ids_are_stable_safe_and_carry_an_extension() {
        let a = attachment_id(&pointer(Some("IMG_1.HEIC"), "image/heic"));
        assert_eq!(a, attachment_id(&pointer(Some("IMG_1.HEIC"), "image/heic")));
        assert!(a.ends_with(".heic"));
        assert!(valid_id(&a));
        assert!(attachment_id(&pointer(None, "image/jpeg")).ends_with(".jpg"));
        assert!(attachment_id(&pointer(Some("../../etc/passwd"), "x/unknown")).ends_with(".bin"));
    }

    #[test]
    fn refuses_ids_that_leave_the_directory() {
        let dir = std::env::temp_dir().join("wigwag-test-ids");
        let a = Attachments::new(dir, 1024).unwrap();
        assert!(a.path("../secret").is_none());
        assert!(a.path("a/b").is_none());
        assert!(a.path(".hidden").is_none());
        assert!(a.path("abc123.jpg").is_some());
    }

    #[test]
    fn parses_data_uris() {
        let out = parse_data_uri("image/png;filename=a.png;base64,aGk=").unwrap();
        assert_eq!(out.content_type, "image/png");
        assert_eq!(out.file_name.as_deref(), Some("a.png"));
        assert_eq!(out.data, b"hi");
        assert!(parse_data_uri("text/plain,hi").is_err());
    }

    #[tokio::test]
    async fn enforces_the_size_limit() {
        let a = Attachments::new(std::env::temp_dir().join("wigwag-test-size"), 1).unwrap();
        assert!(a.outgoing("data:text/plain;base64,aGk=").await.is_err());
    }
}
