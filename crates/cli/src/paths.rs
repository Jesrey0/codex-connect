use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub fn systemd_user_dir() -> Result<PathBuf> {
    Ok(config_root()?.join("systemd/user"))
}
pub fn systemd_registration_dir() -> Result<PathBuf> {
    // The user manager discovers registrations here. Canonical unit contents
    // remain under the effective user XDG config root; this directory should
    // contain only registration links for Codex Connect-managed units.
    Ok(home_dir()?.join(".config/systemd/user"))
}
pub fn config_root() -> Result<PathBuf> {
    Ok(resolve_xdg_root(
        env::var_os("XDG_CONFIG_HOME"),
        home_dir()?.join(".config"),
    ))
}
pub fn state_root() -> Result<PathBuf> {
    Ok(resolve_xdg_root(
        env::var_os("XDG_STATE_HOME"),
        home_dir()?.join(".local/state"),
    ))
}
pub fn cache_root() -> Result<PathBuf> {
    Ok(resolve_xdg_root(
        env::var_os("XDG_CACHE_HOME"),
        home_dir()?.join(".cache"),
    ))
}
fn resolve_xdg_root(value: Option<OsString>, fallback: PathBuf) -> PathBuf {
    value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or(fallback)
}
pub fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .context("HOME is not set; unable to resolve `~`")
}
pub(crate) fn runtime_path(home: &Path, path: &OsStr) -> Result<String> {
    let mut directories = vec![home.join(".local/bin"), home.join(".cargo/bin")];
    let transient_codex = home.join(".codex/tmp");
    let vscode_server = home.join(".vscode-server");

    for directory in env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        let text = directory
            .to_str()
            .context("PATH contains a non-UTF-8 directory")?;
        if text.chars().any(char::is_control) {
            bail!("PATH contains a control character");
        }
        if directory.starts_with(&transient_codex)
            || directory.starts_with(&vscode_server)
            || is_wsl_windows_path(&directory)
        {
            continue;
        }
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }

    for directory in ["/usr/local/bin", "/usr/bin", "/bin", "/snap/bin"] {
        let directory = PathBuf::from(directory);
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    std::env::join_paths(directories)?
        .into_string()
        .map_err(|_| anyhow::anyhow!("PATH is not UTF-8"))
}

fn is_wsl_windows_path(path: &Path) -> bool {
    let mut components = path.components();
    if components.next() != Some(std::path::Component::RootDir)
        || components
            .next()
            .and_then(|component| component.as_os_str().to_str())
            != Some("mnt")
    {
        return false;
    }
    let Some(drive) = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic()
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_roots_ignore_empty_and_relative_values() {
        let fallback = PathBuf::from("/home/operator/.config");
        assert_eq!(resolve_xdg_root(None, fallback.clone()), fallback);
        assert_eq!(
            resolve_xdg_root(Some(OsString::from("")), fallback.clone()),
            fallback
        );
        assert_eq!(
            resolve_xdg_root(Some(OsString::from("relative/config")), fallback.clone()),
            fallback
        );
        assert_eq!(
            resolve_xdg_root(Some(OsString::from("/custom/config")), fallback),
            PathBuf::from("/custom/config")
        );
    }

    #[test]
    fn runtime_path_is_global_and_drops_relative_or_transient_entries() {
        let home = Path::new("/home/operator");
        let input = OsStr::new(
            "relative:/home/operator/.codex/tmp/arg0/run:/home/operator/.vscode-server/bin/remote-cli:/home/operator/.nvm/bin:/opt/custom/bin:/mnt/c/WINDOWS/system32:/mnt/data/tools:/usr/bin",
        );
        let normalized = runtime_path(home, input).unwrap();
        let entries = env::split_paths(OsStr::new(&normalized)).collect::<Vec<_>>();
        assert_eq!(entries[0], home.join(".local/bin"));
        assert_eq!(entries[1], home.join(".cargo/bin"));
        assert!(entries.contains(&home.join(".nvm/bin")));
        assert!(entries.contains(&PathBuf::from("/opt/custom/bin")));
        assert!(entries.contains(&PathBuf::from("/mnt/data/tools")));
        assert!(entries.contains(&PathBuf::from("/usr/bin")));
        assert!(
            !entries
                .iter()
                .any(|path| path.starts_with(home.join(".codex/tmp")))
        );
        assert!(
            !entries
                .iter()
                .any(|path| path.starts_with(home.join(".vscode-server")))
        );
        assert!(!entries.contains(&PathBuf::from("/mnt/c/WINDOWS/system32")));
        assert!(!entries.iter().any(|path| !path.is_absolute()));
    }
}
