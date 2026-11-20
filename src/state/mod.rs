//! The private state directory: its layout, metadata, classification, lock,
//! and the rules for opening it.
//!
//! Every rule here exists because credentials live in the directory.

mod classify;
pub mod directory;
pub mod layout;
mod lock;
mod open;
mod private;

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

pub use classify::{Classification, classify};
pub use lock::StateLock;
pub use open::Prepared;
pub use private::check_private_dir;
#[cfg(test)]
pub use private::{create_private_dir, current_uid};

const WRAPPER_FORMAT: u64 = 1;
const OWNER_MARKER: &str = "portable-opentunnel";

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
    /// deletion can be resumed without contacting the API again, which it
    /// could not do once `token`/`private-key.pem` are partly gone. Absent in
    /// older metadata, and omitted when false so a live identity's file is
    /// unchanged.
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

    /// Refuses an `--api` override that differs from the recorded endpoint: a
    /// saved identity only exists on the service that created it.
    pub fn check_api(&self, requested: Option<&str>) -> AppResult<()> {
        let origin = |api: &str| url::Url::parse(api).map(|url| url.origin().ascii_serialization());
        match requested {
            Some(requested) if origin(requested) != origin(&self.api) => {
                Err(AppError::fail(format!(
                    "the state directory was created for {}, but --api {requested} was given; \
                 omit --api or pass the recorded endpoint",
                    self.api
                )))
            }
            _ => Ok(()),
        }
    }
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
        crate::random::alphanumeric(8)
    ));
    private::create_new_private(&temporary, &content)?;
    let final_path = root.join(layout::WRAPPER_FILE);
    if let Err(error) = fs::rename(&temporary, &final_path) {
        let _ = fs::remove_file(&temporary);
        return Err(AppError::io(format!("replacing {}", final_path.display()))(
            error,
        ));
    }
    private::fsync_dir(root)
}

/// Atomically records that the remote registration is gone.
///
/// This must succeed *before* any credential file is unlinked: it is the only
/// thing that makes an interrupted local deletion resumable. A failure leaves
/// the credentials untouched and the caller must not remove anything.
pub fn mark_remote_deleted(root: &Path, meta: &WrapperMeta) -> AppResult<()> {
    let mut marked = meta.clone();
    marked.remote_deleted = true;
    write_wrapper(root, &marked)
}

/// The public HTTPS URL for a route and tunnel hostname.
pub fn route_url(route: &str, hostname: &str) -> String {
    if route == opentunnel::protocol::names::ROOT_ROUTE {
        format!("https://{hostname}")
    } else {
        format!("https://{route}.{hostname}")
    }
}

/// The command that finishes an interrupted deletion of `root`.
///
/// `ca_cert` matters only while the retry still has to contact the API; once
/// the deletion is recorded, `delete` finishes offline.
pub fn recovery_command(root: &Path, ca_cert: Option<&Path>) -> String {
    let mut command = format!(
        "portable-opentunnel delete --dir {}",
        shell_quote(&root.display().to_string())
    );
    if let Some(ca_cert) = ca_cert {
        command.push_str(" --ca-cert ");
        command.push_str(&shell_quote(&ca_cert.display().to_string()));
    }
    command
}

