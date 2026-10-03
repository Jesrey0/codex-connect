//! Build receipts are hints, never artifact identity or deployment authority.
//! Eligibility is deliberately conservative: unknown build settings still build normally.
use crate::artifact::{self, ArtifactIdentity};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const BUILD_ARGS: &[&str] = &["build", "--release", "-p", "codex-connect", "--locked"];
// Remove these from BOTH the build and its key. Detached jobs have new values on every prepare.
const JOB_ENV: &[&str] = &[
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "SYSTEMD_EXEC_PID",
    "MANAGERPID",
    "MEMORY_PRESSURE_WATCH",
    "MEMORY_PRESSURE_WRITE",
    "PWD",
    "SHLVL",
    "_",
];

pub(crate) fn build_command(cargo: &Path, source: &Path, target: &Path) -> Command {
    let mut command = Command::new(cargo);
    command
        .current_dir(source)
        .env("CARGO_TARGET_DIR", target)
        .args(BUILD_ARGS);
    for variable in JOB_ENV {
        command.env_remove(variable);
    }
    command
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    inputs: String,
    sha256: String,
}

pub(crate) fn reuse(target: &Path, inputs: &str) -> Result<Option<ArtifactIdentity>> {
    reuse_at(
        &target.join("prepare-receipt.json"),
        &target.join("release/codex-connect"),
        inputs,
    )
}

fn reuse_at(receipt: &Path, executable: &Path, inputs: &str) -> Result<Option<ArtifactIdentity>> {
    let bytes = match fs::read(receipt) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Ok(receipt) = serde_json::from_slice::<Receipt>(&bytes) else {
        return Ok(None);
    };
    if receipt.inputs != inputs {
        return Ok(None);
    }
    let Ok(identity) = artifact::for_path(executable) else {
        return Ok(None);
    };
    if identity.sha256 != receipt.sha256 {
        return Ok(None);
    }
    Ok(Some(identity))
}

/// Called under the deployment build lock whenever reuse has not been proven.
/// Cargo freshness uses timestamps, so discard all release compilation outputs,
/// including dependency artifacts and fingerprints, before an ordinary build.
pub(crate) fn invalidate(target: &Path) -> Result<()> {
    let receipt = target.join("prepare-receipt.json");
    match fs::remove_file(&receipt) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("unable to invalidate {}", receipt.display()));
        }
    }
    let release = target.join("release");
    match fs::symlink_metadata(&release) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("unable to inspect release outputs {}", release.display())
            });
        }
    }
    fs::remove_dir_all(&release)
        .with_context(|| format!("unable to invalidate release outputs {}", release.display()))
}

pub(crate) fn record(target: &Path, inputs: String, identity: &ArtifactIdentity) -> Result<()> {
    crate::storage::atomic_write(
        &target.join("prepare-receipt.json"),
        ".prepare-receipt-",
        0o600,
        &serde_json::to_vec(&Receipt {
            inputs,
            sha256: identity.sha256.clone(),
        })?,
    )
}

