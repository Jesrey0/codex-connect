//! Host path resolution plus Connect-only search, image, and patch operations.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::GenericImageView;
use image::ImageFormat;
use image::imageops::FilterType;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;

const MAX_SEARCH_RESULTS: usize = 100;
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
const PROMPT_IMAGE_PATCH_SIZE: u32 = 32;
const HIGH_IMAGE_MAX_DIMENSION: u32 = 2_048;
const HIGH_IMAGE_MAX_PATCHES: usize = 2_500;
const ORIGINAL_IMAGE_MAX_DIMENSION: u32 = 6_000;
const ORIGINAL_IMAGE_MAX_PATCHES: usize = 10_000;
const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".next",
    ".turbo",
    "build",
    "dist",
    "node_modules",
    "target",
];

#[derive(Debug, Error)]
pub enum HostError {
    #[error("the configured default cwd is not a directory")]
    InvalidDefaultCwd,
    #[error("host path is not valid UTF-8 for App Server")]
    NonUtf8Path,
    #[error("host operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("search query must not be empty")]
    EmptyQuery,
    #[error("host search was cancelled")]
    Cancelled,
    #[error("patch command failed: {0}")]
    PatchFailed(String),
    #[error("unsupported image type for {0}")]
    UnsupportedImage(String),
    #[error("image data is invalid for {0}")]
    InvalidImage(String),
    #[error("image detail must be high or original")]
    InvalidImageDetail,
    #[error("image exceeds the {MAX_IMAGE_BYTES}-byte limit")]
    LargeImage,
    #[error("host mutations do not allow symbolic links: {0}")]
    SymlinkMutation(String),
}

#[derive(Clone, Debug)]
pub struct Host {
    default_cwd: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageFile {
    pub path: String,
    pub mime_type: String,
    pub detail: String,
    #[serde(skip_serializing)]
    pub base64_data: String,
}

/// The configured default cwd is only the working directory for relative paths.
/// Absolute paths are host paths governed by the OS and the caller's execution policy.
impl Host {
    pub fn open(default_cwd: impl Into<PathBuf>) -> Result<Self, HostError> {
        let default_cwd = default_cwd.into().canonicalize()?;
        if !default_cwd.is_dir() {
            return Err(HostError::InvalidDefaultCwd);
        }
        Ok(Self { default_cwd })
    }

    pub fn default_cwd(&self) -> &Path {
        &self.default_cwd
    }

    /// Resolve a request-local cwd; omission selects the configured default cwd.
    pub fn resolve_cwd(&self, cwd: Option<&str>) -> Result<String, HostError> {
        self.resolve_app_server_directory(cwd.unwrap_or("."))
    }

    /// Join against an already resolved cwd.
    pub fn path_from_cwd(cwd: &str, requested: &str) -> Result<String, HostError> {
        Path::new(cwd)
            .join(requested)
            .to_str()
            .map(str::to_owned)
            .ok_or(HostError::NonUtf8Path)
    }

    pub fn resolve_app_server_existing(&self, requested: &str) -> Result<String, HostError> {
        let canonical = self.resolve_existing(requested)?;
        canonical
            .to_str()
            .map(str::to_owned)
            .ok_or(HostError::NonUtf8Path)
    }

    pub fn resolve_app_server_directory(&self, requested: &str) -> Result<String, HostError> {
        let candidate = self.resolve_app_server_existing(requested)?;
        if !Path::new(&candidate).is_dir() {
            return Err(HostError::PatchFailed(format!(
                "host path is not a directory: {requested}"
            )));
        }
        Ok(candidate)
    }

    pub fn search_with_cancel(
        &self,
        query: &str,
        requested: Option<&str>,
        result_base: Option<&str>,
        max_results: Option<usize>,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<SearchResults, HostError> {
        if query.is_empty() {
            return Err(HostError::EmptyQuery);
        }
        let root = match requested {
            Some(path) => self.resolve_existing(path)?,
            None => self.default_cwd.clone(),
        };
        let result_base = match result_base {
            Some(path) => self.resolve_existing(path)?,
            None => self.default_cwd.clone(),
        };
        let max_results = max_results.unwrap_or(MAX_SEARCH_RESULTS).clamp(1, 1_000);
        let mut matches = Vec::new();
        search_tree(
            &root,
            &result_base,
            &root,
            query,
            max_results + 1,
            &mut matches,
            &is_cancelled,
        )?;
        let truncated = matches.len() > max_results;
        matches.truncate(max_results);
        Ok(SearchResults { matches, truncated })
    }

    #[cfg(test)]
    fn search(
        &self,
        query: &str,
        requested: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<SearchResults, HostError> {
        self.search_with_cancel(query, requested, None, max_results, || false)
    }

    pub fn image_from_bytes(
        &self,
        requested: &str,
        bytes: Vec<u8>,
        detail: Option<&str>,
    ) -> Result<ImageFile, HostError> {
        let detail = match detail {
            None | Some("high") => "high",
            Some("original") => "original",
            Some(_) => return Err(HostError::InvalidImageDetail),
        };
        let path = self.resolve_existing(requested)?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(HostError::LargeImage);
        }
        let format = image::guess_format(&bytes)
            .map_err(|_| HostError::InvalidImage(self.relative(&path)))?;
        let mime_type = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::WebP => "image/webp",
            _ => return Err(HostError::UnsupportedImage(self.relative(&path))),
        };
        let image = image::load_from_memory_with_format(&bytes, format)
            .map_err(|_| HostError::InvalidImage(self.relative(&path)))?;
        let limits = match detail {
            "high" => (HIGH_IMAGE_MAX_DIMENSION, HIGH_IMAGE_MAX_PATCHES),
            "original" => (ORIGINAL_IMAGE_MAX_DIMENSION, ORIGINAL_IMAGE_MAX_PATCHES),
            _ => unreachable!("detail was validated above"),
        };
        let (width, height) = image.dimensions();
        let (target_width, target_height) =
            prompt_image_dimensions(width, height, limits.0, limits.1);
        let output_format = if matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
        ) {
            format
        } else {
            ImageFormat::Png
        };
        let (bytes, mime_type) =
            if (target_width, target_height) == (width, height) && output_format == format {
                (bytes, mime_type)
            } else {
                let image = if (target_width, target_height) == (width, height) {
                    image
                } else {
                    image.resize_exact(target_width, target_height, FilterType::Triangle)
                };
                let mut encoded = Cursor::new(Vec::new());
                image
                    .write_to(&mut encoded, output_format)
                    .map_err(|_| HostError::InvalidImage(self.relative(&path)))?;
                let encoded = encoded.into_inner();
                if encoded.len() > MAX_IMAGE_BYTES {
                    return Err(HostError::LargeImage);
                }
                let mime_type = match output_format {
                    ImageFormat::Png => "image/png",
                    ImageFormat::Jpeg => "image/jpeg",
                    ImageFormat::WebP => "image/webp",
                    _ => unreachable!("output image format is normalized above"),
                };
                (encoded, mime_type)
            };
        Ok(ImageFile {
            path: self.relative(&path),
            mime_type: mime_type.to_string(),
            detail: detail.to_string(),
            base64_data: STANDARD.encode(bytes),
        })
    }

