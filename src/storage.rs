use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, Local};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub schema_version: u32,
    pub session_id: String,
    pub started_at: DateTime<FixedOffset>,
    pub ended_at: Option<DateTime<FixedOffset>>,
    pub duration_ms: Option<u64>,
    pub platform: String,
    pub arch: String,
    pub hostname: String,
    pub shell: String,
    pub initial_cwd: PathBuf,
    pub terminal: String,
    pub capture_input: bool,
    pub exit_code: Option<u32>,
    #[serde(default)]
    pub exit_signal: Option<String>,
    pub complete: bool,
    pub recording_error: Option<String>,
    pub utf8_replacements: u64,
    pub transcript_version: u32,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    pub term: Term,
    pub timestamp: i64,
    pub command: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub termlog: Extension,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Term {
    pub cols: u16,
    pub rows: u16,
    #[serde(rename = "type")]
    pub kind: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Extension {
    pub started_at: DateTime<FixedOffset>,
    pub transcript_version: u32,
}

pub fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            private_dir(parent)?;
        }
    }
    let mut b = fs::DirBuilder::new();
    b.mode(0o700);
    match b.create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        bail!("not a real directory: {}", path.display())
    }
    if m.uid() != unsafe { libc::geteuid() } {
        bail!("directory is not owned by current user: {}", path.display())
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn new_file(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    use std::os::unix::fs::OpenOptionsExt;
    o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    o.open(path)
        .with_context(|| format!("create {}", path.display()))
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = (|| {
        let mut f = new_file(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
pub fn write_metadata(path: &Path, m: &Metadata) -> Result<()> {
    atomic_write(&path.join("metadata.json"), &serde_json::to_vec_pretty(m)?)
}
pub fn create_session(
    root: &Path,
) -> Result<(String, PathBuf, DateTime<FixedOffset>, std::time::Instant)> {
    let start = std::time::Instant::now();
    let now = Local::now().fixed_offset();
    let id = Uuid::new_v4().to_string();
    private_dir(root)?;
    let mut dir = root.to_path_buf();
    for part in [
        "sessions".into(),
        now.format("%Y").to_string(),
        now.format("%m").to_string(),
        now.format("%d").to_string(),
        id.clone(),
    ] {
        dir.push(part);
        private_dir(&dir)?;
    }
    Ok((id, dir, now, start))
}
pub fn sessions(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(p: &Path, depth: u8, out: &mut Vec<PathBuf>) -> Result<()> {
        if depth == 0 {
            if p.join("metadata.json").is_file() {
                out.push(p.into());
            }
            return Ok(());
        }
        if !p.exists() {
            return Ok(());
        }
        for item in fs::read_dir(p)? {
            let item = item?;
            if item.file_type()?.is_dir() {
                walk(&item.path(), depth - 1, out)?;
            }
        }
        Ok(())
    }
    let mut result = vec![];
    walk(&root.join("sessions"), 4, &mut result)?;
    result.sort();
    result.reverse();
    Ok(result)
}
pub fn resolve(root: &Path, id: &str) -> Result<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        bail!("invalid session ID")
    }
    let matches: Vec<_> = sessions(root)?
        .into_iter()
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(id))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => bail!("session not found: {id}"),
        _ => bail!("ambiguous session prefix: {id}"),
    }
}