/// Hash contents, paths, file membership and the effective build context. Never use Git state.
/// Cargo metadata locates every resolved dependency, including path dependencies in dirty trees.
/// Hash entire package directories, not only the previous dep-info, so new inputs invalidate too.
pub(crate) fn fingerprint(source: &Path, cargo: &Path, target: &Path) -> Result<String> {
    // Arbitrary alternate compilers/flags can read inputs outside the known build graph.
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("CARGO_") && name != "CARGO_HOME" && name != "CARGO_TARGET_DIR"
            || name.starts_with("RUST") && name != "RUSTUP_HOME" && name != "RUSTUP_TOOLCHAIN"
            || matches!(name.as_ref(), "CC" | "CXX" | "AR" | "CFLAGS" | "CXXFLAGS")
            || name.starts_with("CC_")
            || name.starts_with("CFLAGS_")
            || matches!(
                name.as_ref(),
                "COMPILER_PATH"
                    | "GCC_EXEC_PREFIX"
                    | "LIBRARY_PATH"
                    | "CPATH"
                    | "C_INCLUDE_PATH"
                    | "CPLUS_INCLUDE_PATH"
                    | "LD_PRELOAD"
                    | "LD_LIBRARY_PATH"
                    | "RANLIB"
                    | "AS"
                    | "LD"
            )
            || name.ends_with("_CC")
            || name.ends_with("_CFLAGS")
        {
            bail!("custom build environment disables preparation reuse: {name}");
        }
    }
    let mut files = BTreeSet::new();
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
    ] {
        files.insert(source.join(name));
    }
    collect_tree(&source.join("config"), &mut files)?;
    let mut tools = BTreeSet::from([cargo.to_path_buf()]);
    for name in ["rustc", "gcc", "cc", "ar", "as", "ld", "mold", "sha256sum"] {
        tools.insert(resolve_tool(name)?);
    }
    native_inputs(source, &mut files)?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or(crate::paths::home_dir()?.join(".cargo"));
    let mut config_dirs: Vec<_> = source.ancestors().map(|path| path.join(".cargo")).collect();
    config_dirs.push(cargo_home);
    for directory in config_dirs {
        // Cargo ignores config.toml when the older config file exists.
        for name in ["config", "config.toml"] {
            let path = directory.join(name);
            files.insert(path.clone());
            match fs::read_to_string(&path) {
                Ok(text) => inspect_config(&toml::from_str(&text)?, &mut tools, &mut files)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let rustc = resolve_tool("rustc")?;
    let version = Command::new(&rustc)
        .current_dir(source)
        .arg("-vV")
        .output()?;
    if !version.status.success() {
        bail!("unable to identify Rust compiler");
    }
    let version = String::from_utf8(version.stdout)?;
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("Rust compiler has no host target")?;
    let mut metadata = Command::new(cargo);
    metadata
        .current_dir(source)
        .env("CARGO_TARGET_DIR", target)
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            host,
        ]);
    for variable in JOB_ENV {
        metadata.env_remove(variable);
    }
    let output = metadata.output()?;
    if !output.status.success() {
        bail!("unable to resolve build inputs with Cargo metadata");
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    for package in metadata["packages"]
        .as_array()
        .context("Cargo metadata has no packages")?
    {
        let root = Path::new(
            package["manifest_path"]
                .as_str()
                .context("package has no manifest")?,
        )
        .parent()
        .context("manifest has no parent")?;
        if package["source"].is_null()
            && package["targets"]
                .as_array()
                .context("package has no targets")?
                .iter()
                .any(|target| {
                    target["kind"]
                        .as_array()
                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "custom-build"))
                })
        {
            bail!("local build scripts disable preparation reuse");
        }
        if package.get("links").is_some_and(|links| !links.is_null()) && package["name"] != "ring" {
            bail!("additional native library dependencies disable preparation reuse");
        }
        // A workspace-root package could consume arbitrary top-level docs.
        if root == source {
            bail!("workspace-root packages disable preparation reuse");
        }
        collect_tree(root, &mut files)?;
    }
    // The actual compiler include closure also covers assets outside package directories.
    // A newly discovered external include changes the key after building, so no
    // receipt is saved until that include has a before/after content check.
    let dep_info_path = target.join("release/codex-connect.d");
    if dep_info_path.exists() {
        for path in dep_info_paths(&fs::read_to_string(dep_info_path)?)? {
            collect_tree(&path, &mut files)?;
        }
    }
    // Dependency build-script outputs declare additional native inputs. Relative paths
    // are already covered by hashing the full package; unknown absolute inputs are hashed.
    let build_outputs = target.join("release/build");
    if build_outputs.exists() {
        for entry in fs::read_dir(build_outputs)? {
            let output = entry?.path().join("output");
            if !output.is_file() {
                continue;
            }
            for line in fs::read_to_string(output)?.lines() {
                if let Some(path) = line
                    .strip_prefix("cargo:rerun-if-changed=")
                    .or_else(|| line.strip_prefix("cargo::rerun-if-changed="))
                {
                    let path = Path::new(path);
                    if path.is_absolute() {
                        collect_tree(path, &mut files)?;
                    } else if path
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
                    {
                        bail!("build script inputs outside its package disable preparation reuse");
                    }
                }
            }
        }
    }
    // Compiler, standard libraries and compiler shared libraries belong to the build inputs.
    let sysroot = Command::new(&rustc)
        .current_dir(source)
        .args(["--print", "sysroot"])
        .output()?;
    if !sysroot.status.success() {
        bail!("unable to resolve Rust sysroot");
    }
    let sysroot = PathBuf::from(String::from_utf8(sysroot.stdout)?.trim());
    collect_tree(&sysroot.join("lib"), &mut files)?;
    tools.insert(sysroot.join("bin/rustc"));
    tools.insert(sysroot.join("bin/cargo"));
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, version.as_bytes());
    hash_part(&mut hasher, source.as_os_str().as_encoded_bytes());
    hash_part(&mut hasher, target.as_os_str().as_encoded_bytes());
    hash_part(&mut hasher, &serde_json::to_vec(BUILD_ARGS)?);
    let mut environment: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| {
            !JOB_ENV.iter().any(|ignored| name == ignored) && name != "CARGO_TARGET_DIR"
        })
        .collect();
    environment.sort();
    for (name, value) in environment {
        hash_part(&mut hasher, name.as_encoded_bytes());
        hash_part(&mut hasher, value.as_encoded_bytes());
    }
    for tool in tools {
        let real = tool.canonicalize()?;
        files.insert(real);
        hash_part(&mut hasher, tool.as_os_str().as_encoded_bytes());
    }
    hash_files(&mut hasher, &files)?;
    Ok(format!("{:x}", hasher.finalize()))
}