    #[cfg(test)]
    fn image_with_detail(
        &self,
        requested: &str,
        detail: Option<&str>,
    ) -> Result<ImageFile, HostError> {
        let bytes = fs::read(self.resolve_existing(requested)?)?;
        self.image_from_bytes(requested, bytes, detail)
    }

    #[cfg(test)]
    fn image(&self, requested: &str) -> Result<ImageFile, HostError> {
        self.image_with_detail(requested, None)
    }

    pub fn apply_patch(&self, patch: &str, cwd: Option<&str>) -> Result<Vec<String>, HostError> {
        let document = parse_patch(patch)?;
        if document.environment_id.is_some() {
            return Err(HostError::PatchFailed(
                "apply_patch environment selection is unavailable for this host".to_string(),
            ));
        }
        let cwd = self.resolve_cwd(cwd)?;
        let plan = self.plan_patch(document.changes, &cwd)?;
        self.apply_patch_plan(plan)
    }

    fn plan_patch(&self, changes: Vec<PatchChange>, cwd: &str) -> Result<PatchPlan, HostError> {
        let mut actions = Vec::with_capacity(changes.len());
        let mut applied = Vec::with_capacity(changes.len());
        let mut touched = HashSet::new();
        for change in changes {
            match change {
                PatchChange::Add { path, content } => {
                    let path = self.resolve_mutation_path(&Self::path_from_cwd(cwd, &path)?)?;
                    ensure_patch_destination(&path)?;
                    register_patch_path(&mut touched, &path)?;
                    applied.push(self.relative(&path));
                    actions.push(PatchAction::Write {
                        path,
                        content,
                        permissions: None,
                    });
                }
                PatchChange::Delete { path } => {
                    let path = self.resolve_mutation_path(&Self::path_from_cwd(cwd, &path)?)?;
                    ensure_regular_file(&path)?;
                    register_patch_path(&mut touched, &path)?;
                    applied.push(self.relative(&path));
                    actions.push(PatchAction::Delete { path });
                }
                PatchChange::Update {
                    path,
                    move_path,
                    hunks,
                } => {
                    let source = self.resolve_mutation_path(&Self::path_from_cwd(cwd, &path)?)?;
                    ensure_regular_file(&source)?;
                    let permissions = fs::metadata(&source)?.permissions();
                    let original = fs::read(&source)?;
                    let original = std::str::from_utf8(&original).map_err(|_| {
                        HostError::PatchFailed(format!(
                            "cannot update non-UTF-8 file: {}",
                            self.relative(&source)
                        ))
                    })?;
                    let content = apply_hunks(original, &hunks)?;
                    let destination = match move_path {
                        Some(path) => {
                            self.resolve_mutation_path(&Self::path_from_cwd(cwd, &path)?)?
                        }
                        None => source.clone(),
                    };
                    ensure_patch_destination(&destination)?;
                    register_patch_path(&mut touched, &source)?;
                    if destination != source {
                        register_patch_path(&mut touched, &destination)?;
                    }
                    applied.push(self.relative(&destination));
                    actions.push(PatchAction::Write {
                        path: destination.clone(),
                        content,
                        permissions: Some(permissions),
                    });
                    if destination != source {
                        actions.push(PatchAction::Delete { path: source });
                    }
                }
            }
        }
        Ok(PatchPlan { actions, applied })
    }

