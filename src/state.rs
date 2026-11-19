//! Private state directories: classification, locking, and atomic wrapper
//! metadata.
//!
//! Every rule here exists because credentials live in the directory. A stale
//! lock descriptor, a replaced lock path, or a symlinked state root must never
//! authorize a write or a deletion.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::layout;

pub const WRAPPER_FORMAT: u64 = 1;
pub const OWNER_MARKER: &str = "portable-opentunnel";

/// The documented API default; persisted before provisioning so later runs
/// use the recorded endpoint.
pub const DEFAULT_API: &str = "https://opentunnel.xyz";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Keep the identity and its route after exit.
    Reusable,
    /// Create a fresh identity and delete it remotely on exit.
    Ephemeral,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Reusable => "reusable",
            Mode::Ephemeral => "ephemeral",
        }
    }
}

/// The single wrapper metadata file: format, ownership, mode, endpoint, route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WrapperMeta {
    pub format: u64,
    pub owner: String,
    pub mode: Mode,
    pub api: String,
    /// Route label claimed on attach; the public URL is
    /// `https://<route>.<hostname>`, or `https://<hostname>` for `@`.
    pub route: String,
    /// The remote registration is known to be gone, so only local cleanup
    /// remains.
    ///
    /// Written *before* any credential file is unlinked, so an interrupted
    /// deletion can always be resumed without contacting the API again — which
    /// it could not do anyway once `token`/`private-key.pem` are partly gone.
    /// Absent in metadata written by older versions, hence `default`; and
    /// omitted when false, so a live identity's file stays byte-identical.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote_deleted: bool,
}

impl WrapperMeta {
    pub fn new(mode: Mode, api: String, route: String) -> Self {
        Self {
            format: WRAPPER_FORMAT,
            owner: OWNER_MARKER.to_owned(),
            mode,
            api,
            route,
            remote_deleted: false,
        }
    }

    pub fn validate(&self) -> AppResult<()> {
        if self.format != WRAPPER_FORMAT {
            return Err(AppError::fail(format!(
                "unsupported state format {}: this build understands format {WRAPPER_FORMAT}",
                self.format
            )));
        }
        if self.owner != OWNER_MARKER {
            return Err(AppError::fail(format!(
                "state directory is owned by '{}', not '{OWNER_MARKER}'",
                self.owner
            )));
        }
        if !opentunnel::protocol::names::is_valid_route(&self.route) {
            return Err(AppError::fail(format!(
                "saved route '{}' is invalid; refusing to guess a replacement",
                self.route
            )));
        }
        url::Url::parse(&self.api)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
            .ok_or_else(|| {
                AppError::fail(format!(
                    "saved API endpoint '{}' is not an http(s) URL with a host",
                    self.api
                ))
            })?;
        Ok(())
    }
}

/// What a state directory contains, decided without writing anything.
#[derive(Debug)]
pub enum Classification {
    /// Nothing but the lock and/or recognized owned residue: a first run.
    FirstRun { residue: Vec<PathBuf> },
    /// Valid metadata without a completed identity: provisioning is pending
    /// or never started.
    Resumable { meta: WrapperMeta },
    /// A complete identity: metadata plus the SDK's `tunnel.json`.
    Initialized { meta: WrapperMeta, hostname: String },
    /// The remote registration is already gone and only local deletion
    /// remains. Never reprovision over this: finish the deletion instead.
    Deleting { meta: WrapperMeta },
    /// The directory cannot be used, and must not be replaced silently.
    Corrupt { reason: String },
    /// Unrelated content: not ours to initialize inside or delete.
    Unrelated { reason: String },
}

