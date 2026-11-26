//! Isolated settings, read-only artifacts and restricted import staging.
//!
//! Settings reuse the schema 2 store and its atomic replacement, but first
//! verify that the existing entry is a plain file object and never a link.
//! Artifacts resolve only `<state>/projects/<uuid>/product`, accept plain
//! ASCII segments from the existing naming rule, and re-check the opened
//! object on Windows so a reparse point, a link or a containment violation
//! can never be served. Imports accept one `imports/` basename, re-check the
//! opened source handle, and copy it to a private per-request staging
//! directory whose deletion is owned by a receipt-keyed registry. Nothing
//! outside the isolated state directory is read or created.

use crate::connection_settings::{
    ConnectionSettings, ConnectionSettingsError, ConnectionSettingsStore,
};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex, MutexGuard},
    thread::JoinHandle,
};
use uuid::Uuid;

/// Media type of every artifact response.
pub const ARTIFACT_CONTENT_TYPE: &str = "application/octet-stream";
/// Fixed attachment disposition of every artifact response.
pub const ARTIFACT_CONTENT_DISPOSITION: &str = "attachment";
/// Largest artifact a single response may carry.
pub const ARTIFACT_LIMIT: usize = 32 * 1024;

const MAX_RELATIVE_BYTES: usize = 120;
const MAX_SEGMENTS: usize = 8;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;

#[derive(Debug, PartialEq, Eq)]
pub enum ApiFileError {
    /// The request content is not a valid schema 2 settings value.
    InvalidInput,
    /// The stored object is corrupt or is not a plain file object.
    InvalidStore,
    /// The isolated state is unavailable for I/O.
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ArtifactError {
    /// Every illegal, linked, oversized or missing artifact is the same 404.
    NotFound,
}

/// Bounded artifact bytes plus the fixed response metadata.
pub struct ArtifactBytes {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub content_disposition: &'static str,
}

/// Read the isolated schema 2 settings file without activating anything.
///
/// The returned value is exactly what is stored, including synthetic test
/// secrets; no historical, v1 or vault configuration is consulted.
pub fn load_isolated_settings(
    store: &ConnectionSettingsStore,
) -> Result<Option<ConnectionSettings>, ApiFileError> {
    verify_settings_entry(store, false)?;
    match store.load() {
        Ok(settings) => Ok(settings),
        Err(ConnectionSettingsError::InvalidInput | ConnectionSettingsError::InvalidFile) => {
            Err(ApiFileError::InvalidStore)
        }
        Err(ConnectionSettingsError::Io) => Err(ApiFileError::Unavailable),
    }
}

/// Atomically store schema 2 settings in the isolated state directory.
///
/// This never loads credentials, aliases the GUI profile or contacts an
/// external service, and it refuses to write through a link or reparse point.
pub fn save_isolated_settings(
    store: &ConnectionSettingsStore,
    settings: ConnectionSettings,
) -> Result<(), ApiFileError> {
    settings
        .validate()
        .map_err(|_| ApiFileError::InvalidInput)?;
    verify_settings_entry(store, true)?;
    store.save(&settings).map_err(|error| match error {
        ConnectionSettingsError::InvalidInput => ApiFileError::InvalidInput,
        ConnectionSettingsError::InvalidFile => ApiFileError::InvalidStore,
        ConnectionSettingsError::Io => ApiFileError::Unavailable,
    })
}

/// Read one artifact below `<state>/projects/<uuid>/product`.
///
/// `project_id` must be the canonical hyphenated UUID spelling; `relative`
/// uses only the existing artifact naming rule. All failures collapse to
/// [`ArtifactError::NotFound`] and leave the filesystem unchanged.
pub fn read_artifact(
    state_dir: &Path,
    project_id: &str,
    relative: &str,
) -> Result<ArtifactBytes, ArtifactError> {
    let project = canonical_project_id(project_id)?;
    let segments = artifact_segments(relative)?;
    let metadata = std::fs::symlink_metadata(state_dir).map_err(|_| ArtifactError::NotFound)?;
    if !is_plain_directory(&metadata) {
        return Err(ArtifactError::NotFound);
    }
    let state_dir = std::fs::canonicalize(state_dir).map_err(|_| ArtifactError::NotFound)?;
    if !state_dir.is_absolute() {
        return Err(ArtifactError::NotFound);
    }

    let mut current = state_dir.clone();
    for component in ["projects", project.as_str(), "product"] {
        current.push(component);
        verify_directory(&current)?;
    }
    for segment in &segments[..segments.len() - 1] {
        current.push(segment);
        verify_directory(&current)?;
    }
    current.push(segments[segments.len() - 1]);
    read_checked_file(&current, &current)
}

fn verify_settings_entry(
    store: &ConnectionSettingsStore,
    require_directory: bool,
) -> Result<(), ApiFileError> {
    let path = store.path();
    if let Some(directory) = path.parent() {
        match std::fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if !is_plain_directory(&metadata) {
                    return Err(ApiFileError::InvalidStore);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if require_directory {
                    return Err(ApiFileError::Unavailable);
                }
            }
            Err(_) => return Err(ApiFileError::Unavailable),
        }
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !is_plain_file(&metadata) {
                return Err(ApiFileError::InvalidStore);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(ApiFileError::Unavailable),
    }
    Ok(())
}

fn is_plain_directory(metadata: &std::fs::Metadata) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink() && !is_reparse(metadata)
}

