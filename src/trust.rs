//! The extra CA certificate for the control plane.
//!
//! The SDK never disables TLS verification. It trusts the bundled Mozilla
//! roots, the OS store (which honors `SSL_CERT_FILE` and `SSL_CERT_DIR`), and
//! a PEM file given by `--ca-cert` or `OPENTUNNEL_CA_CERT`. Proxy settings
//! come from the environment: reqwest and the bridge code honor
//! `HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` (and lowercase forms).

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// Environment fallback for the extra control-plane CA file.
const CA_ENV: &str = "OPENTUNNEL_CA_CERT";

/// Resolves and validates the extra CA file: `--ca-cert`, else
/// `OPENTUNNEL_CA_CERT`, else none.
///
/// The SDK would read the variable itself, but it logs and ignores a file it
/// cannot read or that holds no certificates, so a typo would only show up
/// later as an opaque TLS failure. Resolving and checking it here turns that
/// into a clear startup error, and the result is passed to the SDK explicitly.
///
/// The path is made absolute because it is also printed in retry commands,
/// which may be run from another directory.
pub fn resolve_ca_cert(cli: Option<PathBuf>) -> AppResult<Option<PathBuf>> {
    let chosen = cli.or_else(|| {
        std::env::var_os(CA_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let Some(path) = chosen else {
        return Ok(None);
    };
    validate(&path)?;
    Ok(Some(std::path::absolute(&path).unwrap_or(path)))
}

fn validate(path: &Path) -> AppResult<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::fail(format!("CA certificate {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(AppError::fail(format!(
            "CA certificate {} is not a regular file",
            path.display()
        )));
    }
    opentunnel::tls::read_ca_certificates(path)
        .map_err(|error| AppError::fail(format!("CA certificate {}: {error}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    #[test]
    fn accepts_a_real_certificate() {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let pem = params.self_signed(&key).unwrap().pem();

        let temp = TempDir::new();
        let path = temp.path().join("ca.pem");
        std::fs::write(&path, pem).unwrap();
        assert_eq!(resolve_ca_cert(Some(path.clone())).unwrap(), Some(path));
    }

    #[test]
    fn rejects_a_missing_file_early() {
        let temp = TempDir::new();
        let error = resolve_ca_cert(Some(temp.path().join("absent.pem"))).unwrap_err();
        assert!(error.message.contains("CA certificate"), "{error}");
    }

    #[test]
    fn rejects_a_file_without_certificates() {
        let temp = TempDir::new();
        let path = temp.path().join("garbage.pem");
        std::fs::write(&path, b"not a certificate").unwrap();
        let error = resolve_ca_cert(Some(path)).unwrap_err();
        assert!(error.message.contains("CA certificate"), "{error}");
        assert!(
            error.message.contains("no PEM certificates") || error.message.contains("invalid CA"),
            "{error}"
        );
    }

    #[test]
    fn rejects_symlinks_and_directories() {
        let temp = TempDir::new();
        let real = temp.path().join("real.pem");
        std::fs::write(&real, b"-----BEGIN CERTIFICATE-----\n").unwrap();
        let link = temp.path().join("link.pem");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        for path in [link, temp.path().to_path_buf()] {
            let error = resolve_ca_cert(Some(path)).unwrap_err();
            assert!(error.message.contains("not a regular file"), "{error}");
        }
    }
}
