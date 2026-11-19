//! Local deletion of a wrapper-owned state directory, and remote deletion.
//!
//! Deletion never recurses into an arbitrary user-supplied path. Only the
//! documented layout is removed, file by file, after re-validating the lock:
//! recognized owned residue, then the SDK profile, then wrapper metadata,
//! then the lock, then a non-recursive `remove_dir` of the root. Unrelated
//! entries are preserved and make the directory removal fail harmlessly.

use std::fs;
use std::io;

use crate::error::{AppError, AppResult};
use crate::layout;
use crate::state::{self, StateLock};

/// Outcome of a local deletion.
#[derive(Debug)]
pub struct LocalRemoval {
    /// Files or directories that could not be removed.
    pub failures: Vec<String>,
    /// True when the state root itself is gone.
    pub root_removed: bool,
    /// Entries left behind because they are not ours.
    pub preserved: Vec<String>,
}

impl LocalRemoval {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty() && self.root_removed
    }

    /// A short description for diagnostics.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.failures.is_empty() {
            parts.push(format!("could not remove: {}", self.failures.join("; ")));
        }
        if self.root_removed {
            parts.push("state directory removed".to_owned());
        } else if !self.preserved.is_empty() {
            parts.push(format!(
                "state directory retained because it contains files this program does not own: {}",
                self.preserved.join(", ")
            ));
        } else {
            parts.push("state directory retained".to_owned());
        }
        parts.join("; ")
    }
}

/// The recovery command printed when a deletion could not complete.
pub fn recovery_command(root: &std::path::Path) -> String {
    format!(
        "portable-opentunnel delete --directory {}",
        shell_quote(&root.display().to_string())
    )
}

/// Quotes a path for a POSIX shell when it needs quoting.
pub fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '@' | '+')
        });
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn remove_entry(path: &std::path::Path, failures: &mut Vec<String>) {
    if let Err(error) = unlink(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        failures.push(format!("{} ({error})", path.display()));
    }
}

/// The single deletion primitive, so a test can inject a deterministic failure.
///
/// Permission-based injection (`chmod 0500` on the parent) is not usable: a
/// container or a `cargo test` run as root ignores the mode bits and the unlink
/// succeeds, which would silently turn the ownership-preservation test into a
/// test of nothing.
fn unlink(path: &std::path::Path) -> io::Result<()> {
    #[cfg(test)]
    if inject_unlink_failure(path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected failure",
        ));
    }
    fs::remove_file(path)
}

/// Test-only seam: fails unlinks of paths whose name matches the armed entry.
#[cfg(test)]
fn inject_unlink_failure(path: &std::path::Path) -> bool {
    let armed = INJECTED_UNLINK.with(|slot| slot.borrow().clone());
    armed.is_some_and(|name| path.file_name().is_some_and(|file| file == name.as_str()))
}