fn is_plain_file(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink() && !is_reparse(metadata)
}

#[cfg(windows)]
fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0
}

#[cfg(not(windows))]
fn is_reparse(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn canonical_project_id(project_id: &str) -> Result<String, ArtifactError> {
    let project = Uuid::parse_str(project_id).map_err(|_| ArtifactError::NotFound)?;
    let canonical = project.hyphenated().to_string();
    if canonical != project_id {
        return Err(ArtifactError::NotFound);
    }
    Ok(canonical)
}

fn artifact_segments(relative: &str) -> Result<Vec<&str>, ArtifactError> {
    if relative.is_empty() || relative.len() > MAX_RELATIVE_BYTES || !relative.is_ascii() {
        return Err(ArtifactError::NotFound);
    }
    let segments: Vec<&str> = relative.split('/').collect();
    if segments.len() > MAX_SEGMENTS {
        return Err(ArtifactError::NotFound);
    }
    for segment in &segments {
        if segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || segment.ends_with('.')
            || segment.ends_with(' ')
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || is_device_name(segment)
        {
            return Err(ArtifactError::NotFound);
        }
    }
    Ok(segments)
}

fn is_device_name(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9')
        }
    }
}

#[cfg(windows)]
fn verify_directory(path: &Path) -> Result<(), ArtifactError> {
    open_object(path, true).map(|_| ())
}

#[cfg(not(windows))]
fn verify_directory(path: &Path) -> Result<(), ArtifactError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ArtifactError::NotFound)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ArtifactError::NotFound);
    }
    Ok(())
}

#[cfg(windows)]
fn read_checked_file(path: &Path, expected: &Path) -> Result<ArtifactBytes, ArtifactError> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_ATTRIBUTE_DIRECTORY, FILE_NAME_NORMALIZED,
        VOLUME_NAME_DOS,
    };
    use std::os::windows::io::AsRawHandle;

    let (file, info) = open_object(path, false)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || info.nNumberOfLinks != 1 {
        return Err(ArtifactError::NotFound);
    }
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    if size > ARTIFACT_LIMIT as u64 {
        return Err(ArtifactError::NotFound);
    }

    let mut buffer = vec![0u16; 32_768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(ArtifactError::NotFound);
    }
    let actual = String::from_utf16(&buffer[..length]).map_err(|_| ArtifactError::NotFound)?;
    if !same_path(&actual, expected) {
        return Err(ArtifactError::NotFound);
    }

    let mut bytes = Vec::with_capacity(size as usize);
    file.take(ARTIFACT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ArtifactError::NotFound)?;
    if bytes.len() > ARTIFACT_LIMIT {
        return Err(ArtifactError::NotFound);
    }
    Ok(ArtifactBytes {
        bytes,
        content_type: ARTIFACT_CONTENT_TYPE,
        content_disposition: ARTIFACT_CONTENT_DISPOSITION,
    })
}