    fn apply_patch_plan(&self, plan: PatchPlan) -> Result<Vec<String>, HostError> {
        let mut completed = Vec::with_capacity(plan.actions.len());
        let mut created_directories = Vec::new();
        let result = (|| -> Result<(), HostError> {
            for action in &plan.actions {
                let path = action.path();
                match action {
                    PatchAction::Write {
                        content,
                        permissions,
                        ..
                    } => {
                        created_directories.extend(create_missing_parent_directories(path)?);
                        let stage = StagedPatchFile::new(path, content, permissions.as_ref())?;
                        let backup = if path.exists() {
                            let backup = reserve_patch_backup(path)?;
                            if let Err(error) = fs::rename(path, &backup) {
                                let _ = fs::remove_file(&backup);
                                return Err(error.into());
                            }
                            Some(backup)
                        } else {
                            None
                        };
                        if let Err(error) = stage.install(path) {
                            if let Some(backup) = &backup {
                                let _ = fs::rename(backup, path);
                            }
                            return Err(error.into());
                        }
                        completed.push(CompletedPatchAction::Write {
                            path: path.to_path_buf(),
                            backup,
                        });
                    }
                    PatchAction::Delete { .. } => {
                        let backup = reserve_patch_backup(path)?;
                        if let Err(error) = fs::rename(path, &backup) {
                            let _ = fs::remove_file(&backup);
                            return Err(error.into());
                        }
                        completed.push(CompletedPatchAction::Delete {
                            path: path.to_path_buf(),
                            backup,
                        });
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            rollback_patch(&completed);
            remove_created_directories(&created_directories);
            return Err(error);
        }
        cleanup_patch_backups(&completed);
        Ok(plan.applied)
    }

    fn resolve_existing(&self, requested: &str) -> Result<PathBuf, HostError> {
        let requested = Path::new(requested);
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.default_cwd.join(requested)
        };
        Ok(candidate.canonicalize()?)
    }

    fn relative(&self, path: &Path) -> String {
        match path.strip_prefix(&self.default_cwd) {
            Ok(relative) => relative.to_string_lossy().to_string(),
            Err(_) => path.to_string_lossy().to_string(),
        }
    }

    fn resolve_mutation_path(&self, requested: &str) -> Result<PathBuf, HostError> {
        #[derive(Clone, Copy)]
        enum ComponentState {
            ExistingDirectory,
            ExistingNonDirectory,
            Missing,
        }

        let requested = Path::new(requested);
        let requested = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.default_cwd.join(requested)
        };
        let mut current = PathBuf::from("/");
        let mut states = Vec::new();
        for component in requested.components() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                std::path::Component::ParentDir => match states.last().copied() {
                    Some(ComponentState::ExistingDirectory) => {
                        states.pop();
                        current.pop();
                    }
                    Some(ComponentState::ExistingNonDirectory) => {
                        return Err(HostError::PatchFailed(format!(
                            "patch path traverses parent through a non-directory component: {}",
                            current.display()
                        )));
                    }
                    Some(ComponentState::Missing) => {
                        return Err(HostError::PatchFailed(format!(
                            "patch path traverses parent through a missing component: {}",
                            current.display()
                        )));
                    }
                    None => {
                        return Err(HostError::PatchFailed(format!(
                            "patch path traverses above the filesystem root: {}",
                            requested.display()
                        )));
                    }
                },
                std::path::Component::Normal(component) => {
                    current.push(component);
                    let state = match fs::symlink_metadata(&current) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            return Err(HostError::SymlinkMutation(self.relative(&current)));
                        }
                        Ok(metadata) if metadata.is_dir() => ComponentState::ExistingDirectory,
                        Ok(_) => ComponentState::ExistingNonDirectory,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            ComponentState::Missing
                        }
                        Err(error) => return Err(error.into()),
                    };
                    states.push(state);
                }
                std::path::Component::Prefix(_) => {
                    return Err(HostError::PatchFailed(format!(
                        "unsupported host mutation path: {}",
                        requested.display()
                    )));
                }
            }
        }
        Ok(current)
    }
}

fn search_tree(
    search_root: &Path,
    result_base: &Path,
    path: &Path,
    query: &str,
    max_results: usize,
    matches: &mut Vec<SearchMatch>,
    is_cancelled: &impl Fn() -> bool,
) -> Result<(), HostError> {
    if is_cancelled() {
        return Err(HostError::Cancelled);
    }
    if matches.len() >= max_results {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        if path != search_root && should_skip_directory(path) {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            search_tree(
                search_root,
                result_base,
                &entry.path(),
                query,
                max_results,
                matches,
                is_cancelled,
            )?;
            if matches.len() >= max_results {
                break;
            }
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Ok(());
    }
    if metadata.len() > MAX_SEARCH_FILE_BYTES {
        return Ok(());
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file).take(MAX_SEARCH_FILE_BYTES);
    for (index, line) in reader.lines().enumerate() {
        if is_cancelled() {
            return Err(HostError::Cancelled);
        }
        let line = match line {
            Ok(line) => line,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if line.contains(query) {
            matches.push(SearchMatch {
                path: path
                    .strip_prefix(result_base)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string(),
                line: index + 1,
                text: line.chars().take(1_000).collect(),
            });
            if matches.len() >= max_results {
                break;
            }
        }
    }
    Ok(())
}

fn prompt_image_dimensions(
    width: u32,
    height: u32,
    max_dimension: u32,
    max_patches: usize,
) -> (u32, u32) {
    let fits = |width: u32, height: u32| {
        width <= max_dimension
            && height <= max_dimension
            && u64::from(width.div_ceil(PROMPT_IMAGE_PATCH_SIZE))
                * u64::from(height.div_ceil(PROMPT_IMAGE_PATCH_SIZE))
                <= max_patches as u64
    };
    if fits(width, height) {
        return (width, height);
    }

    let max_dimension_scale = (f64::from(max_dimension) / f64::from(width.max(height))).min(1.0);
    let width = ((f64::from(width) * max_dimension_scale).round() as u32).max(1);
    let height = ((f64::from(height) * max_dimension_scale).round() as u32).max(1);
    if fits(width, height) {
        return (width, height);
    }

    let width_f64 = f64::from(width);
    let height_f64 = f64::from(height);
    let patch_size = f64::from(PROMPT_IMAGE_PATCH_SIZE);
    let mut scale = (patch_size * patch_size * max_patches as f64 / width_f64 / height_f64).sqrt();
    let scaled_patches_wide = width_f64 * scale / patch_size;
    let scaled_patches_high = height_f64 * scale / patch_size;
    scale *= (scaled_patches_wide.floor() / scaled_patches_wide)
        .min(scaled_patches_high.floor() / scaled_patches_high);

    (
        ((width_f64 * scale).floor() as u32).max(1),
        ((height_f64 * scale).floor() as u32).max(1),
    )
}

fn should_skip_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name))
}