fn shell_quote(value: &str) -> String {
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

/// The saved URL, for the read-only `url` command.
///
/// Strictly local: it never creates the directory or takes the lock, and reads
/// the wrapper route and the SDK's `tunnel.json` hostname only, never a
/// credential. The SDK writes `tunnel.json` last by atomic rename, so its
/// presence is the completion marker. Fails with [`crate::error::NO_TUNNEL`]
/// when nothing usable is initialized.
pub fn saved_url(directory: &Path) -> AppResult<String> {
    let root = &directory::absolutize(directory);
    match classify(root)? {
        Classification::Initialized { meta, hostname } => Ok(route_url(&meta.route, &hostname)),
        Classification::FirstRun { .. } | Classification::Resumable { .. } => Err(
            AppError::no_tunnel(format!("No tunnel initialized in {}.", root.display())),
        ),
        Classification::Deleting { .. } => Err(AppError::no_tunnel(format!(
            "The tunnel in {} was deleted; only local cleanup remains. Finish it with: {}",
            root.display(),
            recovery_command(root, None)
        ))),
        Classification::Corrupt { reason } | Classification::Unrelated { reason } => {
            Err(AppError::fail(reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    use crate::error::{FAILURE, NO_TUNNEL};
    use crate::test_support::TempDir;

    #[test]
    fn shell_quoting_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("/tmp/a-b"), "/tmp/a-b");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("/tmp/a'b"), r#"'/tmp/a'\''b'"#);
    }

    #[test]
    fn the_recovery_command_carries_the_ca_only_when_given() {
        let root = Path::new("/srv/my tunnel");
        assert_eq!(
            recovery_command(root, None),
            "portable-opentunnel delete --dir '/srv/my tunnel'"
        );
        assert_eq!(
            recovery_command(root, Some(Path::new("/etc/ca.pem"))),
            "portable-opentunnel delete --dir '/srv/my tunnel' --ca-cert /etc/ca.pem"
        );
    }

    fn reusable(route: &str) -> WrapperMeta {
        WrapperMeta::new(Mode::Reusable, DEFAULT_API.to_owned(), route.to_owned())
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn initialized(root: &Path, route: &str, hostname: &str) {
        create_private_dir(root).unwrap();
        write_wrapper(root, &reusable(route)).unwrap();
        let profile = layout::profile_dir(root);
        fs::create_dir_all(&profile).unwrap();
        for name in [layout::TOKEN, layout::PRIVATE_KEY, layout::CHAIN] {
            fs::write(profile.join(name), b"secret").unwrap();
        }
        fs::write(
            profile.join(layout::TUNNEL),
            format!(
                r#"{{"id":"abc","hostname":"{hostname}","certificateExpiry":"2099-01-01T00:00:00.000Z"}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn wrapper_writes_are_private_and_leave_no_residue() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        write_wrapper(&root, &reusable("abc123")).unwrap();
        let before = fs::read(root.join(layout::WRAPPER_FILE)).unwrap();

        mark_remote_deleted(&root, &reusable("abc123")).unwrap();

        let after = fs::read_to_string(root.join(layout::WRAPPER_FILE)).unwrap();
        assert_ne!(before, after.as_bytes());
        assert!(after.contains("\"remote_deleted\": true"), "{after}");
        assert!(after.contains("abc123"), "the route must be preserved");
        assert_eq!(mode_of(&root.join(layout::WRAPPER_FILE)), 0o600);
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [layout::WRAPPER_FILE], "residue left behind");
    }

    #[test]
    fn a_conflicting_endpoint_is_refused() {
        let meta = reusable("abc");
        meta.check_api(None).unwrap();
        meta.check_api(Some(DEFAULT_API)).unwrap();
        let error = meta.check_api(Some("http://127.0.0.1:2")).unwrap_err();
        assert!(error.message.contains("--api"), "{error}");
    }

    #[test]
    fn an_endpoint_saved_with_a_trailing_slash_matches_its_origin() {
        let meta = WrapperMeta::new(
            Mode::Reusable,
            "https://Tunnel.Example/".to_owned(),
            "abc".to_owned(),
        );
        meta.check_api(Some("https://tunnel.example")).unwrap();
    }

    #[test]
    fn route_urls_cover_root_and_label() {
        assert_eq!(route_url("@", "x.test"), "https://x.test");
        assert_eq!(route_url("abc", "x.test"), "https://abc.x.test");
    }

    #[test]
    fn saved_url_prints_the_route_and_hostname() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        initialized(&root, "abc", "abc.opentunnel.xyz");
        assert_eq!(saved_url(&root).unwrap(), "https://abc.abc.opentunnel.xyz");
    }

    #[test]
    fn saved_url_with_the_root_route_is_the_bare_hostname() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        initialized(&root, "@", "abc.opentunnel.xyz");
        assert_eq!(saved_url(&root).unwrap(), "https://abc.opentunnel.xyz");
    }

    #[test]
    fn saved_url_of_a_missing_directory_creates_nothing() {
        let temp = TempDir::new();
        let root = temp.path().join("absent");
        let error = saved_url(&root).unwrap_err();
        assert_eq!(error.code, NO_TUNNEL);
        assert!(error.message.starts_with("No tunnel initialized in"));
        assert!(!root.exists());
    }

    #[test]
    fn saved_url_is_absent_until_provisioning_completes() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        assert_eq!(saved_url(&root).unwrap_err().code, NO_TUNNEL);

        let residue = root.join("wrapper.json.tmp-1-abc");
        fs::write(&residue, b"{}").unwrap();
        assert_eq!(saved_url(&root).unwrap_err().code, NO_TUNNEL);
        assert!(residue.exists(), "url is read-only");

        fs::remove_file(&residue).unwrap();
        write_wrapper(&root, &reusable("abc")).unwrap();
        assert_eq!(saved_url(&root).unwrap_err().code, NO_TUNNEL);
    }

    #[test]
    fn saved_url_of_a_marked_deletion_is_absent() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        initialized(&root, "abc", "abc.opentunnel.xyz");
        mark_remote_deleted(&root, &reusable("abc")).unwrap();
        let error = saved_url(&root).unwrap_err();
        assert_eq!(error.code, NO_TUNNEL);
        assert!(error.message.contains("delete --dir"), "{error}");
    }

    #[test]
    fn saved_url_of_corrupt_state_is_an_operational_failure() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        fs::write(root.join(layout::WRAPPER_FILE), b"{oops").unwrap();
        assert_eq!(saved_url(&root).unwrap_err().code, FAILURE);

        let sdk_only = temp.path().join("sdk-only");
        let profile = layout::profile_dir(&sdk_only);
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join(layout::TUNNEL), b"{}").unwrap();
        assert_eq!(saved_url(&sdk_only).unwrap_err().code, FAILURE);
    }
}