#[cfg(not(windows))]
fn read_checked_file(path: &Path, expected: &Path) -> Result<ArtifactBytes, ArtifactError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ArtifactError::NotFound)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ArtifactError::NotFound);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(ArtifactError::NotFound);
        }
    }
    if metadata.len() > ARTIFACT_LIMIT as u64 {
        return Err(ArtifactError::NotFound);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| ArtifactError::NotFound)?;
    if canonical != expected {
        return Err(ArtifactError::NotFound);
    }
    let mut file = std::fs::File::open(path).map_err(|_| ArtifactError::NotFound)?;
    let mut bytes = Vec::new();
    file.take(ARTIFACT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ArtifactError::NotFound)?;
    if bytes.len() > ARTIFACT_LIMIT {
        return Err(ArtifactError::NotFound);
    }
    Ok(ArtifactBytes {
        bytes,
        content_type: ARTIFACT_CONTENT_TYPE,
        content_disposition: ARTIFACT_CONTENT_DISPOSITION,
    })
}

#[cfg(windows)]
fn open_object(
    path: &Path,
    directory: bool,
) -> Result<(std::fs::File, windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION), ArtifactError>
{
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CreateFileW,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(ArtifactError::NotFound);
    }
    let file = unsafe { std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(ArtifactError::NotFound);
    }
    Ok((file, info))
}

fn same_path(actual: &str, expected: &Path) -> bool {
    let normalise = |value: &str| {
        value
            .replace('/', "\\")
            .trim_start_matches(r"\\?\")
            .to_lowercase()
    };
    normalise(actual) == normalise(&expected.to_string_lossy())
}

// ---------------------------------------------------------------------------
// Restricted import staging
// ---------------------------------------------------------------------------

/// Largest document a single import may stage.
pub const IMPORT_LIMIT: usize = 256 * 1024;
/// Private, per-request staging lives below this directory of `imports/`.
const STAGING_DIR: &str = ".staging";

#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    /// Invalid name, missing or non-conforming source, or disallowed content.
    /// Every case is the fixed HTTP 422 `invalid_import` before acceptance.
    InvalidInput,
    /// Copy or staging write failed inside the isolated state. Fixed 500.
    Internal,
}

/// Whether a staged import ended in a known way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Known non-acceptance by the bridge (never reached the core).
    Rejected,
    /// The real final receipt for this request was observed.
    Completed,
    /// Acceptance or completion could not be confirmed; stage is retained.
    Unknown,
}

/// Cleanup ownership of a staged import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOwnership {
    /// The stage still owns its copy and nothing has removed it yet.
    Owned,
    /// The copy was intentionally kept for manual inspection.
    Retained,
    /// The copy and its per-request directory were removed.
    Cleaned,
}

/// One validated document copied into process-private staging.
///
/// Only [`ImportStage::core_path`] may reach the core; the client-supplied
/// value is never forwarded. The original filename is preserved so the core's
/// `DocumentSnapshot` keeps its name.
#[derive(Debug)]
pub struct ImportStage {
    core_path: PathBuf,
    filename: String,
    ownership: ImportOwnership,
}

impl ImportStage {
    /// Absolute path of the private copy the core is allowed to open.
    pub fn core_path(&self) -> &Path {
        &self.core_path
    }

