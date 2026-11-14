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
    /// When set, outgoing attachments given by path must resolve inside it.
    send_root: Option<PathBuf>,
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
        Ok(Self {
            dir,
            max_bytes,
            send_root: None,
        })
    }

    pub fn restrict_sends_to(&mut self, root: &Path) -> std::io::Result<()> {
        self.send_root = Some(std::fs::canonicalize(root)?);
        Ok(())
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

    /// Where an outgoing attachment path really points, if it may be sent.
    async fn check_path(&self, path: &Path) -> Result<PathBuf, String> {
        let cannot = |e: std::io::Error| format!("cannot read attachment {}: {e}", path.display());
        // Resolve symlinks and `..` first, so neither can step around the checks.
        let real = tokio::fs::canonicalize(path).await.map_err(cannot)?;
        if let Some(root) = &self.send_root
            && !real.starts_with(root)
        {
            return Err(format!(
                "attachment {} is outside --send-dir {}",
                path.display(),
                root.display()
            ));
        }
        // The data directory holds the account's keys. Only its attachments may go out.
        let dir = std::fs::canonicalize(&self.dir).unwrap_or_else(|_| self.dir.clone());
        if let Some(data_dir) = dir.parent()
            && real.starts_with(data_dir)
            && !real.starts_with(&dir)
        {
            return Err(format!(
                "attachment {} is in wigwag's data directory, which holds its keys",
                path.display()
            ));
        }
        // Devices and pipes (/dev/zero, a FIFO) could hang or exhaust memory.
        let meta = tokio::fs::metadata(&real).await.map_err(cannot)?;
        if !meta.is_file() {
            return Err(format!(
                "attachment {} is not a regular file",
                path.display()
            ));
        }
        if meta.len() > self.max_bytes {
            return Err(format!(
                "attachment is {} bytes, over the {} byte limit",
                meta.len(),
                self.max_bytes
            ));
        }
        Ok(real)
    }

    /// Reads an attachment to send: a file path, or a `data:<mime>[;filename=<name>];base64,<data>` URI.
    pub async fn outgoing(&self, spec: &str) -> Result<Outgoing, String> {
        let out = if let Some(rest) = spec.strip_prefix("data:") {
            parse_data_uri(rest)?
        } else {
            let path = Path::new(spec.strip_prefix("file://").unwrap_or(spec));
            let real = self.check_path(path).await?;
            let data = tokio::fs::read(&real)
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
    async fn send_dir_confines_paths() {
        let base = std::env::temp_dir().join("wigwag-test-senddir");
        let inside = base.join("ok");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("a.txt"), b"hi").unwrap();
        std::fs::write(base.join("secret.txt"), b"no").unwrap();
        let mut a = Attachments::new(base.join("data/attachments"), 1024).unwrap();
        a.restrict_sends_to(&inside).unwrap();
        assert!(
            a.outgoing(inside.join("a.txt").to_str().unwrap())
                .await
                .is_ok()
        );
        let escape = inside.join("../secret.txt");
        assert!(a.outgoing(escape.to_str().unwrap()).await.is_err());
        assert!(
            a.outgoing(base.join("secret.txt").to_str().unwrap())
                .await
                .is_err()
        );
        assert!(a.outgoing("data:text/plain;base64,aGk=").await.is_ok());
    }

    #[tokio::test]
    async fn refuses_keys_devices_and_big_files() {
        let base = std::env::temp_dir().join(format!("wigwag-test-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("wigwag.db"), b"keys").unwrap();
        let a = Attachments::new(base.join("attachments"), 4).unwrap();
        std::fs::write(base.join("attachments/ok.txt"), b"hi").unwrap();
        let db = a.outgoing(base.join("wigwag.db").to_str().unwrap()).await;
        assert!(db.unwrap_err().contains("holds its keys"));
        assert!(
            a.outgoing(base.join("attachments/ok.txt").to_str().unwrap())
                .await
                .is_ok()
        );
        let dev = a.outgoing("/dev/zero").await.unwrap_err();
        assert!(dev.contains("not a regular file"), "{dev}");
        let big = std::env::temp_dir().join(format!("wigwag-test-big-{}", std::process::id()));
        std::fs::write(&big, b"too big").unwrap();
        assert!(
            a.outgoing(big.to_str().unwrap())
                .await
                .unwrap_err()
                .contains("limit")
        );
    }

    #[tokio::test]
    async fn enforces_the_size_limit() {
        let a = Attachments::new(std::env::temp_dir().join("wigwag-test-size"), 1).unwrap();
        assert!(a.outgoing("data:text/plain;base64,aGk=").await.is_err());
    }
}
