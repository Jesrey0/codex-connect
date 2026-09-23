use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::Builder;

pub(crate) fn atomic_write(path: &Path, prefix: &str, mode: u32, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("storage path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("unable to create {}", parent.display()))?;
    let mut temporary = Builder::new().prefix(prefix).tempfile_in(parent)?;
    set_file_mode(temporary.as_file(), mode)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_directory(parent)
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