// Native compiler helpers, headers and default link inputs are separate from rustc.
fn native_inputs(source: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    let gcc = resolve_tool("gcc")?;
    for (option, names) in [
        ("-print-prog-name=", &["cc1", "collect2"][..]),
        (
            "-print-file-name=",
            &[
                "include",
                "include-fixed",
                "libgcc.a",
                "libgcc_s.so",
                "libc.so",
                "libm.so",
                "libpthread.a",
                "librt.a",
                "libdl.a",
                "libutil.a",
                "crti.o",
                "crtn.o",
                "crt1.o",
                "Scrt1.o",
                "crtbeginS.o",
                "crtendS.o",
            ][..],
        ),
    ] {
        for name in names {
            let output = Command::new(&gcc)
                .current_dir(source)
                .arg(format!("{option}{name}"))
                .output()?;
            if !output.status.success() {
                bail!("unable to identify native compiler inputs");
            }
            let text = String::from_utf8(output.stdout)?;
            let path = Path::new(text.trim());
            // GCC echoes an unknown file name, e.g. include-fixed when not installed.
            if path.as_os_str() == *name && option == "-print-file-name=" {
                continue;
            }
            if !path.is_absolute() {
                bail!("unresolved native compiler input: {name}");
            }
            collect_tree(path, files)?;
            collect_linker_script_inputs(path, files)?;
        }
    }
    let output = Command::new(&gcc)
        .current_dir(source)
        .args(["-E", "-x", "c", "-v", "-"])
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("unable to identify native header search paths");
    }
    let stderr = String::from_utf8(output.stderr)?;
    let (_, search) = stderr
        .split_once("#include <...> search starts here:")
        .context("missing native header search paths")?;
    let (search, _) = search
        .split_once("End of search list.")
        .context("incomplete native header search paths")?;
    for line in search.lines().filter(|line| !line.trim().is_empty()) {
        let path = Path::new(line.trim());
        if !path.is_absolute() {
            bail!("unsupported native header search path");
        }
        collect_tree(path, files)?;
    }
    Ok(())
}

fn collect_linker_script_inputs(path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    if !path.is_file() || fs::metadata(path)?.len() > 65536 {
        return Ok(());
    }
    let bytes = fs::read(path)?;
    if !bytes.starts_with(b"/* GNU ld script") {
        return Ok(());
    }
    let text = std::str::from_utf8(&bytes)?;
    // GCC's default .so inputs can be scripts naming the real shared/static libraries.
    for token in
        text.split(|character: char| character.is_whitespace() || matches!(character, '(' | ')'))
    {
        if token.starts_with('/') && !token.starts_with("/*") && token != "/" {
            collect_tree(Path::new(token), files)?;
        }
    }
    Ok(())
}