    /// Basename preserved for the core-side document snapshot.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn ownership(&self) -> ImportOwnership {
        self.ownership
    }

    /// Keep the private copy for manual inspection. Every retention path
    /// (acceptance/completion unknown, forced shutdown) must call this before
    /// the stage is dropped; an unretained stage is removed on drop.
    pub fn retain(&mut self) {
        self.ownership = ImportOwnership::Retained;
    }
}

impl Drop for ImportStage {
    fn drop(&mut self) {
        if self.ownership == ImportOwnership::Owned {
            // Abandoned before acceptance (for example a cancelled HTTP
            // request): the copy was never handed to the core, so it is known
            // not accepted and may be removed. Retention paths call
            // [`ImportStage::retain`] first.
            remove_staged(&self.core_path);
            self.ownership = ImportOwnership::Cleaned;
        }
    }
}

/// Validate and copy one `imports/` document into private staging.
///
/// `filename` is a bare single segment (artifact naming rule) with an ASCII
/// `.md`/`.txt` extension, matched case-insensitively. The source is opened
/// once and re-checked on that handle: plain file, single hard link, no
/// reparse/link and final path equal to the expected path. Contents must be
/// 1 byte to 256 KiB of non-blank UTF-8. Nothing outside `<state>/imports` is
/// read, and no directory outside the isolated state is created.
pub fn stage_import(state_dir: &Path, filename: &str) -> Result<ImportStage, ImportError> {
    import_filename(filename)?;
    let metadata = std::fs::symlink_metadata(state_dir).map_err(|_| ImportError::InvalidInput)?;
    if !is_plain_directory(&metadata) {
        return Err(ImportError::InvalidInput);
    }
    let state_dir = std::fs::canonicalize(state_dir).map_err(|_| ImportError::InvalidInput)?;
    let imports = state_dir.join("imports");
    verify_directory(&imports).map_err(|_| ImportError::InvalidInput)?;

    let source = imports.join(filename);
    let bytes = read_import_source(&source)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| ImportError::InvalidInput)?;
    if text.trim().is_empty() {
        return Err(ImportError::InvalidInput);
    }

    let staging_root = imports.join(STAGING_DIR);
    prepare_staging_root(&staging_root)?;
    let request_dir = staging_root.join(Uuid::new_v4().to_string());
    std::fs::create_dir(&request_dir).map_err(|_| ImportError::Internal)?;
    let target = request_dir.join(filename);
    if write_staged(&target, &bytes).is_err() {
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir(&request_dir);
        return Err(ImportError::Internal);
    }
    Ok(ImportStage {
        core_path: target,
        filename: filename.to_owned(),
        ownership: ImportOwnership::Owned,
    })
}

/// Remove a staged copy for a known outcome, or keep it when unknown.
///
/// Bridge callers must route this through [`ImportRegistry`] so the I/O runs
/// on the cleanup thread instead of the bridge owner thread.
pub fn finalize_import(mut stage: ImportStage, outcome: ImportOutcome) {
    match outcome {
        ImportOutcome::Rejected | ImportOutcome::Completed => {
            remove_staged(&stage.core_path);
            stage.ownership = ImportOwnership::Cleaned;
        }
        ImportOutcome::Unknown => stage.ownership = ImportOwnership::Retained,
    }
}

fn remove_staged(core_path: &Path) {
    let _ = std::fs::remove_file(core_path);
    if let Some(parent) = core_path.parent() {
        // Only succeeds when the per-request directory is empty, so a foreign
        // file in it can never be removed by a shared name.
        let _ = std::fs::remove_dir(parent);
    }
}

fn import_filename(filename: &str) -> Result<(), ImportError> {
    if filename.is_empty()
        || filename.len() > MAX_RELATIVE_BYTES
        || !filename.is_ascii()
        || filename == "."
        || filename == ".."
        || filename.ends_with('.')
        || filename.ends_with(' ')
        || !filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || is_device_name(filename)
    {
        return Err(ImportError::InvalidInput);
    }
    let lower = filename.to_ascii_lowercase();
    let has_extension = (lower.ends_with(".md") && filename.len() > 3)
        || (lower.ends_with(".txt") && filename.len() > 4);
    if !has_extension {
        return Err(ImportError::InvalidInput);
    }
    Ok(())
}

