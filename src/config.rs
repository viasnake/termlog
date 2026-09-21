use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub capture_input: bool,
    pub recording_required: bool,
    pub flush_interval_ms: u64,
    pub shell: Option<Shell>,
    pub storage: Storage,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shell {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    pub path: Option<PathBuf>,
    pub retention_days: u32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            capture_input: false,
            recording_required: true,
            flush_interval_ms: 1000,
            shell: None,
            storage: Storage::default(),
        }
    }
}
fn home() -> Result<PathBuf> {
    Ok(env::var_os("HOME").context("HOME is not set")?.into())
}
fn xdg(name: &str, fallback: &str) -> Result<PathBuf> {
    match env::var_os(name).filter(|s| !s.is_empty()) {
        Some(v) if PathBuf::from(&v).is_absolute() => Ok(v.into()),
        _ => Ok(home()?.join(fallback)),
    }
}
impl Config {
    pub fn load(explicit: Option<PathBuf>) -> Result<Self> {
        let required = explicit.is_some();
        let path = match explicit {
            Some(p) => p,
            None => xdg("XDG_CONFIG_HOME", ".config")?.join("termlog/config.toml"),
        };
        let conf: Self = match fs::read_to_string(&path) {
            Ok(s) => {
                toml::from_str(&s).with_context(|| format!("invalid config: {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Self::default(),
            Err(e) => return Err(e).context("reading config"),
        };
        if conf.storage.retention_days != 0 {
            bail!("retention_days must be 0; automatic deletion is not implemented")
        }
        if conf.flush_interval_ms > 86_400_000 {
            bail!("flush_interval_ms exceeds one day")
        }
        if conf.shell.as_ref().is_some_and(|s| s.command.is_empty()) {
            bail!("shell.command is empty")
        }
        Ok(conf)
    }
    pub fn state_dir(&self) -> Result<PathBuf> {
        let p = match &self.storage.path {
            Some(p) => p.clone(),
            None => xdg("XDG_STATE_HOME", ".local/state")?.join("termlog"),
        };
        if !p.is_absolute() {
            bail!("storage.path must be absolute")
        }
        Ok(p)
    }
    pub fn shell_command(&self) -> Result<Vec<String>> {
        if let Some(s) = &self.shell {
            let mut v = vec![s.command.clone()];
            v.extend(s.args.clone());
            return Ok(v);
        }
        let shell = env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(account_shell)
            .context("no login shell; configure [shell]")?;
        Ok(vec![shell, "-l".into()])
    }
}
fn account_shell() -> Option<String> {
    // getpwuid is used only at startup, before any worker threads exist.
    unsafe {
        let p = libc::getpwuid(libc::geteuid());
        if p.is_null() || (*p).pw_shell.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*p).pw_shell)
                .to_string_lossy()
                .into_owned(),
        )
    }
}
