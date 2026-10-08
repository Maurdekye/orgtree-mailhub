//! Attachment bytes on disk (`<HUB_DATA>/blobs/<id>`, the v1 layout, so an
//! existing data folder's files are used in place) and the upload limit.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Config;

/// `db.blob_path`: ids are server-minted hex; anything else is reduced to
/// its alphanumerics. v1 joined an id that reduced to nothing onto the
/// directory itself (a recorded gap); here that id has no path at all.
pub fn blob_path(dir: &Path, id: &str) -> Option<PathBuf> {
    let safe: String = id.chars().filter(|c| c.is_alphanumeric()).collect();
    (!safe.is_empty()).then(|| dir.join(safe))
}

/// Why the limit could not be read: v1 answered 503 rather than silently
/// widening it.
#[derive(Debug)]
pub struct InvalidLimit;

/// The current upload limit: the embedding host's runtime file when it is
/// configured and present, the startup default otherwise. Read once per
/// upload (and per /healthz), so a change never alters an upload already in
/// flight.
pub fn attachment_limit(cfg: &Config) -> Result<u64, InvalidLimit> {
    let Some(path) = &cfg.runtime_config_file else { return Ok(cfg.max_file_bytes) };
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cfg.max_file_bytes),
        Err(_) => return Err(InvalidLimit),
    };
    let v: Value = serde_json::from_slice(&text).map_err(|_| InvalidLimit)?;
    // `type(limit) is int and limit > 0`: a JSON integer, not 1e3, not true
    match v.get("max_attachment_bytes") {
        Some(Value::Number(n)) if !n.is_f64() => match n.as_u64() {
            Some(l) if l > 0 => Ok(l),
            _ => Err(InvalidLimit),
        },
        _ => Err(InvalidLimit),
    }
}

/// Remove partial uploads a killed hub left behind (no upload can be in
/// flight before the listeners open).
pub fn clean_partials(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "part").unwrap_or(false) && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// Deletes an upload's files unless it was committed (also on cancellation:
/// a client that hangs up mid-upload leaves nothing behind).
pub struct UploadFiles {
    pub partial: PathBuf,
    pub final_path: PathBuf,
    pub committed: bool,
}

impl Drop for UploadFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.partial);
        if !self.committed {
            let _ = std::fs::remove_file(&self.final_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_path_stays_inside() {
        let dir = Path::new("/data/blobs");
        for evil in ["../../etc/passwd", "..\\..\\win.ini", "a/b", "%2e%2e", "....//....//x"] {
            let p = blob_path(dir, evil).unwrap();
            assert_eq!(p.parent(), Some(dir), "{evil}");
        }
        for empty in ["..", "///", "%%%", "-", "."] {
            assert!(blob_path(dir, empty).is_none(), "{empty}");
        }
    }
}