#[cfg(test)]
thread_local! {
    static INJECTED_UNLINK: std::cell::RefCell<Option<String>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Arms [`inject_unlink_failure`] for the duration of the guard.
#[cfg(test)]
struct InjectedUnlinkFailure;

#[cfg(test)]
impl InjectedUnlinkFailure {
    fn arm(name: &str) -> Self {
        INJECTED_UNLINK.with(|slot| *slot.borrow_mut() = Some(name.to_owned()));
        Self
    }
}

#[cfg(test)]
impl Drop for InjectedUnlinkFailure {
    fn drop(&mut self) {
        INJECTED_UNLINK.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Removes a directory that should now be empty.
///
/// A non-empty directory is *retained*, not a failure: it can only hold entries
/// this program does not own, and those are reported instead. Real I/O errors
/// are still failures.
fn remove_dir_if_present(
    path: &std::path::Path,
    failed: &mut Vec<String>,
    retained: &mut Vec<String>,
) {
    match fs::remove_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
            retained.push(format!("{} (unrelated entries)", path.display()));
        }
        // `ENOTEMPTY` is not always mapped; compare raw codes as well.
        Err(error) if error.raw_os_error() == Some(libc::ENOTEMPTY) => {
            retained.push(format!("{} (unrelated entries)", path.display()));
        }
        Err(error) => failed.push(format!("{} ({error})", path.display())),
    }
}

/// Deletes the owned parts of a state directory while holding its lock.
///
/// Callers must have validated the lock and, for an ephemeral or explicit
/// delete, completed any remote deletion first.
pub fn remove_local(lock: &mut StateLock) -> AppResult<LocalRemoval> {
    lock.validate()?;
    let root = lock.root().to_path_buf();
    let mut failures = Vec::new();
    let mut preserved = Vec::new();

    // 1. Owned residue first, then final SDK files. The SDK profile is a real
    //    directory of known names only; anything else there is preserved, and
    //    a directory kept alive by such entries is retained rather than failed.
    let profile = layout::profile_dir(&root);
    let sdk = layout::sdk_root(&root);
    remove_owned_sdk_profile(&profile, &mut failures, &mut preserved);
    remove_dir_if_present(&profile, &mut failures, &mut preserved);
    remove_owned_sdk_dir(&sdk, &mut failures, &mut preserved);
    remove_dir_if_present(&sdk, &mut failures, &mut preserved);

    // 2. Wrapper metadata and its temporary residue — but only once the owned
    //    SDK entries are actually gone. Removing the wrapper first would leave
    //    SDK files without their ownership+route record, which the classifier
    //    can only call corrupt: a state the printed `delete` retry could never
    //    repair.
    let sdk_clean = failures.is_empty();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => {
            return Err(AppError::io(format!("reading {}", root.display()))(error));
        }
    };
    let mut own_names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(AppError::io(format!("reading {}", root.display())))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            preserved.push("<non-UTF-8 name>".to_owned());
            continue;
        };
        match layout::root_entry(name) {
            layout::RootEntry::Wrapper | layout::RootEntry::WrapperTemp => {
                if !sdk_clean {
                    // Keep the wrapper: it is the only record that this
                    // directory is ours and which route it carries.
                    preserved.push(name.to_owned());
                } else if state::safe_to_remove(&entry.path()) {
                    remove_entry(&entry.path(), &mut failures);
                } else {
                    preserved.push(name.to_owned());
                }
            }
            layout::RootEntry::Lock => own_names.push(name.to_owned()),
            layout::RootEntry::SdkDir => preserved.push(name.to_owned()),
            layout::RootEntry::Unknown => preserved.push(name.to_owned()),
        }
    }

    // 3. The lock last, still held.
    if failures.is_empty()
        && let Err(error) = lock.remove_lock_file()
    {
        failures.push(error.message);
    }

    // 4. Non-recursive root removal: fails harmlessly if preserved entries or
    //    a fresh concurrent run are there.
    let root_removed = if failures.is_empty() {
        match fs::remove_dir(&root) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    } else {
        false
    };

    if !root_removed && failures.is_empty() {
        // Re-list to report what kept the directory alive.
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !preserved.contains(&name) {
                    preserved.push(name);
                }
            }
        }
        preserved.sort();
        preserved.dedup();
    }
    let _ = own_names;

    Ok(LocalRemoval {
        failures,
        root_removed,
        preserved,
    })
}

fn remove_owned_sdk_profile(
    profile: &std::path::Path,
    failures: &mut Vec<String>,
    preserved: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(profile) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            preserved.push("sdk/default/<non-UTF-8 name>".to_owned());
            continue;
        };
        match layout::profile_entry(name) {
            layout::ProfileEntry::Final | layout::ProfileEntry::Temp => {
                if state::safe_to_remove(&entry.path()) {
                    remove_entry(&entry.path(), failures);
                } else {
                    preserved.push(format!("sdk/default/{name}"));
                }
            }
            layout::ProfileEntry::Unknown => preserved.push(format!("sdk/default/{name}")),
        }
    }
}

