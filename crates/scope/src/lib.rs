//! Durable host-scope policy and Connect-only search, image, and patch operations.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::GenericImageView;
use image::ImageFormat;
use image::imageops::FilterType;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Cursor;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;

const MAX_SEARCH_RESULTS: usize = 100;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
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
pub enum ScopeError {
    #[error("path is outside the configured scope root")]
    OutsideRoot,
    #[error("scope path is not valid UTF-8 for App Server")]
    NonUtf8Path,
    #[error("scope operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("search query must not be empty")]
    EmptyQuery,
    #[error("scope search was cancelled")]
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
    #[error("scope mutations do not allow symbolic links: {0}")]
    SymlinkMutation(String),
    #[error("the configured scope root cannot be mutated")]
    RootMutation,
}

fn search_names_tree(
    root: &Path,
    path: &Path,
    query: &str,
    max_results: usize,
    paths: &mut Vec<String>,
) -> Result<(), ScopeError> {
    if paths.len() >= max_results {
        return Ok(());
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if path != root
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(query))
    {
        paths.push(
            path.strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string(),
        );
    }
    if metadata.is_dir() {
        if path != root && should_skip_directory(path) {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            search_names_tree(root, &entry?.path(), query, max_results, paths)?;
            if paths.len() >= max_results {
                break;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NameSearchResults {
    pub paths: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct Scope {
    root: PathBuf,
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

/// The single durable host trust boundary. Projects are selected per operation
/// by official Codex `cwd` fields or paths, never by mutable backend state.
impl Scope {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ScopeError> {
        let root = root.into().canonicalize()?;
        if !root.is_dir() {
            return Err(ScopeError::OutsideRoot);
        }
        Ok(Self { root })
    }

    pub fn search_names(
        &self,
        query: &str,
        requested: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<NameSearchResults, ScopeError> {
        if query.is_empty() {
            return Err(ScopeError::EmptyQuery);
        }
        let root = match requested {
            Some(path) => self.resolve_existing(path)?,
            None => self.root.clone(),
        };
        let limit = max_results.unwrap_or(MAX_SEARCH_RESULTS).clamp(1, 1_000);
        let mut paths = Vec::new();
        search_names_tree(&self.root, &root, query, limit + 1, &mut paths)?;
        let truncated = paths.len() > limit;
        paths.truncate(limit);
        Ok(NameSearchResults { paths, truncated })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a request-local cwd; omission selects the authorization root.
    pub fn resolve_cwd(&self, cwd: Option<&str>) -> Result<String, ScopeError> {
        self.resolve_app_server_directory(cwd.unwrap_or("."))
    }

    /// Join against an already resolved cwd. The consumer must still fence the path.
    pub fn path_from_cwd(cwd: &str, requested: &str) -> Result<String, ScopeError> {
        Path::new(cwd)
            .join(requested)
            .to_str()
            .map(str::to_owned)
            .ok_or(ScopeError::NonUtf8Path)
    }

    pub fn resolve_app_server_existing(&self, requested: &str) -> Result<String, ScopeError> {
        let candidate = self.resolve_rooted_path(requested, true)?;
        let canonical = candidate.canonicalize()?;
        if !canonical.starts_with(&self.root) {
            return Err(ScopeError::OutsideRoot);
        }
        canonical
            .to_str()
            .map(str::to_owned)
            .ok_or(ScopeError::NonUtf8Path)
    }

    pub fn resolve_app_server_directory(&self, requested: &str) -> Result<String, ScopeError> {
        let candidate = self.resolve_app_server_existing(requested)?;
        if !Path::new(&candidate).is_dir() {
            return Err(ScopeError::PatchFailed(format!(
                "scope path is not a directory: {requested}"
            )));
        }
        Ok(candidate)
    }

    pub fn search_with_cancel(
        &self,
        query: &str,
        requested: Option<&str>,
        max_results: Option<usize>,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<SearchResults, ScopeError> {
        if query.is_empty() {
            return Err(ScopeError::EmptyQuery);
        }
        let root = match requested {
            Some(path) => self.resolve_existing(path)?,
            None => self.root.clone(),
        };
        let max_results = max_results.unwrap_or(MAX_SEARCH_RESULTS).clamp(1, 1_000);
        let mut matches = Vec::new();
        search_tree(
            &self.root,
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
    ) -> Result<SearchResults, ScopeError> {
        self.search_with_cancel(query, requested, max_results, || false)
    }

    pub fn image_with_detail(
        &self,
        requested: &str,
        detail: Option<&str>,
    ) -> Result<ImageFile, ScopeError> {
        let detail = match detail {
            None | Some("high") => "high",
            Some("original") => "original",
            Some(_) => return Err(ScopeError::InvalidImageDetail),
        };
        let path = self.resolve_existing(requested)?;
        let mut file = fs::File::open(&path)?;
        let mut bytes = Vec::with_capacity(MAX_IMAGE_BYTES.saturating_add(1));
        file.by_ref()
            .take(MAX_IMAGE_BYTES.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ScopeError::LargeImage);
        }
        let format = image::guess_format(&bytes)
            .map_err(|_| ScopeError::InvalidImage(self.relative(&path)))?;
        let mime_type = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::WebP => "image/webp",
            _ => return Err(ScopeError::UnsupportedImage(self.relative(&path))),
        };
        let image = image::load_from_memory_with_format(&bytes, format)
            .map_err(|_| ScopeError::InvalidImage(self.relative(&path)))?;
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
                    .map_err(|_| ScopeError::InvalidImage(self.relative(&path)))?;
                let encoded = encoded.into_inner();
                if encoded.len() > MAX_IMAGE_BYTES {
                    return Err(ScopeError::LargeImage);
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
    fn image(&self, requested: &str) -> Result<ImageFile, ScopeError> {
        self.image_with_detail(requested, None)
    }

    pub fn apply_patch(&self, patch: &str, cwd: Option<&str>) -> Result<Vec<String>, ScopeError> {
        let document = parse_patch(patch)?;
        if document.environment_id.is_some() {
            return Err(ScopeError::PatchFailed(
                "apply_patch environment selection is unavailable for this scope".to_string(),
            ));
        }
        let cwd = self.resolve_cwd(cwd)?;
        let plan = self.plan_patch(document.changes, &cwd)?;
        self.apply_patch_plan(plan)
    }

    fn plan_patch(&self, changes: Vec<PatchChange>, cwd: &str) -> Result<PatchPlan, ScopeError> {
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
                    actions.push(PatchAction::Write { path, content });
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
                    let original = fs::read(&source)?;
                    let original = std::str::from_utf8(&original).map_err(|_| {
                        ScopeError::PatchFailed(format!(
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
                    });
                    if destination != source {
                        actions.push(PatchAction::Delete { path: source });
                    }
                }
            }
        }
        Ok(PatchPlan { actions, applied })
    }

    fn apply_patch_plan(&self, plan: PatchPlan) -> Result<Vec<String>, ScopeError> {
        let transaction = tempfile::Builder::new()
            .prefix(".codex-connect-patch-")
            .tempdir_in(&self.root)?;
        let mut staged = Vec::with_capacity(plan.actions.len());
        for (index, action) in plan.actions.iter().enumerate() {
            let stage = match action {
                PatchAction::Write { content, .. } => {
                    let stage = transaction.path().join(format!("write-{index}"));
                    fs::write(&stage, content)?;
                    Some(stage)
                }
                PatchAction::Delete { .. } => None,
            };
            staged.push(stage);
        }

        let mut completed = Vec::with_capacity(plan.actions.len());
        let mut created_directories = Vec::new();
        let result = (|| -> Result<(), ScopeError> {
            for (index, action) in plan.actions.iter().enumerate() {
                let path = action.path();
                match action {
                    PatchAction::Write { .. } => {
                        created_directories
                            .extend(create_missing_parent_directories(&self.root, path)?);
                        let backup = if path.exists() {
                            let backup = transaction.path().join(format!("backup-{index}"));
                            fs::rename(path, &backup)?;
                            Some(backup)
                        } else {
                            None
                        };
                        let stage = staged[index].as_ref().expect("write actions are staged");
                        fs::rename(stage, path)?;
                        completed.push(CompletedPatchAction::Write {
                            path: path.to_path_buf(),
                            backup,
                        });
                    }
                    PatchAction::Delete { .. } => {
                        let backup = transaction.path().join(format!("backup-{index}"));
                        fs::rename(path, &backup)?;
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
        Ok(plan.applied)
    }

    fn resolve_existing(&self, requested: &str) -> Result<PathBuf, ScopeError> {
        let requested = Path::new(requested);
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };
        let candidate = candidate.canonicalize()?;
        if candidate.starts_with(&self.root) {
            Ok(candidate)
        } else {
            Err(ScopeError::OutsideRoot)
        }
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string()
    }

    fn resolve_rooted_path(
        &self,
        requested: &str,
        allow_scope_root: bool,
    ) -> Result<PathBuf, ScopeError> {
        let requested = Path::new(requested);
        let relative = if requested.is_absolute() {
            requested
                .strip_prefix(&self.root)
                .map_err(|_| ScopeError::OutsideRoot)?
        } else {
            requested
        };
        let mut candidate = self.root.clone();
        for component in relative.components() {
            match component {
                std::path::Component::Normal(component) => candidate.push(component),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir
                | std::path::Component::Prefix(_)
                | std::path::Component::RootDir => return Err(ScopeError::OutsideRoot),
            }
        }
        if !allow_scope_root && candidate == self.root {
            return Err(ScopeError::RootMutation);
        }
        Ok(candidate)
    }

    fn resolve_mutation_path(&self, requested: &str) -> Result<PathBuf, ScopeError> {
        let candidate = self.resolve_rooted_path(requested, false)?;
        let relative = candidate
            .strip_prefix(&self.root)
            .map_err(|_| ScopeError::OutsideRoot)?;
        let mut current = self.root.clone();
        for component in relative.components() {
            let std::path::Component::Normal(component) = component else {
                return Err(ScopeError::OutsideRoot);
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(ScopeError::SymlinkMutation(self.relative(&current)));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(candidate)
    }
}

fn search_tree(
    root: &Path,
    path: &Path,
    query: &str,
    max_results: usize,
    matches: &mut Vec<SearchMatch>,
    is_cancelled: &impl Fn() -> bool,
) -> Result<(), ScopeError> {
    if is_cancelled() {
        return Err(ScopeError::Cancelled);
    }
    if matches.len() >= max_results {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        if path != root && should_skip_directory(path) {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            search_tree(
                root,
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
            return Err(ScopeError::Cancelled);
        }
        let line = match line {
            Ok(line) => line,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if line.contains(query) {
            matches.push(SearchMatch {
                path: path
                    .strip_prefix(root)
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
    line_hint: Option<usize>,
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
    Write { path: PathBuf, content: Vec<u8> },
    Delete { path: PathBuf },
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

fn parse_patch(patch: &str) -> Result<PatchDocument, ScopeError> {
    let lines = patch.lines().collect::<Vec<_>>();
    if lines.first().copied() != Some("*** Begin Patch")
        || lines.last().copied() != Some("*** End Patch")
    {
        return Err(ScopeError::PatchFailed(
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
            return Err(ScopeError::PatchFailed(
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
                    ScopeError::PatchFailed("added file lines must start with `+`".to_string())
                })?;
                content.push_str(line);
                content.push('\n');
                count += 1;
                index += 1;
            }
            if count == 0 {
                return Err(ScopeError::PatchFailed(
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
                if !header.starts_with("@@") {
                    return Err(ScopeError::PatchFailed(
                        "updated files require `@@` hunk headers".to_string(),
                    ));
                }
                let line_hint = parse_line_hint(header);
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
                            return Err(ScopeError::PatchFailed(
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
                    return Err(ScopeError::PatchFailed("patch hunk is empty".to_string()));
                }
                hunks.push(PatchHunk {
                    lines: hunk_lines,
                    line_hint,
                    end_of_file,
                });
                if end_of_file {
                    break;
                }
            }
            if hunks.is_empty() && move_path.is_none() {
                return Err(ScopeError::PatchFailed(
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
        return Err(ScopeError::PatchFailed(format!(
            "unknown patch directive: {header}"
        )));
    }
    if changes.is_empty() {
        return Err(ScopeError::PatchFailed(
            "patch contains no file changes".to_string(),
        ));
    }
    Ok(PatchDocument {
        environment_id,
        changes,
    })
}

fn require_patch_path(path: &str) -> Result<(), ScopeError> {
    if path.is_empty() {
        return Err(ScopeError::PatchFailed(
            "patch file paths must not be empty".to_string(),
        ));
    }
    Ok(())
}

fn parse_line_hint(header: &str) -> Option<usize> {
    header
        .split_whitespace()
        .find_map(|part| part.strip_prefix('+'))
        .and_then(|part| part.split(',').next())
        .and_then(|part| part.parse::<usize>().ok())
        .map(|line| line.saturating_sub(1))
}

fn apply_hunks(original: &str, hunks: &[PatchHunk]) -> Result<Vec<u8>, ScopeError> {
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
    let mut offset = 0_i64;
    for hunk in hunks {
        let old = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(value) | PatchLine::Remove(value) => Some(value),
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
        let base = hunk.line_hint.unwrap_or(current.len());
        let hinted = (base as i64 + offset).max(0) as usize;
        let position = if old.is_empty() {
            hinted.min(current.len())
        } else {
            find_lines(&current, &old, hinted).ok_or_else(|| {
                ScopeError::PatchFailed("patch context did not match the target file".to_string())
            })?
        };
        let removed = old.len();
        current.splice(position..position + removed, replacement.clone());
        offset += replacement.len() as i64 - removed as i64;
    }
    let mut output = current.join(newline);
    if !hunks.iter().any(|hunk| hunk.end_of_file) && (trailing_newline || !output.is_empty()) {
        output.push_str(newline);
    }
    Ok(output.into_bytes())
}

fn find_lines(lines: &[String], pattern: &[&String], hinted: usize) -> Option<usize> {
    let matches_at = |start: usize| {
        start + pattern.len() <= lines.len()
            && lines[start..start + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(actual, expected)| actual == *expected)
    };
    if matches_at(hinted) {
        return Some(hinted);
    }
    (0..=lines.len().saturating_sub(pattern.len())).find(|start| matches_at(*start))
}

fn ensure_regular_file(path: &Path) -> Result<(), ScopeError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(ScopeError::SymlinkMutation(path.display().to_string()));
    }
    if !metadata.is_file() {
        return Err(ScopeError::PatchFailed(format!(
            "expected a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_patch_destination(path: &Path) -> Result<(), ScopeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ScopeError::SymlinkMutation(path.display().to_string()))
        }
        Ok(metadata) if !metadata.is_file() => Err(ScopeError::PatchFailed(format!(
            "patch destination is not a regular file: {}",
            path.display()
        ))),
        Ok(_) | Err(_) => Ok(()),
    }
}

fn register_patch_path(touched: &mut HashSet<PathBuf>, path: &Path) -> Result<(), ScopeError> {
    if touched.insert(path.to_path_buf()) {
        Ok(())
    } else {
        Err(ScopeError::PatchFailed(format!(
            "patch changes the same path more than once: {}",
            path.display()
        )))
    }
}

fn create_missing_parent_directories(root: &Path, path: &Path) -> Result<Vec<PathBuf>, ScopeError> {
    let mut missing = Vec::new();
    let mut current = path.parent().ok_or(ScopeError::OutsideRoot)?;
    while current != root && !current.exists() {
        missing.push(current.to_path_buf());
        current = current.parent().ok_or(ScopeError::OutsideRoot)?;
    }
    if !current.is_dir() {
        return Err(ScopeError::PatchFailed(format!(
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
    use super::Scope;
    use super::ScopeError;
    use base64::Engine;
    use std::fs;

    #[test]
    fn app_server_paths_are_rooted_to_the_configured_scope() {
        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        fs::write(scope_dir.path().join("inside.txt"), "inside").unwrap();
        let scope = Scope::open(scope_dir.path()).unwrap();

        let resolved = scope.resolve_app_server_existing("inside.txt").unwrap();
        assert_eq!(
            std::path::Path::new(&resolved).canonicalize().unwrap(),
            scope_dir.path().join("inside.txt").canonicalize().unwrap()
        );
        assert!(matches!(
            scope.resolve_app_server_existing(outside_dir.path().to_str().unwrap()),
            Err(ScopeError::OutsideRoot)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn app_server_paths_forward_the_validated_canonical_target() {
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        fs::write(scope_dir.path().join("target.txt"), "inside").unwrap();
        symlink("target.txt", scope_dir.path().join("link.txt")).unwrap();
        let scope = Scope::open(scope_dir.path()).unwrap();

        let resolved = scope.resolve_app_server_existing("link.txt").unwrap();
        assert_eq!(
            std::path::Path::new(&resolved),
            scope_dir.path().join("target.txt").canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn app_server_paths_reject_non_utf8_canonical_targets() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        let invalid_name = OsString::from_vec(b"invalid-\xff.txt".to_vec());
        let target = scope_dir.path().join(invalid_name);
        fs::write(&target, "inside").unwrap();
        symlink(&target, scope_dir.path().join("alias.txt")).unwrap();
        let scope = Scope::open(scope_dir.path()).unwrap();

        assert!(matches!(
            scope.resolve_app_server_existing("alias.txt"),
            Err(ScopeError::NonUtf8Path)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn app_server_mutations_reject_symlink_escape_paths() {
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        symlink(outside_dir.path(), scope_dir.path().join("escape")).unwrap();
        let scope = Scope::open(scope_dir.path()).unwrap();

        assert!(matches!(
            scope.resolve_mutation_path("escape/new.txt"),
            Err(ScopeError::SymlinkMutation(_))
        ));
    }

    #[test]
    fn search_omits_git_data() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("visible.txt"), "needle").unwrap();
        fs::create_dir(temporary.path().join(".git")).unwrap();
        fs::write(temporary.path().join(".git/hidden"), "needle").unwrap();
        let scope = Scope::open(temporary.path()).unwrap();
        let matches = scope.search("needle", None, None).unwrap();
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].path, "visible.txt");
    }

    #[test]
    fn search_skips_build_output_directories() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("visible.txt"), "needle").unwrap();
        fs::create_dir(temporary.path().join("target")).unwrap();
        fs::write(temporary.path().join("target/hidden"), "needle").unwrap();
        let scope = Scope::open(temporary.path()).unwrap();
        assert_eq!(scope.search("needle", None, None).unwrap().matches.len(), 1);
    }

    #[test]
    fn content_search_honors_cancellation() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("visible.txt"), "needle").unwrap();
        let scope = Scope::open(temporary.path()).unwrap();
        assert!(matches!(
            scope.search_with_cancel("needle", None, None, || true),
            Err(ScopeError::Cancelled)
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
        let scope = Scope::open(temporary.path()).unwrap();
        assert!(
            scope
                .search("needle", None, None)
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
        let scope = Scope::open(temporary.path()).unwrap();
        let image = scope.image("image.png").unwrap();
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
        let scope = Scope::open(temporary.path()).unwrap();

        let high = scope.image_with_detail("large.png", Some("high")).unwrap();
        let high_bytes = base64::engine::general_purpose::STANDARD
            .decode(high.base64_data)
            .unwrap();
        assert_eq!(
            image::load_from_memory(&high_bytes).unwrap().dimensions(),
            (1600, 1600)
        );

        let original = scope
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
        let scope = Scope::open(temporary.path()).unwrap();
        let error = scope
            .apply_patch("--- a/file\n+++ b/file\n", None)
            .unwrap_err();
        assert!(error.to_string().contains("official"));
    }

    #[test]
    fn applies_the_official_patch_format() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file.txt"), "before\nafter\n").unwrap();
        let scope = Scope::open(temporary.path()).unwrap();
        let changed = scope
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
    fn patch_paths_cannot_escape_and_new_parents_are_created_inside_root() {
        let temporary = tempfile::tempdir().unwrap();
        let scope = Scope::open(temporary.path()).unwrap();
        let error = scope
            .apply_patch(
                "*** Begin Patch\n*** Add File: ../outside.txt\n+nope\n*** End Patch\n",
                None,
            )
            .unwrap_err();
        assert!(matches!(error, ScopeError::OutsideRoot));
        scope
            .apply_patch(
                "*** Begin Patch\n*** Add File: nested/file.txt\n+inside\n*** End Patch\n",
                None,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("nested/file.txt")).unwrap(),
            "inside\n"
        );
    }

    #[test]
    fn request_cwd_rebases_paths_without_changing_the_scope_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("project")).unwrap();
        fs::write(temporary.path().join("project/local.txt"), "inside").unwrap();
        let scope = Scope::open(temporary.path()).unwrap();

        let cwd = scope.resolve_cwd(Some("project")).unwrap();
        let local = Scope::path_from_cwd(&cwd, "local.txt").unwrap();
        assert_eq!(
            std::path::Path::new(&scope.resolve_app_server_existing(&local).unwrap()),
            temporary
                .path()
                .join("project/local.txt")
                .canonicalize()
                .unwrap()
        );

        scope
            .apply_patch(
                "*** Begin Patch\n*** Add File: created.txt\n+created\n*** End Patch\n",
                Some("project"),
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("project/created.txt")).unwrap(),
            "created\n"
        );
        assert!(matches!(
            scope.apply_patch(
                "*** Begin Patch\n*** Add File: ../outside.txt\n+nope\n*** End Patch\n",
                Some("project")
            ),
            Err(ScopeError::OutsideRoot)
        ));
    }

    #[test]
    fn patch_preflight_prevents_partial_application() {
        let temporary = tempfile::tempdir().unwrap();
        let scope = Scope::open(temporary.path()).unwrap();
        let error = scope
            .apply_patch(
                "*** Begin Patch\n*** Add File: first.txt\n+created\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch\n", None
            )
            .unwrap_err();
        assert!(error.to_string().contains("scope operation failed"));
        assert!(!temporary.path().join("first.txt").exists());
    }

    #[test]
    fn patch_preserves_crlf_and_accepts_end_of_file_marker() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file.txt"), "before\r\nafter\r\n").unwrap();
        let scope = Scope::open(temporary.path()).unwrap();
        scope
            .apply_patch(
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+changed\n*** End of File\n*** End Patch\n", None
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("file.txt")).unwrap(),
            "changed\r\nafter"
        );
    }

    #[test]
    fn name_search_remains_scope_fenced_and_skips_build_directories() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("src")).unwrap();
        fs::create_dir_all(temporary.path().join("target/generated")).unwrap();
        fs::write(temporary.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            temporary.path().join("src/main_test.rs"),
            "fn main_test() {}\n",
        )
        .unwrap();
        fs::write(
            temporary.path().join("target/generated/main.rs"),
            "ignored\n",
        )
        .unwrap();
        let scope = Scope::open(temporary.path()).unwrap();

        let names = scope.search_names("main.rs", None, None).unwrap();
        assert_eq!(names.paths, vec!["src/main.rs"]);
        assert_eq!(
            scope.search_names("src", None, None).unwrap().paths,
            vec!["src"]
        );
        let limited = scope.search_names("main", None, Some(1)).unwrap();
        assert_eq!(limited.paths.len(), 1);
        assert!(limited.truncated);
        assert!(scope.resolve_app_server_existing("../outside").is_err());
    }
}
