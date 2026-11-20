//! Deciding what a state directory holds, without writing anything.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::WrapperMeta;
use super::layout;
use crate::error::{AppError, AppResult};

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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::state::{
        DEFAULT_API, Mode, StateLock, create_private_dir, mark_remote_deleted, write_wrapper,
    };
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
        mark_remote_deleted(&root, &reusable()).unwrap();
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
    fn residue_in_the_root_and_the_profile_is_a_first_run() {
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
    fn unknown_files_inside_the_profile_leave_metadata_resumable() {
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
}