struct PatchDocument {
    environment_id: Option<String>,
    changes: Vec<PatchChange>,
}

enum PatchChange {
    Add {
        path: String,
        content: Vec<u8>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        hunks: Vec<PatchHunk>,
    },
}

struct PatchHunk {
    lines: Vec<PatchLine>,
    change_context: Option<String>,
    end_of_file: bool,
}

enum PatchLine {
    Context(String),
    Add(String),
    Remove(String),
}

struct PatchPlan {
    actions: Vec<PatchAction>,
    applied: Vec<String>,
}

enum PatchAction {
    Write {
        path: PathBuf,
        content: Vec<u8>,
        permissions: Option<fs::Permissions>,
    },
    Delete {
        path: PathBuf,
    },
}

impl PatchAction {
    fn path(&self) -> &Path {
        match self {
            Self::Write { path, .. } | Self::Delete { path } => path,
        }
    }
}

enum CompletedPatchAction {
    Write {
        path: PathBuf,
        backup: Option<PathBuf>,
    },
    Delete {
        path: PathBuf,
        backup: PathBuf,
    },
}

fn parse_patch(patch: &str) -> Result<PatchDocument, HostError> {
    let lines = patch.trim().lines().collect::<Vec<_>>();
    if lines.first().map(|s| s.trim()) != Some("*** Begin Patch")
        || lines.last().map(|s| s.trim()) != Some("*** End Patch")
    {
        return Err(HostError::PatchFailed(
            "expected the official `*** Begin Patch` / `*** End Patch` format".to_string(),
        ));
    }
    let end = lines.len() - 1;
    let mut index = 1;
    let environment_id = lines
        .get(index)
        .and_then(|line| line.strip_prefix("*** Environment ID: "))
        .map(str::to_string);
    if environment_id.is_some() {
        if environment_id.as_deref() == Some("") {
            return Err(HostError::PatchFailed(
                "apply_patch environment_id cannot be empty".to_string(),
            ));
        }
        index += 1;
    }
    let mut changes = Vec::new();
    while index < end {
        let header = lines[index];
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            require_patch_path(path)?;
            index += 1;
            let mut content = String::new();
            let mut count = 0;
            while index < end && !lines[index].starts_with("*** ") {
                let line = lines[index].strip_prefix('+').ok_or_else(|| {
                    HostError::PatchFailed("added file lines must start with `+`".to_string())
                })?;
                content.push_str(line);
                content.push('\n');
                count += 1;
                index += 1;
            }
            if count == 0 {
                return Err(HostError::PatchFailed(
                    "added files require at least one `+` line".to_string(),
                ));
            }
            changes.push(PatchChange::Add {
                path: path.to_string(),
                content: content.into_bytes(),
            });
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Delete File: ") {
            require_patch_path(path)?;
            changes.push(PatchChange::Delete {
                path: path.to_string(),
            });
            index += 1;
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Update File: ") {
            require_patch_path(path)?;
            let path = path.to_string();
            index += 1;
            let move_path = lines
                .get(index)
                .and_then(|line| line.strip_prefix("*** Move to: "));
            let move_path = match move_path {
                Some(destination) => {
                    require_patch_path(destination)?;
                    index += 1;
                    Some(destination.to_string())
                }
                None => None,
            };
            let mut hunks = Vec::new();
            while index < end && !lines[index].starts_with("*** ") {
                let header = lines[index];
                if header.trim().is_empty() {
                    index += 1;
                    continue;
                }
                if !header.starts_with("@@") {
                    return Err(HostError::PatchFailed(
                        "updated files require `@@` hunk headers".to_string(),
                    ));
                }
                let change_context = if header == "@@" {
                    None
                } else if let Some(context) = header.strip_prefix("@@ ") {
                    if context.is_empty() {
                        None
                    } else {
                        Some(context.to_string())
                    }
                } else {
                    return Err(HostError::PatchFailed(
                        "patch hunk headers must be `@@` or `@@ <context>`".to_string(),
                    ));
                };
                index += 1;
                let mut hunk_lines = Vec::new();
                let mut end_of_file = false;
                while index < end
                    && !lines[index].starts_with("@@")
                    && lines[index] != "*** End of File"
                    && !lines[index].starts_with("*** ")
                {
                    let line = lines[index];
                    let parsed = match line.chars().next() {
                        Some(' ') => PatchLine::Context(line[1..].to_string()),
                        Some('+') => PatchLine::Add(line[1..].to_string()),
                        Some('-') => PatchLine::Remove(line[1..].to_string()),
                        _ => {
                            if line.trim().is_empty() {
                                let next_non_empty =
                                    lines[index..=end].iter().find(|l| !l.trim().is_empty());
                                if let Some(next) = next_non_empty
                                    && (next.starts_with("@@") || next.starts_with("*** "))
                                {
                                    while index < end && lines[index].trim().is_empty() {
                                        index += 1;
                                    }
                                    break;
                                }
                            }
                            return Err(HostError::PatchFailed(
                                "patch hunk lines must start with ` `, `+`, or `-`".to_string(),
                            ));
                        }
                    };
                    hunk_lines.push(parsed);
                    index += 1;
                }
                if index < end && lines[index] == "*** End of File" {
                    end_of_file = true;
                    index += 1;
                }
                if hunk_lines.is_empty() {
                    return Err(HostError::PatchFailed("patch hunk is empty".to_string()));
                }
                hunks.push(PatchHunk {
                    lines: hunk_lines,
                    change_context,
                    end_of_file,
                });
                if end_of_file {
                    while index < end && lines[index].trim().is_empty() {
                        index += 1;
                    }
                    break;
                }
            }
            if hunks.is_empty() && move_path.is_none() {
                return Err(HostError::PatchFailed(
                    "updated files require a hunk or `*** Move to:` directive".to_string(),
                ));
            }
            changes.push(PatchChange::Update {
                path,
                move_path,
                hunks,
            });
            continue;
        }
        return Err(HostError::PatchFailed(format!(
            "unknown patch directive: {header}"
        )));
    }
    if changes.is_empty() {
        return Err(HostError::PatchFailed(
            "patch contains no file changes".to_string(),
        ));
    }
    Ok(PatchDocument {
        environment_id,
        changes,
    })
}