impl Classification {
    pub fn is_initialized(&self) -> bool {
        matches!(self, Classification::Initialized { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SdkContent {
    /// No `sdk/` directory at all.
    Absent,
    /// `sdk/` holds only recognized residue and no profile.
    ResidueOnly,
    /// `sdk/default/` exists.
    Profile,
    /// `sdk/` holds entries this program does not recognize.
    Unknown,
}

struct Scan {
    has_wrapper: bool,
    sdk: SdkContent,
    has_profile_tunnel: bool,
    residue: Vec<PathBuf>,
    unknown: Vec<String>,
    unsafe_entries: Vec<String>,
}

fn scan(root: &Path) -> AppResult<Scan> {
    let mut scan = Scan {
        has_wrapper: false,
        sdk: SdkContent::Absent,
        has_profile_tunnel: false,
        residue: Vec::new(),
        unknown: Vec::new(),
        unsafe_entries: Vec::new(),
    };
    let entries =
        fs::read_dir(root).map_err(AppError::io(format!("reading {}", root.display())))?;
    for entry in entries {
        let entry = entry.map_err(AppError::io(format!("reading {}", root.display())))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            scan.unknown.push("<non-UTF-8 name>".to_owned());
            continue;
        };
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(AppError::io(format!("inspecting {}", path.display())))?;
        let kind = layout::root_entry(name);
        // A symlink at any recognized name is a substitution attempt.
        if file_type.is_symlink() && kind != layout::RootEntry::Unknown {
            scan.unsafe_entries.push(name.to_owned());
            continue;
        }
        match kind {
            layout::RootEntry::Lock => {
                if !file_type.is_file() {
                    scan.unsafe_entries.push(name.to_owned());
                }
            }
            layout::RootEntry::Wrapper => {
                if file_type.is_file() {
                    scan.has_wrapper = true;
                } else {
                    scan.unsafe_entries.push(name.to_owned());
                }
            }
            layout::RootEntry::WrapperTemp => {
                if file_type.is_file() {
                    scan.residue.push(path);
                } else {
                    scan.unsafe_entries.push(name.to_owned());
                }
            }
            layout::RootEntry::SdkDir => {
                if file_type.is_dir() {
                    scan.sdk = scan_sdk_dir(root, &mut scan)?;
                } else {
                    scan.unsafe_entries.push(name.to_owned());
                }
            }
            layout::RootEntry::Unknown => scan.unknown.push(name.to_owned()),
        }
    }
    Ok(scan)
}

/// Inspects `sdk/` and `sdk/default/`, recording owned residue and unknown
/// entries.
fn scan_sdk_dir(root: &Path, scan: &mut Scan) -> AppResult<SdkContent> {
    let sdk = layout::sdk_root(root);
    let sdk_entries =
        fs::read_dir(&sdk).map_err(AppError::io(format!("reading {}", sdk.display())))?;
    let mut content = SdkContent::ResidueOnly;
    for entry in sdk_entries {
        let entry = entry.map_err(AppError::io(format!("reading {}", sdk.display())))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            scan.unknown.push("sdk/<non-UTF-8 name>".to_owned());
            content = SdkContent::Unknown;
            continue;
        };
        let file_type = entry.file_type().map_err(AppError::io(format!(
            "inspecting {}",
            entry.path().display()
        )))?;
        if name == layout::PROFILE {
            if file_type.is_dir() {
                content = SdkContent::Profile;
            } else {
                scan.unsafe_entries.push(format!("sdk/{name}"));
            }
        } else if file_type.is_file() && layout::is_profile_temp(name) {
            scan.residue.push(entry.path());
        } else {
            scan.unknown.push(format!("sdk/{name}"));
            if content != SdkContent::Profile {
                content = SdkContent::Unknown;
            }
        }
    }
    if content != SdkContent::Profile {
        return Ok(content);
    }

    let profile = layout::profile_dir(root);
    let profile_content = scan_profile(&profile, scan)?;
    scan.has_profile_tunnel = profile_content.completed;
    // A profile holding only recognized atomic-write residue is an interrupted
    // first run, not an identity: no final file was ever committed.
    if profile_content.has_final {
        Ok(SdkContent::Profile)
    } else {
        Ok(SdkContent::ResidueOnly)
    }
}

/// What `sdk/default/` holds beyond residue.
#[derive(Debug, Clone, Copy, Default)]
struct ProfileContent {
    /// At least one final (committed) file exists.
    has_final: bool,
    /// `tunnel.json`, the completion marker, exists.
    completed: bool,
}

fn scan_profile(profile: &Path, scan: &mut Scan) -> AppResult<ProfileContent> {
    let mut content = ProfileContent::default();
    let entries =
        fs::read_dir(profile).map_err(AppError::io(format!("reading {}", profile.display())))?;
    for entry in entries {
        let entry = entry.map_err(AppError::io(format!("reading {}", profile.display())))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            scan.unknown.push("sdk/default/<non-UTF-8 name>".to_owned());
            continue;
        };
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(AppError::io(format!("inspecting {}", path.display())))?;
        if file_type.is_symlink() {
            scan.unsafe_entries.push(format!("sdk/default/{name}"));
            continue;
        }
        match layout::profile_entry(name) {
            layout::ProfileEntry::Final => {
                if file_type.is_file() {
                    content.has_final = true;
                    if name == layout::TUNNEL {
                        content.completed = true;
                    }
                } else {
                    scan.unsafe_entries.push(format!("sdk/default/{name}"));
                }
            }
            layout::ProfileEntry::Temp => {
                if file_type.is_file() {
                    scan.residue.push(path);
                } else {
                    scan.unsafe_entries.push(format!("sdk/default/{name}"));
                }
            }
            layout::ProfileEntry::Unknown => scan.unknown.push(format!("sdk/default/{name}")),
        }
    }
    Ok(content)
}

fn read_wrapper(root: &Path) -> AppResult<WrapperMeta> {
    let path = root.join(layout::WRAPPER_FILE);
    let bytes = fs::read(&path).map_err(AppError::io(format!("reading {}", path.display())))?;
    let meta: WrapperMeta = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::fail(format!(
            "{} is malformed ({error}); refusing to replace the identity",
            path.display()
        ))
    })?;
    meta.validate()?;
    Ok(meta)
}