fn prepare_staging_root(root: &Path) -> Result<(), ImportError> {
    match std::fs::create_dir(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(root).map_err(|_| ImportError::Internal)?;
            if !is_plain_directory(&metadata) {
                return Err(ImportError::Internal);
            }
            Ok(())
        }
        Err(_) => Err(ImportError::Internal),
    }
}

fn write_staged(target: &Path, bytes: &[u8]) -> Result<(), ImportError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|_| ImportError::Internal)?;
    file.write_all(bytes).map_err(|_| ImportError::Internal)?;
    file.flush().map_err(|_| ImportError::Internal)
}

#[cfg(windows)]
fn read_import_source(source: &Path) -> Result<Vec<u8>, ImportError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_ATTRIBUTE_DIRECTORY, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };

    let (mut file, info) = open_object(source, false).map_err(|_| ImportError::InvalidInput)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || info.nNumberOfLinks != 1 {
        return Err(ImportError::InvalidInput);
    }
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    if size == 0 || size > IMPORT_LIMIT as u64 {
        return Err(ImportError::InvalidInput);
    }

    let mut buffer = vec![0u16; 32_768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(ImportError::InvalidInput);
    }
    let actual = String::from_utf16(&buffer[..length]).map_err(|_| ImportError::InvalidInput)?;
    if !same_path(&actual, source) {
        return Err(ImportError::InvalidInput);
    }

    let mut bytes = Vec::with_capacity(size as usize);
    file.take(IMPORT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ImportError::InvalidInput)?;
    if bytes.is_empty() || bytes.len() > IMPORT_LIMIT {
        return Err(ImportError::InvalidInput);
    }
    Ok(bytes)
}