fn require_patch_path(path: &str) -> Result<(), HostError> {
    if path.is_empty() {
        return Err(HostError::PatchFailed(
            "patch file paths must not be empty".to_string(),
        ));
    }
    Ok(())
}

fn apply_hunks(original: &str, hunks: &[PatchHunk]) -> Result<Vec<u8>, HostError> {
    let newline = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let trailing_newline = original.ends_with(newline);
    let body = original.strip_suffix(newline).unwrap_or(original);
    let mut current = if body.is_empty() {
        Vec::new()
    } else {
        body.split(newline).map(str::to_string).collect::<Vec<_>>()
    };
    let mut search_start = 0usize;
    for hunk in hunks {
        if let Some(context) = &hunk.change_context {
            let pattern = [context.clone()];
            let position =
                find_lines(&current, &pattern, search_start, false).ok_or_else(|| {
                    HostError::PatchFailed(format!("patch context `{context}` was not found"))
                })?;
            search_start = position + 1;
        }
        let old = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(value) | PatchLine::Remove(value) => Some(value.clone()),
                PatchLine::Add(_) => None,
            })
            .collect::<Vec<_>>();
        let replacement = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(value) | PatchLine::Add(value) => Some(value.clone()),
                PatchLine::Remove(_) => None,
            })
            .collect::<Vec<_>>();
        let position = if old.is_empty() {
            if hunk.end_of_file {
                current.len()
            } else {
                search_start.min(current.len())
            }
        } else {
            find_lines(&current, &old, search_start, hunk.end_of_file).ok_or_else(|| {
                HostError::PatchFailed("patch context did not match the target file".to_string())
            })?
        };
        let removed = old.len();
        current.splice(position..position + removed, replacement.clone());
        search_start = position + replacement.len();
    }
    let mut output = current.join(newline);
    if trailing_newline || !output.is_empty() {
        output.push_str(newline);
    }
    Ok(output.into_bytes())
}

fn find_lines(
    lines: &[String],
    pattern: &[String],
    start: usize,
    end_of_file: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(lines.len()));
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let matches_at = |start: usize| {
        start + pattern.len() <= lines.len()
            && lines[start..start + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(actual, expected)| actual == expected)
    };
    let matches_rstrip = |start: usize| {
        start + pattern.len() <= lines.len()
            && lines[start..start + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(actual, expected)| actual.trim_end() == expected.trim_end())
    };
    let matches_trim = |start: usize| {
        start + pattern.len() <= lines.len()
            && lines[start..start + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(actual, expected)| actual.trim() == expected.trim())
    };
    let search_start = if end_of_file {
        lines.len().saturating_sub(pattern.len()).max(start)
    } else {
        start
    };
    let last = lines.len().saturating_sub(pattern.len());
    if search_start > last {
        return None;
    }
    (search_start..=last)
        .find(|index| matches_at(*index))
        .or_else(|| (search_start..=last).find(|index| matches_rstrip(*index)))
        .or_else(|| (search_start..=last).find(|index| matches_trim(*index)))
}