fn inspect_config(
    config: &toml::Value,
    tools: &mut BTreeSet<PathBuf>,
    files: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let table = config.as_table().context("Cargo config is not a table")?;
    for (name, value) in table {
        match name.as_str() {
            "build" => {
                for (key, value) in value.as_table().context("invalid build config")? {
                    match key.as_str() {
                        "jobs" | "incremental" => {}
                        "rustc-wrapper" => {
                            let wrapper = value.as_str().context("invalid wrapper")?;
                            if Path::new(wrapper)
                                .file_name()
                                .and_then(|name| name.to_str())
                                != Some("sccache")
                            {
                                bail!("custom compiler wrappers disable preparation reuse");
                            }
                            tools.insert(resolve_tool(wrapper)?);
                        }
                        _ => bail!("unsupported Cargo build setting: {key}"),
                    }
                }
            }
            "target" => {
                for target in value.as_table().context("invalid target config")?.values() {
                    for (key, value) in target.as_table().context("invalid target config")? {
                        match key.as_str() {
                            "linker" => {
                                let linker = value.as_str().context("invalid linker")?;
                                if !matches!(
                                    Path::new(linker).file_name().and_then(|name| name.to_str()),
                                    Some("gcc" | "cc")
                                ) {
                                    bail!("custom compiler drivers disable preparation reuse");
                                }
                                tools.insert(resolve_tool(linker)?);
                            }
                            "rustflags" => {
                                let flags = value.as_array().context("unsupported rustflags")?;
                                for flag in flags {
                                    let flag = flag.as_str().context("invalid rustflag")?;
                                    if flag == "-C" {
                                        continue;
                                    }
                                    if let Some(directory) = flag.strip_prefix("link-arg=-B") {
                                        let directory = Path::new(directory);
                                        if !directory.is_absolute() {
                                            bail!(
                                                "relative linker search directories disable reuse"
                                            );
                                        }
                                        // -B selects compiler/linker helpers and native libraries.
                                        // Include membership so adding a higher-priority helper invalidates.
                                        for entry in fs::read_dir(directory)? {
                                            let path = entry?.path();
                                            let name = path
                                                .file_name()
                                                .and_then(|name| name.to_str())
                                                .context("non-UTF8 linker input")?;
                                            if matches!(
                                                name,
                                                "ld" | "ld.mold"
                                                    | "ld.bfd"
                                                    | "ld.gold"
                                                    | "as"
                                                    | "ar"
                                                    | "cc1"
                                                    | "cc1plus"
                                                    | "collect2"
                                            ) || matches!(
                                                path.extension()
                                                    .and_then(|extension| extension.to_str()),
                                                Some("o" | "a" | "so")
                                            ) {
                                                collect_tree(&path, files)?;
                                            }
                                        }
                                    } else if let Some(linker) =
                                        flag.strip_prefix("link-arg=-fuse-ld=")
                                    {
                                        if linker != "mold" {
                                            bail!(
                                                "unsupported native linker disables preparation reuse"
                                            );
                                        }
                                        tools.insert(resolve_tool(linker)?);
                                    } else {
                                        bail!("unsupported rustflag disables preparation reuse");
                                    }
                                }
                            }
                            _ => bail!("unsupported Cargo target setting: {key}"),
                        }
                    }
                }
            }
            "profile" => {} // Profile values are fully covered by config content.
            "env" => {
                if value
                    .as_table()
                    .context("invalid environment config")?
                    .keys()
                    .any(|key| key != "SCCACHE_DIR")
                {
                    bail!("custom Cargo environment disables preparation reuse");
                }
            }
            _ => bail!("unsupported Cargo config setting: {name}"),
        }
    }
    Ok(())
}

fn resolve_tool(name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path.components().count() != 1 {
        bail!("relative tool paths disable preparation reuse");
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH").context("PATH is unset")?) {
        let path = directory.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    bail!("build tool is unavailable: {name}")
}

fn collect_tree(path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            files.insert(path.to_path_buf());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.is_symlink() {
        let real = path.canonicalize()?;
        if !real.is_file() {
            bail!(
                "directory symlinks disable preparation reuse: {}",
                path.display()
            );
        }
        files.insert(path.to_path_buf());
        files.insert(real);
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | "node_modules")
            ) {
                continue;
            }
            collect_tree(&entry.path(), files)?;
        }
    } else if metadata.is_file() {
        files.insert(path.canonicalize()?);
    } else {
        bail!("non-file build input: {}", path.display());
    }
    Ok(())
}

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_files(hasher: &mut Sha256, files: &BTreeSet<PathBuf>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut present = Vec::new();
    for path in files {
        hash_part(hasher, path.as_os_str().as_encoded_bytes());
        match fs::metadata(path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    bail!("build input is no longer a file");
                }
                hasher.update([1]);
                hasher.update(metadata.len().to_le_bytes());
                hasher.update(metadata.mode().to_le_bytes());
                present.push(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => hasher.update([0]),
            Err(error) => return Err(error.into()),
        }
    }
    // Use the same host hashing primitive as artifact identity, batching to avoid
    // process-per-file cost and argument-size limits. --zero preserves literal paths.
    for paths in present.chunks(128) {
        let output = Command::new("sha256sum")
            .args(["--zero", "--"])
            .args(paths)
            .output()?;
        if !output.status.success() {
            bail!("unable to hash build input contents");
        }
        let entries: Vec<_> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .collect();
        if entries.len() != paths.len() {
            bail!("incomplete build input hashes");
        }
        for (entry, path) in entries.iter().zip(paths) {
            if entry.len() < 66
                || !entry[..64].iter().all(u8::is_ascii_hexdigit)
                || &entry[66..] != path.as_os_str().as_encoded_bytes()
            {
                bail!("invalid build input hash");
            }
            hash_part(hasher, &entry[..64]);
        }
    }
    Ok(())
}