#[cfg(not(windows))]
fn read_import_source(source: &Path) -> Result<Vec<u8>, ImportError> {
    let metadata = std::fs::symlink_metadata(source).map_err(|_| ImportError::InvalidInput)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ImportError::InvalidInput);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(ImportError::InvalidInput);
        }
    }
    let size = metadata.len();
    if size == 0 || size > IMPORT_LIMIT as u64 {
        return Err(ImportError::InvalidInput);
    }
    let canonical = std::fs::canonicalize(source).map_err(|_| ImportError::InvalidInput)?;
    if canonical != source {
        return Err(ImportError::InvalidInput);
    }
    let mut file = std::fs::File::open(source).map_err(|_| ImportError::InvalidInput)?;
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(IMPORT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ImportError::InvalidInput)?;
    if bytes.is_empty() || bytes.len() > IMPORT_LIMIT {
        return Err(ImportError::InvalidInput);
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Receipt-driven cleanup registry
// ---------------------------------------------------------------------------

/// Bounded registry tying staged copies to real core `request_id` receipts.
///
/// Entries are inserted when the core accepts a submission and removed by the
/// bridge when that request's `CommandResult` is drained. Removal schedules
/// the deletion on one dedicated thread, so cleanup I/O never runs on the
/// bridge owner thread. A full registry refuses new imports instead of
/// evicting a stage whose fate is still unknown; a retained entry is only
/// released by its receipt or by bridge shutdown.
pub struct ImportRegistry {
    limit: usize,
    state: Mutex<RegistryState>,
    cleaner: Mutex<Option<ImportCleaner>>,
}

struct RegistryState {
    reserved: usize,
    active: HashMap<u64, ImportStage>,
}

/// Reservation for one import while it waits in the bridge queue.
pub struct ImportTicket {
    registry: Option<Arc<ImportRegistry>>,
}

impl ImportRegistry {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new(RegistryState {
                reserved: 0,
                active: HashMap::new(),
            }),
            cleaner: Mutex::new(None),
        })
    }

    /// Reserve one slot before queue admission. `None` means the registry is
    /// full and the caller must treat the import as a known rejection.
    pub fn reserve(self: &Arc<Self>) -> Option<ImportTicket> {
        let mut state = lock(&self.state);
        if state.reserved + state.active.len() >= self.limit {
            return None;
        }
        state.reserved += 1;
        Some(ImportTicket {
            registry: Some(self.clone()),
        })
    }

    /// Number of staged copies waiting for a receipt.
    pub fn pending_len(&self) -> usize {
        lock(&self.state).active.len()
    }

    /// Remove the staged copy for a request whose real receipt was observed.
    pub fn complete(&self, request_id: u64) {
        let removed = lock(&self.state).active.remove(&request_id);
        if let Some(stage) = removed {
            self.schedule(stage, ImportOutcome::Completed);
        }
    }

    /// Remove the staged copy for a known non-acceptance.
    pub fn reject(&self, stage: ImportStage) {
        self.schedule(stage, ImportOutcome::Rejected);
    }

    fn activate(&self, request_id: u64, stage: ImportStage) {
        let mut state = lock(&self.state);
        state.reserved = state.reserved.saturating_sub(1);
        state.active.insert(request_id, stage);
    }

    fn release_reservation(&self) {
        let mut state = lock(&self.state);
        state.reserved = state.reserved.saturating_sub(1);
    }

    fn schedule(&self, stage: ImportStage, outcome: ImportOutcome) {
        let mut cleaner = lock(&self.cleaner);
        if cleaner.is_none() {
            match ImportCleaner::start() {
                Ok(started) => *cleaner = Some(started),
                Err(_) => {
                    // No thread available: clean up here rather than leak, and
                    // never on the bridge thread (callers are HTTP or test).
                    drop(cleaner);
                    finalize_import(stage, outcome);
                    return;
                }
            }
        }
        if let Some(started) = cleaner.as_ref() {
            if let Err((stage, outcome)) = started.schedule(stage, outcome) {
                drop(cleaner);
                finalize_import(stage, outcome);
            }
        }
    }
}

impl Drop for ImportRegistry {
    fn drop(&mut self) {
        // Forced shutdown: accepted imports without a receipt keep their copy.
        if let Ok(state) = self.state.get_mut() {
            for stage in state.active.values_mut() {
                stage.retain();
            }
        }
    }
}

impl ImportTicket {
    /// Move the reservation to the accepted request id. After this the bridge
    /// owns the staged copy until the receipt arrives.
    pub fn activate(mut self, request_id: u64, stage: ImportStage) {
        if let Some(registry) = self.registry.take() {
            registry.activate(request_id, stage);
        }
    }
}

impl Drop for ImportTicket {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.release_reservation();
        }
    }
}

struct ImportCleaner {
    sender: Option<mpsc::Sender<(ImportStage, ImportOutcome)>>,
    worker: Option<JoinHandle<()>>,
}

impl ImportCleaner {
    fn start() -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<(ImportStage, ImportOutcome)>();
        let worker = std::thread::Builder::new()
            .name("agentranch-import-cleanup".into())
            .spawn(move || {
                for (stage, outcome) in receiver {
                    finalize_import(stage, outcome);
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    fn schedule(
        &self,
        stage: ImportStage,
        outcome: ImportOutcome,
    ) -> Result<(), (ImportStage, ImportOutcome)> {
        match self.sender.as_ref() {
            Some(sender) => sender.send((stage, outcome)).map_err(|error| error.0),
            None => Err((stage, outcome)),
        }
    }
}

impl Drop for ImportCleaner {
    fn drop(&mut self) {
        // Closing the channel lets the worker finish every queued job (known
        // outcomes only) before the join returns. Retained stages are never
        // queued, so shutdown leaves them on disk untouched.
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