fn ensure_regular_file(path: &Path) -> Result<(), HostError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(HostError::SymlinkMutation(path.display().to_string()));
    }
    if !metadata.is_file() {
        return Err(HostError::PatchFailed(format!(
            "expected a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_patch_destination(path: &Path) -> Result<(), HostError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(HostError::SymlinkMutation(path.display().to_string()))
        }
        Ok(metadata) if !metadata.is_file() => Err(HostError::PatchFailed(format!(
            "patch destination is not a regular file: {}",
            path.display()
        ))),
        Ok(_) | Err(_) => Ok(()),
    }
}

fn register_patch_path(touched: &mut HashSet<PathBuf>, path: &Path) -> Result<(), HostError> {
    if touched.insert(path.to_path_buf()) {
        Ok(())
    } else {
        Err(HostError::PatchFailed(format!(
            "patch changes the same path more than once: {}",
            path.display()
        )))
    }
}

fn create_missing_parent_directories(path: &Path) -> Result<Vec<PathBuf>, HostError> {
    let mut missing = Vec::new();
    let mut current = path.parent().ok_or_else(|| {
        HostError::PatchFailed(format!(
            "patch destination has no parent: {}",
            path.display()
        ))
    })?;
    while !current.exists() {
        missing.push(current.to_path_buf());
        current = current.parent().ok_or_else(|| {
            HostError::PatchFailed(format!(
                "unable to resolve patch destination parent: {}",
                path.display()
            ))
        })?;
    }
    if !current.is_dir() {
        return Err(HostError::PatchFailed(format!(
            "patch destination parent is not a directory: {}",
            current.display()
        )));
    }
    let mut created = Vec::with_capacity(missing.len());
    for directory in missing.iter().rev() {
        fs::create_dir(directory)?;
        created.push(directory.clone());
    }
    Ok(created)
}

struct StagedPatchFile {
    path: PathBuf,
    armed: bool,
}

impl StagedPatchFile {
    fn new(
        path: &Path,
        content: &[u8],
        permissions: Option<&fs::Permissions>,
    ) -> Result<Self, HostError> {
        let parent = path.parent().ok_or_else(|| {
            HostError::PatchFailed(format!(
                "patch destination has no parent: {}",
                path.display()
            ))
        })?;
        let temporary = tempfile::Builder::new()
            .prefix(".codex-connect-patch-stage-")
            .tempfile_in(parent)?;
        let stage = temporary.path().to_path_buf();
        temporary.close()?;
        let guard = Self {
            path: stage,
            armed: true,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&guard.path)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions.clone())?;
        }
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        Ok(guard)
    }

    fn install(mut self, destination: &Path) -> Result<(), std::io::Error> {
        fs::rename(&self.path, destination)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for StagedPatchFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn reserve_patch_backup(path: &Path) -> Result<PathBuf, HostError> {
    let parent = path.parent().ok_or_else(|| {
        HostError::PatchFailed(format!("patch path has no parent: {}", path.display()))
    })?;
    let temporary = tempfile::Builder::new()
        .prefix(".codex-connect-patch-backup-")
        .tempfile_in(parent)?;
    let (file, backup) = temporary.keep().map_err(|error| error.error)?;
    drop(file);
    Ok(backup)
}

fn cleanup_patch_backups(completed: &[CompletedPatchAction]) {
    for action in completed {
        match action {
            CompletedPatchAction::Write {
                backup: Some(backup),
                ..
            }
            | CompletedPatchAction::Delete { backup, .. } => {
                let _ = fs::remove_file(backup);
            }
            CompletedPatchAction::Write { backup: None, .. } => {}
        }
    }
}

fn rollback_patch(completed: &[CompletedPatchAction]) {
    for action in completed.iter().rev() {
        match action {
            CompletedPatchAction::Write { path, backup } => {
                let _ = fs::remove_file(path);
                if let Some(backup) = backup {
                    let _ = fs::rename(backup, path);
                }
            }
            CompletedPatchAction::Delete { path, backup } => {
                let _ = fs::rename(backup, path);
            }
        }
    }
}

fn remove_created_directories(directories: &[PathBuf]) {
    for directory in directories.iter().rev() {
        let _ = fs::remove_dir(directory);
    }
}

#[cfg(test)]
mod tests {
    use super::Host;
    use super::HostError;
    use super::StagedPatchFile;
    use base64::Engine;
    use std::fs;

    #[test]
    fn app_server_paths_use_default_cwd_but_allow_absolute_host_paths() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        fs::write(workspace_dir.path().join("inside.txt"), "inside").unwrap();
        fs::write(outside_dir.path().join("outside.txt"), "outside").unwrap();
        let host = Host::open(workspace_dir.path()).unwrap();

        let resolved = host.resolve_app_server_existing("inside.txt").unwrap();
        assert_eq!(
            std::path::Path::new(&resolved).canonicalize().unwrap(),
            workspace_dir
                .path()
                .join("inside.txt")
                .canonicalize()
                .unwrap()
        );
        let outside = host
            .resolve_app_server_existing(outside_dir.path().join("outside.txt").to_str().unwrap())
            .unwrap();
        assert_eq!(
            std::path::Path::new(&outside),
            outside_dir
                .path()
                .join("outside.txt")
                .canonicalize()
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_write_can_cross_filesystems() {
        use std::os::unix::fs::MetadataExt;

        let workspace = tempfile::tempdir().unwrap();
        let Ok(destination) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        if workspace.path().metadata().unwrap().dev()
            == destination.path().metadata().unwrap().dev()
        {
            return;
        }
        let host = Host::open(workspace.path()).unwrap();
        let target = destination.path().join("cross-device.txt");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+cross-device\n*** End Patch\n",
            target.display()
        );
        host.apply_patch(&patch, None).unwrap();
        assert_eq!(fs::read_to_string(target).unwrap(), "cross-device\n");
    }

    #[test]
    fn abandoned_patch_stage_is_removed_by_guard() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("destination.txt");
        let stage = StagedPatchFile::new(&destination, b"staged\n", None).unwrap();
        let stage_path = stage.path.clone();
        assert!(stage_path.is_file());
        drop(stage);
        assert!(!stage_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn app_server_paths_forward_the_validated_canonical_target() {
        use std::os::unix::fs::symlink;

        let workspace_dir = tempfile::tempdir().unwrap();
        fs::write(workspace_dir.path().join("target.txt"), "inside").unwrap();
        symlink("target.txt", workspace_dir.path().join("link.txt")).unwrap();
        let host = Host::open(workspace_dir.path()).unwrap();

        let resolved = host.resolve_app_server_existing("link.txt").unwrap();
        assert_eq!(
            std::path::Path::new(&resolved),
            workspace_dir
                .path()
                .join("target.txt")
                .canonicalize()
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn app_server_paths_reject_non_utf8_canonical_targets() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let workspace_dir = tempfile::tempdir().unwrap();
        let invalid_name = OsString::from_vec(b"invalid-\xff.txt".to_vec());
        let target = workspace_dir.path().join(invalid_name);
        fs::write(&target, "inside").unwrap();
        symlink(&target, workspace_dir.path().join("alias.txt")).unwrap();
        let host = Host::open(workspace_dir.path()).unwrap();

        assert!(matches!(
            host.resolve_app_server_existing("alias.txt"),
            Err(HostError::NonUtf8Path)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn app_server_mutations_reject_symlink_escape_paths() {
        use std::os::unix::fs::symlink;

        let workspace_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        symlink(outside_dir.path(), workspace_dir.path().join("escape")).unwrap();
        let host = Host::open(workspace_dir.path()).unwrap();

        assert!(matches!(
            host.resolve_mutation_path("escape/new.txt"),
            Err(HostError::SymlinkMutation(_))
        ));
        assert!(matches!(
            host.resolve_mutation_path("escape/../new.txt"),
            Err(HostError::SymlinkMutation(_))
        ));
    }

    #[test]
    fn search_omits_git_data() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("visible.txt"), "needle").unwrap();
        fs::create_dir(temporary.path().join(".git")).unwrap();
        fs::write(temporary.path().join(".git/hidden"), "needle").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let matches = host.search("needle", None, None).unwrap();
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].path, "visible.txt");
    }

    #[test]
    fn single_file_search_keeps_a_usable_result_path() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("needle.txt"), "find me\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let matches = host.search("find me", Some("needle.txt"), None).unwrap();
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].path, "needle.txt");
    }

    #[test]
    fn subdirectory_search_results_stay_relative_to_request_cwd() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("src")).unwrap();
        fs::write(temporary.path().join("src/lib.rs"), "find me\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let matches = host.search("find me", Some("src"), None).unwrap();
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].path, "src/lib.rs");
    }

    #[test]
    fn search_skips_build_output_directories() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("visible.txt"), "needle").unwrap();
        fs::create_dir(temporary.path().join("target")).unwrap();
        fs::write(temporary.path().join("target/hidden"), "needle").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        assert_eq!(host.search("needle", None, None).unwrap().matches.len(), 1);
    }

    #[test]
    fn content_search_honors_cancellation() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("visible.txt"), "needle").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        assert!(matches!(
            host.search_with_cancel("needle", None, None, None, || true),
            Err(HostError::Cancelled)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn search_does_not_follow_symbolic_links() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        fs::write(external.path(), "needle").unwrap();
        symlink(external.path(), temporary.path().join("linked.txt")).unwrap();
        let host = Host::open(temporary.path()).unwrap();
        assert!(
            host.search("needle", None, None)
                .unwrap()
                .matches
                .is_empty()
        );
    }

    #[test]
    fn encodes_a_supported_image() {
        let temporary = tempfile::tempdir().unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==")
            .unwrap();
        fs::write(temporary.path().join("image.png"), &png).unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let image = host.image("image.png").unwrap();
        assert_eq!(image.path, "image.png");
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(
            image.base64_data,
            base64::engine::general_purpose::STANDARD.encode(png)
        );
    }

    #[test]
    fn image_detail_matches_codex_prompt_limits() {
        use image::GenericImageView;
        use image::ImageFormat;
        use std::io::Cursor;

        let temporary = tempfile::tempdir().unwrap();
        let source = image::DynamicImage::new_rgba8(2048, 2048);
        let mut encoded = Cursor::new(Vec::new());
        source.write_to(&mut encoded, ImageFormat::Png).unwrap();
        fs::write(temporary.path().join("large.png"), encoded.into_inner()).unwrap();
        let host = Host::open(temporary.path()).unwrap();

        let high = host.image_with_detail("large.png", Some("high")).unwrap();
        let high_bytes = base64::engine::general_purpose::STANDARD
            .decode(high.base64_data)
            .unwrap();
        assert_eq!(
            image::load_from_memory(&high_bytes).unwrap().dimensions(),
            (1600, 1600)
        );

        let original = host
            .image_with_detail("large.png", Some("original"))
            .unwrap();
        let original_bytes = base64::engine::general_purpose::STANDARD
            .decode(original.base64_data)
            .unwrap();
        assert_eq!(
            image::load_from_memory(&original_bytes)
                .unwrap()
                .dimensions(),
            (2048, 2048)
        );
    }

    #[test]
    fn explains_the_expected_patch_format() {
        let temporary = tempfile::tempdir().unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let error = host
            .apply_patch("--- a/file\n+++ b/file\n", None)
            .unwrap_err();
        assert!(error.to_string().contains("official"));
    }

    #[test]
    fn patch_tolerates_outer_whitespace_and_marker_padding() {
        let temporary = tempfile::tempdir().unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let changed = host
            .apply_patch(
                "\n\n  *** Begin Patch  \n*** Add File: a.txt\n+a\n  *** End Patch  \n\n",
                None,
            )
            .unwrap();
        assert_eq!(changed, vec!["a.txt"]);
        assert_eq!(
            fs::read_to_string(temporary.path().join("a.txt")).unwrap(),
            "a\n"
        );
    }

    #[test]
    fn patch_rejects_blank_separator_in_added_file() {
        let temporary = tempfile::tempdir().unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let error = host
            .apply_patch(
                "*** Begin Patch\n*** Add File: a.txt\n+a\n\n*** End Patch\n",
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("must start with `+`"));
    }

    #[test]
    fn patch_rejects_blank_separator_after_deleted_file() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("a.txt"), "a\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let error = host
            .apply_patch(
                "*** Begin Patch\n*** Delete File: a.txt\n\n*** Add File: b.txt\n+b\n*** End Patch\n",
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("unknown patch directive"));
    }

    #[test]
    fn patch_tolerates_blank_lines_before_end_patch_in_update_hunk() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file.txt"), "before\nafter\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let changed = host
            .apply_patch(
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+changed\n\n\n*** End Patch\n",
                None,
            )
            .unwrap();
        assert_eq!(changed, vec!["file.txt"]);
        assert_eq!(
            fs::read_to_string(temporary.path().join("file.txt")).unwrap(),
            "changed\nafter\n"
        );
    }

    #[test]
    fn patch_rejects_unprefixed_blank_line_inside_added_content() {
        let temporary = tempfile::tempdir().unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let error = host
            .apply_patch(
                "*** Begin Patch\n*** Add File: a.txt\n+a\n\n+b\n*** End Patch\n",
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("must start with `+`"));
    }

    #[test]
    fn patch_tolerates_blank_lines_before_end_of_file_marker() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file.txt"), "before\nafter\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        host
            .apply_patch(
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-after\n+changed\n\n\n*** End of File\n*** End Patch\n",
                None,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("file.txt")).unwrap(),
            "before\nchanged\n"
        );
    }

    #[test]
    fn patch_tolerates_blank_lines_after_end_of_file_marker() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file.txt"), "before\nafter\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let changed = host
            .apply_patch(
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-after\n+changed\n*** End of File\n\n\n*** Add File: added.txt\n+added\n*** End Patch\n",
                None,
            )
            .unwrap();
        assert_eq!(changed, vec!["file.txt", "added.txt"]);
        assert_eq!(
            fs::read_to_string(temporary.path().join("file.txt")).unwrap(),
            "before\nchanged\n"
        );
        assert_eq!(
            fs::read_to_string(temporary.path().join("added.txt")).unwrap(),
            "added\n"
        );
    }

    #[test]
    fn applies_the_official_patch_format() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file.txt"), "before\nafter\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let changed = host
            .apply_patch(
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+changed\n*** End Patch\n", None
            )
            .unwrap();
        assert_eq!(changed, vec!["file.txt"]);
        assert_eq!(
            fs::read_to_string(temporary.path().join("file.txt")).unwrap(),
            "changed\nafter\n"
        );
    }

    #[test]
    fn patch_paths_may_leave_default_cwd_and_new_parents_are_created() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let host = Host::open(&workspace).unwrap();
        host.apply_patch(
            "*** Begin Patch\n*** Add File: ../outside.txt\n+outside\n*** End Patch\n",
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("outside.txt")).unwrap(),
            "outside\n"
        );
        host.apply_patch(
            "*** Begin Patch\n*** Add File: nested/file.txt\n+inside\n*** End Patch\n",
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("nested/file.txt")).unwrap(),
            "inside\n"
        );
    }

    #[test]
    fn patch_rejects_parent_traversal_through_non_directory_or_missing_components() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("plain-file"), "not a directory\n").unwrap();
        fs::write(temporary.path().join("victim.txt"), "keep me\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();

        let through_file = host
            .apply_patch(
                "*** Begin Patch\n*** Delete File: plain-file/../victim.txt\n*** End Patch\n",
                None,
            )
            .unwrap_err();
        assert!(through_file.to_string().contains("non-directory component"));
        assert_eq!(
            fs::read_to_string(temporary.path().join("victim.txt")).unwrap(),
            "keep me\n"
        );

        let through_missing = host
            .apply_patch(
                "*** Begin Patch\n*** Delete File: missing/../victim.txt\n*** End Patch\n",
                None,
            )
            .unwrap_err();
        assert!(through_missing.to_string().contains("missing component"));
        assert_eq!(
            fs::read_to_string(temporary.path().join("victim.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn request_cwd_rebases_paths_without_creating_an_authorization_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("project")).unwrap();
        fs::write(temporary.path().join("project/local.txt"), "inside").unwrap();
        let host = Host::open(temporary.path()).unwrap();

        let cwd = host.resolve_cwd(Some("project")).unwrap();
        let local = Host::path_from_cwd(&cwd, "local.txt").unwrap();
        assert_eq!(
            std::path::Path::new(&host.resolve_app_server_existing(&local).unwrap()),
            temporary
                .path()
                .join("project/local.txt")
                .canonicalize()
                .unwrap()
        );

        host.apply_patch(
            "*** Begin Patch\n*** Add File: created.txt\n+created\n*** End Patch\n",
            Some("project"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("project/created.txt")).unwrap(),
            "created\n"
        );
        host.apply_patch(
            "*** Begin Patch\n*** Add File: ../outside.txt\n+outside\n*** End Patch\n",
            Some("project"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("outside.txt")).unwrap(),
            "outside\n"
        );
    }

    #[test]
    fn patch_preflight_prevents_partial_application() {
        let temporary = tempfile::tempdir().unwrap();
        let host = Host::open(temporary.path()).unwrap();
        let error = host
            .apply_patch(
                "*** Begin Patch\n*** Add File: first.txt\n+created\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch\n", None
            )
            .unwrap_err();
        assert!(error.to_string().contains("host operation failed"));
        assert!(!temporary.path().join("first.txt").exists());
    }

    #[test]
    fn patch_preserves_crlf_and_accepts_end_of_file_marker() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file.txt"), "before\r\nafter\r\n").unwrap();
        let host = Host::open(temporary.path()).unwrap();
        host
            .apply_patch(
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-after\n+changed\n*** End of File\n*** End Patch\n", None
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("file.txt")).unwrap(),
            "before\r\nchanged\r\n"
        );
    }

    #[test]
    fn patch_context_headers_anchor_repeated_content() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(
            temporary.path().join("sample.py"),
            "class First:\n    def run(self):\n        return \"same\"\n\nclass Target:\n    def run(self):\n        return \"same\"\n",
        )
        .unwrap();
        let host = Host::open(temporary.path()).unwrap();
        host.apply_patch(
            "*** Begin Patch\n*** Update File: sample.py\n@@ class Target:\n-        return \"same\"\n+        return \"changed\"\n*** End Patch\n",
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("sample.py")).unwrap(),
            "class First:\n    def run(self):\n        return \"same\"\n\nclass Target:\n    def run(self):\n        return \"changed\"\n"
        );
    }

    #[test]
    fn patch_hunks_search_forward_in_order() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(
            temporary.path().join("file.txt"),
            "same\nfirst\nsame\nsecond\n",
        )
        .unwrap();
        let host = Host::open(temporary.path()).unwrap();
        host.apply_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-same\n+one\n@@\n-same\n+two\n*** End Patch\n",
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("file.txt")).unwrap(),
            "one\nfirst\ntwo\nsecond\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_updates_and_moves_preserve_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("tool.sh");
        fs::write(&executable, "old\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let private = temporary.path().join("private.txt");
        fs::write(&private, "secret\n").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();

        let host = Host::open(temporary.path()).unwrap();
        host.apply_patch(
            "*** Begin Patch\n*** Update File: tool.sh\n@@\n-old\n+new\n*** Update File: private.txt\n*** Move to: moved.txt\n@@\n-secret\n+kept\n*** End Patch\n",
            None,
        )
        .unwrap();

        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(temporary.path().join("moved.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