fn remove_owned_sdk_dir(
    sdk: &std::path::Path,
    failures: &mut Vec<String>,
    preserved: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(sdk) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            preserved.push("sdk/<non-UTF-8 name>".to_owned());
            continue;
        };
        if name == layout::PROFILE {
            continue;
        }
        // Residue can also be left directly in the storage root.
        if layout::is_profile_temp(name) && state::safe_to_remove(&entry.path()) {
            remove_entry(&entry.path(), failures);
        } else {
            preserved.push(format!("sdk/{name}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::layout::PROFILE;
    use crate::state::{DEFAULT_API, Mode, WrapperMeta, write_wrapper};
    use crate::test_support::TempDir;

    fn populated(root: &std::path::Path) {
        state::create_private_dir(root).unwrap();
        let lock = StateLock::acquire(root, true).unwrap();
        write_wrapper(
            root,
            &WrapperMeta::new(Mode::Reusable, DEFAULT_API.to_owned(), "abc".to_owned()),
        )
        .unwrap();
        let profile = layout::profile_dir(root);
        fs::create_dir_all(&profile).unwrap();
        for name in [layout::TOKEN, layout::PRIVATE_KEY, layout::TUNNEL] {
            fs::write(profile.join(name), b"secret").unwrap();
        }
        lock.validate().unwrap();
    }

    #[test]
    fn removes_the_whole_owned_layout() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);
        let mut lock = StateLock::acquire(&root, false).unwrap();
        let removal = remove_local(&mut lock).unwrap();
        assert!(removal.failures.is_empty(), "{removal:?}");
        assert!(removal.root_removed, "{removal:?}");
        assert!(!root.exists());
    }

    #[test]
    fn preserves_unrelated_entries_and_the_root() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);
        fs::write(root.join("notes.txt"), b"mine").unwrap();
        let mut lock = StateLock::acquire(&root, false).unwrap();
        let removal = remove_local(&mut lock).unwrap();
        assert!(removal.failures.is_empty(), "{removal:?}");
        assert!(!removal.root_removed);
        assert!(root.join("notes.txt").exists());
        assert!(!root.join("wrapper.json").exists());
        assert!(!layout::profile_dir(&root).exists());
        assert!(removal.describe().contains("does not own"));
    }

    /// The regression for recursive `Storage::remove`: an unrelated file inside
    /// `sdk/default` (or `sdk/`) must survive a deletion that otherwise
    /// succeeds, and the report must say the directory was retained.
    #[test]
    fn preserves_unrelated_entries_inside_the_sdk_directories() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);
        let profile = layout::profile_dir(&root);
        let sdk = layout::sdk_root(&root);
        fs::write(profile.join("my-notes.txt"), b"keep me").unwrap();
        fs::write(sdk.join("scratch.log"), b"keep me too").unwrap();

        let mut lock = StateLock::acquire(&root, false).unwrap();
        let removal = remove_local(&mut lock).unwrap();

        // Retaining unrelated entries is not an operational failure.
        assert!(removal.failures.is_empty(), "{removal:?}");
        assert!(!removal.root_removed);
        // Unknown entries survive, together with their parent directories.
        assert_eq!(fs::read(profile.join("my-notes.txt")).unwrap(), b"keep me");
        assert_eq!(fs::read(sdk.join("scratch.log")).unwrap(), b"keep me too");
        // Every recognized credential file is gone.
        for name in [
            layout::TOKEN,
            layout::PRIVATE_KEY,
            layout::CERTIFICATE,
            layout::CHAIN,
            layout::PENDING,
            layout::TUNNEL,
        ] {
            assert!(!profile.join(name).exists(), "{name} should be removed");
        }
        assert!(!root.join(layout::WRAPPER_FILE).exists());
        let described = removal.describe();
        assert!(described.contains("retained"), "{described}");
        assert!(described.contains("my-notes.txt"), "{described}");
    }

    #[test]
    fn removes_owned_residue() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);
        let profile = layout::profile_dir(&root);
        fs::write(profile.join("token.tmp-42"), b"partial").unwrap();
        fs::write(root.join("wrapper.json.tmp-42-abcdef"), b"{}").unwrap();
        let mut lock = StateLock::acquire(&root, false).unwrap();
        let removal = remove_local(&mut lock).unwrap();
        assert!(removal.root_removed, "{removal:?}");
        assert!(!root.exists());
    }

    /// The recovery gap: when removing an owned entry *fails*, the wrapper must
    /// survive. Removing it would leave SDK files with no ownership record,
    /// which the classifier can only call corrupt — a state the printed
    /// `delete` retry could never repair.
    ///
    /// The failure is injected at the deletion primitive rather than produced
    /// with `chmod 0500`: root ignores the mode bits, so a permission-based
    /// version of this test silently asserts nothing in a container.
    #[test]
    fn keeps_the_wrapper_when_an_owned_file_cannot_be_removed() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);

        let mut lock = StateLock::acquire(&root, false).unwrap();
        let removal = {
            let _injected = InjectedUnlinkFailure::arm(layout::TOKEN);
            remove_local(&mut lock).unwrap()
        };

        assert!(
            !removal.failures.is_empty(),
            "an unremovable owned file must be reported, got {removal:?}"
        );
        assert!(!removal.root_removed);
        // The wrapper must still be there, so the directory stays identifiable
        // and `delete` can resume.
        let wrapper = fs::read_to_string(root.join(layout::WRAPPER_FILE))
            .expect("the wrapper must survive a failed local deletion");
        assert!(wrapper.contains("portable-opentunnel"), "{wrapper}");
        // The lock must also survive: removing it would let a concurrent run
        // take a fresh lock in a half-deleted directory.
        assert!(
            root.join(layout::LOCK_FILE).exists(),
            "the lock must remain"
        );
        // The failed entry itself is still present, so a retry has something
        // real to finish.
        assert!(layout::profile_dir(&root).join(layout::TOKEN).exists());
        // And the directory is still recognized as ours, so the printed retry
        // command can proceed rather than being refused as corrupt.
        assert!(
            !matches!(
                state::classify(&root).unwrap(),
                crate::state::Classification::Corrupt { .. }
            ),
            "a failed deletion must stay recoverable"
        );
    }

    /// Once the injected failure is gone, the very same directory deletes
    /// cleanly: the recorded failure is the only thing that blocked it.
    #[test]
    fn a_retry_after_the_failure_completes_the_deletion() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);

        let mut lock = StateLock::acquire(&root, false).unwrap();
        {
            let _injected = InjectedUnlinkFailure::arm(layout::TOKEN);
            let failed = remove_local(&mut lock).unwrap();
            assert!(!failed.failures.is_empty(), "{failed:?}");
        }
        lock.validate().unwrap();
        let removal = remove_local(&mut lock).unwrap();
        assert!(removal.failures.is_empty(), "{removal:?}");
        assert!(removal.root_removed, "{removal:?}");
        assert!(!root.exists());
    }

    /// Unknown files are retained but are not a failure: the owned entries are
    /// gone, so removing the wrapper is correct and the leftovers are simply
    /// not ours.
    #[test]
    fn an_unknown_file_inside_the_profile_does_not_block_wrapper_removal() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);
        let profile = layout::profile_dir(&root);
        fs::write(profile.join("my-notes.txt"), b"keep me").unwrap();

        let mut lock = StateLock::acquire(&root, false).unwrap();
        let removal = remove_local(&mut lock).unwrap();

        assert!(removal.failures.is_empty(), "{removal:?}");
        assert_eq!(fs::read(profile.join("my-notes.txt")).unwrap(), b"keep me");
        for name in [layout::TOKEN, layout::PRIVATE_KEY, layout::TUNNEL] {
            assert!(!profile.join(name).exists(), "{name} should be gone");
        }
        assert!(!root.join(layout::WRAPPER_FILE).exists());
    }

    #[test]
    fn refuses_without_the_lock() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        populated(&root);
        let mut lock = StateLock::acquire(&root, false).unwrap();
        fs::remove_file(root.join(layout::LOCK_FILE)).unwrap();
        let error = remove_local(&mut lock).unwrap_err();
        assert!(error.message.contains("lock"), "{}", error.message);
    }

    #[test]
    fn profile_constant_matches_layout() {
        assert_eq!(PROFILE, layout::PROFILE);
    }

    #[test]
    fn shell_quoting_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("/tmp/a-b"), "/tmp/a-b");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("/tmp/a'b"), r#"'/tmp/a'\''b'"#);
    }
}