/// Reads the SDK's `tunnel.json` hostname only; no credential file is read.
fn read_hostname(root: &Path) -> AppResult<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TunnelFile {
        hostname: String,
    }
    let path = layout::profile_dir(root).join(layout::TUNNEL);
    let bytes = fs::read(&path).map_err(AppError::io(format!("reading {}", path.display())))?;
    let file: TunnelFile = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::fail(format!("{} is malformed ({error})", path.display())))?;
    if file.hostname.is_empty()
        || !file
            .hostname
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
    {
        return Err(AppError::fail(format!(
            "{} records an invalid hostname",
            path.display()
        )));
    }
    Ok(file.hostname)
}

/// Decides what the directory holds. Never creates, writes, or locks.
pub fn classify(root: &Path) -> AppResult<Classification> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Classification::FirstRun {
                residue: Vec::new(),
            });
        }
        Err(error) => {
            return Err(AppError::io(format!("inspecting {}", root.display()))(
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(AppError::fail(format!(
            "{} is a symlink; a state directory must be a real directory",
            root.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(AppError::fail(format!(
            "{} exists and is not a directory",
            root.display()
        )));
    }
    let scan = scan(root)?;

    if !scan.unsafe_entries.is_empty() {
        let mut names = scan.unsafe_entries.clone();
        names.sort();
        return Ok(Classification::Corrupt {
            reason: format!(
                "{} contains entries with unsafe types (symlinks or non-regular files): {}",
                root.display(),
                names.join(", ")
            ),
        });
    }

    if scan.has_wrapper {
        let meta = match read_wrapper(root) {
            Ok(meta) => meta,
            Err(error) => {
                return Ok(Classification::Corrupt {
                    reason: error.message,
                });
            }
        };
        // The deletion marker wins over everything else: the remote
        // registration is gone, and the credential files may be partially or
        // wholly absent. Neither a URL nor a reprovision is meaningful here.
        if meta.remote_deleted {
            return Ok(Classification::Deleting { meta });
        }
        if scan.has_profile_tunnel {
            let hostname = match read_hostname(root) {
                Ok(hostname) => hostname,
                Err(error) => {
                    return Ok(Classification::Corrupt {
                        reason: error.message,
                    });
                }
            };
            return Ok(Classification::Initialized { meta, hostname });
        }
        return Ok(Classification::Resumable { meta });
    }

    // No metadata. An identity without its recorded route cannot be
    // recovered, so it is never replaced silently.
    match scan.sdk {
        SdkContent::Profile => {
            return Ok(Classification::Corrupt {
                reason: format!(
                    "{} holds SDK identity files but no {}; the route is unknown, so the \
                     identity is not replaced",
                    root.display(),
                    layout::WRAPPER_FILE
                ),
            });
        }
        SdkContent::Unknown => {
            return Ok(Classification::Unrelated {
                reason: format!(
                    "{} contains files this program does not recognize",
                    root.display()
                ),
            });
        }
        SdkContent::Absent | SdkContent::ResidueOnly => {}
    }
    if !scan.unknown.is_empty() {
        let mut names = scan.unknown.clone();
        names.sort();
        return Ok(Classification::Unrelated {
            reason: format!(
                "{} is not empty and is not a portable-opentunnel state directory (found: {})",
                root.display(),
                names.join(", ")
            ),
        });
    }
    Ok(Classification::FirstRun {
        residue: scan.residue,
    })
}

/// The public HTTPS URL for a route and tunnel hostname.
pub fn route_url(route: &str, hostname: &str) -> String {
    if route == opentunnel::protocol::names::ROOT_ROUTE {
        format!("https://{hostname}")
    } else {
        format!("https://{route}.{hostname}")
    }
}

// ---------------------------------------------------------------------------
// Directory permissions
// ---------------------------------------------------------------------------

pub fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Verifies that `root` is a real directory owned by this user and not
/// accessible by group or others. Existing permissions are never relaxed.
pub fn check_private_dir(root: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(root)
        .map_err(AppError::io(format!("inspecting {}", root.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::fail(format!(
            "{} is not a real directory",
            root.display()
        )));
    }
    if metadata.uid() != current_uid() {
        return Err(AppError::fail(format!(
            "{} is owned by uid {}, not the current user (uid {})",
            root.display(),
            metadata.uid(),
            current_uid()
        )));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(AppError::fail(format!(
            "{} is accessible by group or others (mode {mode:o}); credentials must stay private. \
             Run: chmod 700 {}",
            root.display(),
            root.display()
        )));
    }
    Ok(())
}

/// Creates `root` with mode 0700, failing if anything is already there.
pub fn create_private_dir(root: &Path) -> AppResult<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(root)
        .map_err(AppError::io(format!("creating {}", root.display())))?;
    fsync_dir(root)
}

pub fn fsync_dir(path: &Path) -> AppResult<()> {
    let file =
        File::open(path).map_err(AppError::io(format!("opening {} for sync", path.display())))?;
    file.sync_all()
        .map_err(AppError::io(format!("syncing {}", path.display())))
}

/// dev/ino pair identifying a directory or file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    pub fn of(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

fn open_dir(root: &Path) -> AppResult<FileId> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(AppError::io(format!("opening {}", root.display())))?;
    let metadata = file
        .metadata()
        .map_err(AppError::io(format!("inspecting {}", root.display())))?;
    Ok(FileId::of(&metadata))
}

// ---------------------------------------------------------------------------
// The lock
// ---------------------------------------------------------------------------

/// An exclusive lock on one state directory, held for the whole operation.
///
/// `Drop` releases the OS lock and closes the descriptor. Reusable state keeps
/// the lock file; final deletion removes it while the lock is still held.
pub struct StateLock {
    root: PathBuf,
    /// Held open for the whole operation; closing it releases the lock.
    _file: File,
    path_id: FileId,
    dir_id: FileId,
}

impl std::fmt::Debug for StateLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StateLock")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl StateLock {
    /// Opens (creating when `create`) and locks `<root>/.lock`.
    pub fn acquire(root: &Path, create: bool) -> AppResult<Self> {
        let dir_id = open_dir(root)?;
        let path = root.join(layout::LOCK_FILE);

        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        if create {
            options.create(true).truncate(false).mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(AppError::io(format!("opening {}", path.display())))?;

        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => AppError::fail(format!(
                "another portable-opentunnel process holds the lock in {}",
                root.display()
            )),
            fs::TryLockError::Error(error) => AppError::fail(format!(
                "cannot lock {}: {error}. Reliable exclusive locking is required; \
                 some shared-filesystem mount options do not provide it",
                path.display()
            )),
        })?;

        let metadata = file
            .metadata()
            .map_err(AppError::io(format!("inspecting {}", path.display())))?;
        let lock = Self {
            root: root.to_path_buf(),
            _file: file,
            path_id: FileId::of(&metadata),
            dir_id,
        };
        lock.validate()?;
        Ok(lock)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Confirms that the opened descriptor still is the file at the lock path
    /// and that the directory is still the one we opened. Called before every
    /// state write and before any deletion.
    pub fn validate(&self) -> AppResult<()> {
        let path = self.root.join(layout::LOCK_FILE);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            AppError::fail(format!(
                "the lock at {} is gone or unreadable ({error}); refusing to continue",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::fail(format!(
                "the lock at {} was replaced by an unsafe entry; refusing to continue",
                path.display()
            )));
        }
        if FileId::of(&metadata) != self.path_id {
            return Err(AppError::fail(format!(
                "{} or its lock was replaced by another file; refusing to continue",
                self.root.display()
            )));
        }
        if open_dir(&self.root)? != self.dir_id {
            return Err(AppError::fail(format!(
                "{} was replaced while it was in use; refusing to continue",
                self.root.display()
            )));
        }
        Ok(())
    }

    /// Removes the lock file as the final step of deleting a state directory,
    /// while still holding the lock.
    pub fn remove_lock_file(&mut self) -> AppResult<()> {
        self.validate()?;
        let path = self.root.join(layout::LOCK_FILE);
        fs::remove_file(&path).map_err(AppError::io(format!("removing {}", path.display())))
    }
}

// ---------------------------------------------------------------------------
// Wrapper metadata writes
// ---------------------------------------------------------------------------

fn random_suffix() -> String {
    crate::random::alphanumeric(8)
}

fn create_new_private(path: &Path, content: &[u8]) -> AppResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(AppError::io(format!("creating {}", path.display())))?;
    file.write_all(content)
        .and_then(|()| file.sync_all())
        .map_err(AppError::io(format!("writing {}", path.display())))
}

/// Atomically replaces `wrapper.json`: exclusive private temporary file,
/// fsync, rename, then directory sync.
pub fn write_wrapper(root: &Path, meta: &WrapperMeta) -> AppResult<()> {
    meta.validate()?;
    let mut content = serde_json::to_vec_pretty(meta)
        .map_err(|error| AppError::fail(format!("serializing state metadata: {error}")))?;
    content.push(b'\n');
    let temporary = root.join(format!(
        "{}.tmp-{}-{}",
        layout::WRAPPER_FILE,
        std::process::id(),
        random_suffix()
    ));
    create_new_private(&temporary, &content)?;
    let final_path = root.join(layout::WRAPPER_FILE);
    if let Err(error) = fs::rename(&temporary, &final_path) {
        let _ = fs::remove_file(&temporary);
        return Err(AppError::io(format!("replacing {}", final_path.display()))(
            error,
        ));
    }
    fsync_dir(root)
}

/// Atomically records that the remote registration is gone.
///
/// This must succeed *before* any credential file is unlinked: it is the only
/// thing that makes an interrupted local deletion resumable, because a partial
/// profile can no longer be authenticated to the API. A failure leaves the
/// credentials untouched and the caller must abort.
pub fn mark_remote_deleted(root: &Path, meta: &WrapperMeta) -> AppResult<WrapperMeta> {
    let mut marked = meta.clone();
    marked.remote_deleted = true;
    write_wrapper(root, &marked)?;
    Ok(marked)
}

/// Whether a recognized residue file is safe to remove: a regular file owned
/// by this user, not a symlink.
pub fn safe_to_remove(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == current_uid()
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    fn reusable() -> WrapperMeta {
        WrapperMeta::new(Mode::Reusable, DEFAULT_API.to_owned(), "abc123".to_owned())
    }

    fn complete_profile(root: &Path) {
        let profile = layout::profile_dir(root);
        fs::create_dir_all(&profile).unwrap();
        for name in [
            layout::TOKEN,
            layout::PRIVATE_KEY,
            layout::CERTIFICATE,
            layout::CHAIN,
        ] {
            fs::write(profile.join(name), b"secret").unwrap();
        }
    }

    #[test]
    fn new_directory_is_classified_as_first_run() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::FirstRun { ref residue } if residue.is_empty()
        ));
    }

    #[test]
    fn missing_directory_is_first_run_without_creating_it() {
        let temp = TempDir::new();
        let root = temp.path().join("absent");
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::FirstRun { .. }
        ));
        assert!(!root.exists(), "classify must not create the directory");
    }

    #[test]
    fn wrapper_round_trips() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        write_wrapper(&root, &reusable()).unwrap();
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Resumable { .. }
        ));
        assert_eq!(
            fs::metadata(root.join(layout::WRAPPER_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn the_remote_deleted_marker_reclassifies_even_without_credentials() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        write_wrapper(&root, &reusable()).unwrap();

        // An identity that had been fully provisioned, then interrupted after
        // the remote deletion was recorded and some credential files were
        // already unlinked: `token`/`key` are gone, `tunnel.json` may remain.
        let profile = layout::profile_dir(&root);
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join(layout::TUNNEL),
            br#"{"id":"abc","hostname":"abc.opentunnel.xyz","certificateExpiry":"2099-01-01T00:00:00.000Z"}"#,
        )
        .unwrap();
        // No token, no private key: `load` would call this corrupt.
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Corrupt { .. } | Classification::Initialized { .. }
        ));

        let marked = mark_remote_deleted(&root, &reusable()).unwrap();
        assert!(marked.remote_deleted);
        // With the marker, the directory is a resumable deletion, not corrupt.
        match classify(&root).unwrap() {
            Classification::Deleting { meta } => {
                assert!(meta.remote_deleted);
                assert_eq!(meta.route, "abc123");
            }
            other => panic!("expected a pending deletion, got {other:?}"),
        }
    }

    #[test]
    fn old_metadata_without_the_marker_still_loads() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        // Exactly what an older build wrote: no `remote_deleted` field.
        fs::write(
            root.join(layout::WRAPPER_FILE),
            br#"{"format":1,"owner":"portable-opentunnel","mode":"reusable","api":"https://opentunnel.xyz","route":"abc123"}"#,
        )
        .unwrap();
        match classify(&root).unwrap() {
            Classification::Resumable { meta } => assert!(!meta.remote_deleted),
            other => panic!("expected a resumable identity, got {other:?}"),
        }
    }

    #[test]
    fn marking_the_deletion_is_atomic_and_keeps_the_route() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        write_wrapper(&root, &reusable()).unwrap();
        let before = fs::read(root.join(layout::WRAPPER_FILE)).unwrap();

        let marked = mark_remote_deleted(&root, &reusable()).unwrap();
        assert!(marked.remote_deleted);
        assert_eq!(marked.route, "abc123", "the route must be preserved");
        // No temporary file is left behind, and permissions stay private.
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|name| name.contains(".tmp-")),
            "residue left behind: {names:?}"
        );
        assert_eq!(
            fs::metadata(root.join(layout::WRAPPER_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_ne!(before, fs::read(root.join(layout::WRAPPER_FILE)).unwrap());
    }

    #[test]
    fn malformed_metadata_is_corrupt() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        fs::write(root.join(layout::WRAPPER_FILE), b"{not json").unwrap();
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Corrupt { .. }
        ));
    }

    #[test]
    fn metadata_without_a_route_is_corrupt() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        fs::write(
            root.join(layout::WRAPPER_FILE),
            br#"{"format":1,"owner":"portable-opentunnel","mode":"reusable","api":"https://opentunnel.xyz"}"#,
        )
        .unwrap();
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Corrupt { .. }
        ));
    }

    #[test]
    fn completed_identity_is_initialized() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        write_wrapper(&root, &reusable()).unwrap();
        complete_profile(&root);
        fs::write(
            layout::profile_dir(&root).join(layout::TUNNEL),
            br#"{"id":"abc","hostname":"abc.opentunnel.xyz","certificateExpiry":"2099-01-01T00:00:00.000Z"}"#,
        )
        .unwrap();
        match classify(&root).unwrap() {
            Classification::Initialized { meta, hostname } => {
                assert_eq!(meta.route, "abc123");
                assert_eq!(hostname, "abc.opentunnel.xyz");
            }
            other => panic!("expected initialized, got {other:?}"),
        }
    }

    #[test]
    fn sdk_files_without_wrapper_are_corrupt_not_replaced() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        complete_profile(&root);
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Corrupt { .. }
        ));
    }

    #[test]
    fn unrelated_files_are_not_adopted() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        fs::write(root.join("notes.txt"), b"mine").unwrap();
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Unrelated { .. }
        ));
    }

    #[test]
    fn recognized_residue_is_recovered() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let profile = layout::profile_dir(&root);
        fs::create_dir_all(&profile).unwrap();
        fs::write(root.join("wrapper.json.tmp-1-abc"), b"{}").unwrap();
        fs::write(profile.join("token.tmp-1"), b"secret").unwrap();
        match classify(&root).unwrap() {
            Classification::FirstRun { residue } => assert_eq!(residue.len(), 2),
            other => panic!("expected first run, got {other:?}"),
        }
    }

    #[test]
    fn sdk_dir_with_only_residue_is_a_first_run() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let sdk = layout::sdk_root(&root);
        fs::create_dir_all(&sdk).unwrap();
        fs::write(sdk.join("token.tmp-3"), b"partial").unwrap();
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::FirstRun { ref residue } if residue.len() == 1
        ));
    }

    #[test]
    fn unknown_files_inside_the_profile_are_preserved() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        write_wrapper(&root, &reusable()).unwrap();
        let profile = layout::profile_dir(&root);
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("notes.txt"), b"mine").unwrap();
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Resumable { .. }
        ));
    }

    #[test]
    fn symlinked_lock_is_rejected() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let target = temp.path().join("elsewhere");
        fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, root.join(layout::LOCK_FILE)).unwrap();
        assert!(StateLock::acquire(&root, true).is_err());
        assert!(matches!(
            classify(&root).unwrap(),
            Classification::Corrupt { .. }
        ));
    }

    #[test]
    fn second_acquire_fails() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let _held = StateLock::acquire(&root, true).unwrap();
        let error = StateLock::acquire(&root, true).unwrap_err();
        assert!(
            error.message.contains("holds the lock"),
            "{}",
            error.message
        );
    }

    #[test]
    fn replaced_lock_path_is_detected() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let lock = StateLock::acquire(&root, true).unwrap();
        fs::remove_file(root.join(layout::LOCK_FILE)).unwrap();
        fs::write(root.join(layout::LOCK_FILE), b"").unwrap();
        assert!(lock.validate().is_err());
    }

    #[test]
    fn unlinked_lock_never_authorizes_writes() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let mut lock = StateLock::acquire(&root, true).unwrap();
        fs::remove_file(root.join(layout::LOCK_FILE)).unwrap();
        assert!(lock.validate().is_err());
        assert!(lock.remove_lock_file().is_err());
    }

    #[test]
    fn route_urls_cover_root_and_label() {
        assert_eq!(route_url("@", "x.test"), "https://x.test");
        assert_eq!(route_url("abc", "x.test"), "https://abc.x.test");
    }

    #[test]
    fn refuses_a_group_accessible_directory() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = check_private_dir(&root).unwrap_err();
        assert!(error.message.contains("chmod 700"), "{}", error.message);
    }
}
