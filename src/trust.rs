//! The SDK's trust settings for the control plane.
//!
//! The SDK never disables TLS verification. It trusts the bundled Mozilla
//! roots, the OS store (which honors `SSL_CERT_FILE` and `SSL_CERT_DIR`), and
//! a PEM file given by `--ca-cert` or `OPENTUNNEL_CA_CERT`. Proxy settings
//! come from the environment: reqwest and the bridge code honor
//! `HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` (and lowercase forms).

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// The variables the pinned SDK actually honors. Documented as such in the
/// README; the SDK does not read `OPENTUNNEL_API`.
pub const CA_ENV: &str = "OPENTUNNEL_CA_CERT";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trust {
    /// PEM file of extra roots; `None` lets the SDK apply its own default.
    pub ca_cert: Option<PathBuf>,
}

impl Trust {
    /// Resolves the CA setting for a new client.
    ///
    /// `cli` comes from `--ca-cert`. When it is absent the SDK reads
    /// `OPENTUNNEL_CA_CERT` itself, so this returns `None` and the
    /// environment keeps working. A file named explicitly on the command line
    /// is checked up front, because a silent fallback to the public roots
    /// would hide a typo until certificate validation failed.
    /// Resolves the CA setting for a new client.
    ///
    /// `cli` comes from `--ca-cert`. When it is absent the SDK would read
    /// `OPENTUNNEL_CA_CERT` itself, but it *silently ignores* a file it cannot
    /// read or that holds no certificates, so the variable is resolved and
    /// validated here as well and then passed to the SDK explicitly. Either
    /// way a bad file becomes a clear startup error instead of an opaque TLS
    /// failure later (audit §6.3).
    pub fn resolve(cli: Option<PathBuf>) -> AppResult<Self> {
        let chosen = match cli {
            Some(path) => Some(path),
            None => Self::env_ca_cert(),
        };
        if let Some(path) = &chosen {
            Self::validate(path)?;
        }
        Ok(Self { ca_cert: chosen })
    }

    /// Checks a CA file up front, with the SDK's own public reader.
    ///
    /// The SDK logs and ignores an unreadable or empty CA file, then continues
    /// with the bundled and OS roots, so a typo only shows up later as an
    /// opaque TLS failure. Validating here turns that into a clear startup
    /// error (audit §6.3).
    pub fn validate(path: &Path) -> AppResult<()> {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            AppError::fail(format!("CA certificate {}: {error}", path.display()))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::fail(format!(
                "CA certificate {} is not a regular file",
                path.display()
            )));
        }
        opentunnel::tls::read_ca_certificates(path).map_err(|error| {
            AppError::fail(format!("CA certificate {}: {error}", path.display()))
        })?;
        Ok(())
    }

    /// The environment value the SDK would use, for recovery hints.
    pub fn env_ca_cert() -> Option<PathBuf> {
        std::env::var_os(CA_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    /// Whether proxy variables are set, for a diagnostic line. The values are
    /// never printed: they can carry credentials.
    pub fn proxy_env_present() -> Vec<&'static str> {
        const NAMES: &[&str] = &[
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        NAMES
            .iter()
            .copied()
            .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    #[test]
    fn accepts_an_existing_file() {
        let temp = TempDir::new();
        let path = temp.path().join("ca.pem");
        // A real certificate: validation parses the PEM, so a bare header
        // is not enough.
        std::fs::write(&path, self_signed_ca_pem()).unwrap();
        assert_eq!(
            Trust::resolve(Some(path.clone())).unwrap().ca_cert,
            Some(path)
        );
    }

    /// A genuine self-signed CA certificate, PEM-encoded.
    fn self_signed_ca_pem() -> String {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.self_signed(&key).unwrap().pem()
    }

    #[test]
    fn rejects_a_missing_file_early() {
        let temp = TempDir::new();
        let error = Trust::resolve(Some(temp.path().join("absent.pem"))).unwrap_err();
        assert!(
            error.message.contains("CA certificate"),
            "{}",
            error.message
        );
    }

    #[test]
    fn rejects_a_file_without_certificates() {
        let temp = TempDir::new();
        let path = temp.path().join("garbage.pem");
        std::fs::write(&path, b"not a certificate").unwrap();
        let error = Trust::resolve(Some(path)).unwrap_err();
        assert!(
            error.message.contains("CA certificate"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("no PEM certificates") || error.message.contains("invalid CA"),
            "{}",
            error.message
        );
    }

    #[test]
    fn rejects_a_symlinked_ca_file() {
        let temp = TempDir::new();
        let real = temp.path().join("real.pem");
        std::fs::write(&real, b"-----BEGIN CERTIFICATE-----\n").unwrap();
        let link = temp.path().join("link.pem");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = Trust::resolve(Some(link)).unwrap_err();
        assert!(
            error.message.contains("not a regular file"),
            "{}",
            error.message
        );
    }

    #[test]
    fn rejects_a_directory() {
        let temp = TempDir::new();
        let error = Trust::resolve(Some(temp.path().to_path_buf())).unwrap_err();
        assert!(error.message.contains("not a regular file"));
    }

    #[test]
    fn absent_option_keeps_sdk_default() {
        assert_eq!(Trust::resolve(None).unwrap().ca_cert, None);
    }
}