fn dep_info_paths(text: &str) -> Result<Vec<PathBuf>> {
    let line = text.lines().next().context("empty dep-info")?;
    let (_, inputs) = line.split_once(": ").context("invalid dep-info")?;
    let mut paths = Vec::new();
    let mut path = String::new();
    let mut escaped = false;
    for character in inputs.chars() {
        if escaped {
            path.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() {
            if !path.is_empty() {
                paths.push(PathBuf::from(std::mem::take(&mut path)));
            }
        } else {
            path.push(character);
        }
    }
    if escaped {
        bail!("unterminated dep-info escape");
    }
    if !path.is_empty() {
        paths.push(PathBuf::from(path));
    }
    if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
        bail!("unsupported dep-info paths");
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidation_discards_the_release_tree_and_preserves_other_state() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path();
        for path in [
            "release/codex-connect",
            "release/deps/stale.rlib",
            "release/.fingerprint/stale.json",
            "release/build/generated.rs",
            "debug/codex-connect",
            "prepare-receipt.json",
        ] {
            let path = target.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"existing bytes").unwrap();
        }
        invalidate(target).unwrap();
        invalidate(target).unwrap();
        assert!(!target.join("release").exists());
        assert!(!target.join("prepare-receipt.json").exists());
        assert_eq!(
            fs::read(target.join("debug/codex-connect")).unwrap(),
            b"existing bytes"
        );
    }

    #[test]
    fn dep_info_handles_escaped_paths_and_rejects_missing_evidence() {
        assert_eq!(
            dep_info_paths("/target/app: /source\\ tree/main.rs /asset\n").unwrap(),
            vec![
                PathBuf::from("/source tree/main.rs"),
                PathBuf::from("/asset")
            ]
        );
        for invalid in ["", "invalid", "/app: relative", "/app: /dangling\\"] {
            assert!(dep_info_paths(invalid).is_err());
        }
    }

    #[test]
    fn unsupported_config_is_ineligible_instead_of_guessing_inputs() {
        for config in [
            "[build]\nrustc='custom'",
            "[env]\nCC='custom'",
            "[target.custom]\nrunner='custom'",
        ] {
            let config: toml::Value = toml::from_str(config).unwrap();
            assert!(inspect_config(&config, &mut BTreeSet::new(), &mut BTreeSet::new()).is_err());
        }
    }

    #[test]
    fn directory_symlinks_are_ineligible() {
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("loop");
        std::os::unix::fs::symlink(directory.path(), &link).unwrap();
        assert!(collect_tree(&link, &mut BTreeSet::new()).is_err());
    }

    #[test]
    fn detached_job_variables_are_removed_from_the_actual_build() {
        let command = build_command(
            Path::new("cargo"),
            Path::new("/source"),
            Path::new("/target"),
        );
        for variable in JOB_ENV {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == *variable && value.is_none())
            );
        }
        assert_eq!(command.get_args().collect::<Vec<_>>(), BUILD_ARGS);
    }
}

#[cfg(test)]
#[test]
#[ignore = "manual eligibility check with the host's real compiler/cache, without preparing or deploying"]
fn managed_input_probe() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let cargo = resolve_tool("cargo").unwrap();
    let target = crate::paths::cache_root()
        .unwrap()
        .join("codex-connect/deploy/build");
    let inputs = fingerprint(&source, &cargo, &target).unwrap();
    println!("Managed build input digest: {inputs}");
}
