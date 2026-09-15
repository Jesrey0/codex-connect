use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactIdentity {
    pub build_id: String,
    pub sha256: String,
    pub executable: PathBuf,
}

pub fn current() -> Result<ArtifactIdentity> {
    let executable = std::env::current_exe()
        .context("unable to locate the running codex-connect binary")?
        .canonicalize()
        .context("unable to canonicalize the running codex-connect binary")?;
    for_path(&executable)
}

pub fn for_path(path: &Path) -> Result<ArtifactIdentity> {
    let executable = path
        .canonicalize()
        .with_context(|| format!("unable to canonicalize {}", path.display()))?;
    let sha256 = sha256(&executable)?;
    Ok(ArtifactIdentity {
        build_id: sha256[..12].to_string(),
        sha256,
        executable,
    })
}

fn sha256(path: &Path) -> Result<String> {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .with_context(|| format!("unable to hash {} with sha256sum", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "sha256sum failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)?;
    let digest = stdout
        .split_whitespace()
        .next()
        .context("sha256sum returned no digest")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("sha256sum returned an invalid digest: {digest}");
    }
    Ok(digest.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_identity_is_content_addressed() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::write(&first, b"same bytes").unwrap();
        std::fs::write(&second, b"same bytes").unwrap();

        let left = for_path(&first).unwrap();
        let right = for_path(&second).unwrap();
        assert_eq!(left.sha256, right.sha256);
        assert_eq!(left.build_id, right.build_id);
        assert_eq!(left.build_id.len(), 12);
    }
}
