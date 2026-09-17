use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tempfile::Builder;

pub const BACKEND_SERVICE: &str = "codex-connect.service";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub scope: ScopeSettings,
    #[serde(default)]
    pub backend: BackendConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScopeSettings {
    pub root: String,
}

impl Default for ScopeSettings {
    fn default() -> Self {
        Self {
            root: "~/projects".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendConfig {
    pub listen: String,
    pub codex_bin: String,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8767".to_string(),
            codex_bin: "codex".to_string(),
        }
    }
}

impl Config {
    pub fn new() -> Self {
        Self {
            scope: ScopeSettings::default(),
            backend: BackendConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.scope.root.trim().is_empty() {
            bail!("scope.root must not be empty");
        }
        self.backend.validate()
    }
}

impl BackendConfig {
    pub fn validate(&self) -> Result<()> {
        let listen: SocketAddr = self
            .listen
            .parse()
            .with_context(|| format!("backend has invalid listen address `{}`", self.listen))?;
        if !listen.ip().is_loopback() {
            bail!("backend must listen on loopback; found `{}`", self.listen);
        }
        for (field, value) in [("codex_bin", self.codex_bin.as_str())] {
            if value.trim().is_empty() {
                bail!("backend has an empty {field}");
            }
        }
        Ok(())
    }

    pub fn listen_addr(&self) -> Result<SocketAddr> {
        Ok(self.listen.parse()?)
    }
}

pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn default() -> Result<Self> {
        Ok(Self::at(config_path()?))
    }
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn exists(&self) -> bool {
        self.path.is_file()
    }
    pub fn read_bytes(&self) -> Result<Vec<u8>> {
        fs::read(&self.path)
            .with_context(|| format!("unable to read configuration {}", self.path.display()))
    }
    pub fn load(&self) -> Result<Config> {
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("unable to read configuration {}", self.path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("configuration {} is not valid TOML", self.path.display()))?;
        config
            .validate()
            .with_context(|| format!("configuration {} failed validation", self.path.display()))?;
        Ok(config)
    }
    pub fn save(&self, config: &Config) -> Result<()> {
        config.validate()?;
        self.write_atomic(toml::to_string_pretty(config)?.as_bytes())
    }
    pub fn restore_bytes(&self, bytes: &[u8]) -> Result<()> {
        self.write_atomic(bytes)
    }
    pub fn write_atomic(&self, bytes: &[u8]) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
        let mode = existing_mode(&self.path).unwrap_or(0o600);
        let mut temporary = Builder::new().prefix(".config-").tempfile_in(parent)?;
        set_file_mode(temporary.as_file(), mode)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        sync_directory(parent)
    }
}

fn config_path() -> Result<PathBuf> {
    Ok(config_root()?.join("codex-connect/config.toml"))
}
pub fn systemd_user_dir() -> Result<PathBuf> {
    Ok(config_root()?.join("systemd/user"))
}
pub fn systemd_registration_dir() -> Result<PathBuf> {
    // The user manager discovers registrations here. Canonical unit contents
    // remain under the workspace-local XDG config root; this directory should
    // contain only registration links for Codex Connect-managed units.
    Ok(home_dir()?.join(".config/systemd/user"))
}
pub fn config_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(default_workspace_root()?.join(".config"))
}
pub fn state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(default_workspace_root()?.join(".local/state"))
}
pub fn default_workspace_root() -> Result<PathBuf> {
    Ok(home_dir()?.join("projects"))
}
pub fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .context("HOME is not set; unable to resolve `~`")
}
pub fn expand_path(value: &str) -> Result<PathBuf> {
    let home = home_dir()?;
    let path = if value == "~" {
        home
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}
pub fn display_path(path: &Path) -> String {
    let Ok(home) = home_dir() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_string(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}
fn existing_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777)
}
pub(crate) fn set_file_mode(file: &fs::File, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    Ok(())
}
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_single_backend_configuration() {
        let config: Config = toml::from_str("[scope]\nroot = \"~/projects\"\n[backend]\nlisten = \"127.0.0.1:8767\"\ncodex_bin = \"codex\"\n").unwrap();
        config.validate().unwrap();
        assert!(toml::to_string_pretty(&config).unwrap().contains("[scope]"));
    }
}
