//! The thin layer between this program and the pinned SDK.
//!
//! Everything the wrapper needs from `opentunnel` is concentrated here:
//! building a `Client` with an explicit endpoint and an explicitly located
//! `Storage`, provisioning or resuming one profile, and rendering SDK errors
//! with the right exit status.

use std::path::Path;

use opentunnel::{
    Client, ClientOptions, Error as SdkError, Identity, PendingIdentity, ProvisionStage, Storage,
};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::layout;

/// Builds a client that stores exactly one profile under `<state>/sdk`.
pub fn client(api: &str, state_root: &Path, ca_cert: Option<&Path>) -> AppResult<Client> {
    let api = Url::parse(api)
        .map_err(|error| AppError::fail(format!("invalid API endpoint '{api}': {error}")))?;
    Ok(Client::new(ClientOptions {
        api: Some(api),
        storage: Some(Storage::at(layout::sdk_root(state_root))),
        ca_cert: ca_cert.map(Path::to_path_buf),
    }))
}

/// The three states provisioning can be in after looking at the SDK files.
#[derive(Debug)]
pub enum Provisioned {
    /// A complete identity is on disk.
    Complete(Identity),
    /// A pending identity whose certificate was never issued.
    Pending(PendingIdentity),
    /// Nothing at all: the profile must be created.
    Absent,
}

pub fn inspect(client: &Client) -> AppResult<Provisioned> {
    match client.get(layout::PROFILE) {
        Ok(Some(identity)) => Ok(Provisioned::Complete(identity)),
        Ok(None) => match client.pending(layout::PROFILE) {
            Ok(Some(pending)) => Ok(Provisioned::Pending(pending)),
            Ok(None) => Ok(Provisioned::Absent),
            Err(error) => Err(sdk_error("reading the pending identity", error)),
        },
        Err(error) => Err(sdk_error("reading the saved identity", error)),
    }
}

/// Progress messages for provisioning, written to stderr only.
pub fn describe(stage: ProvisionStage) -> &'static str {
    match stage {
        ProvisionStage::CreatingTunnel => "creating the tunnel registration",
        ProvisionStage::GeneratingKey => "generating the local key",
        ProvisionStage::GeneratingCsr => "preparing the certificate request",
        ProvisionStage::ResumingCertificate => "resuming the saved certificate request",
        ProvisionStage::RequestingCertificate => "requesting the certificate",
        ProvisionStage::WaitingCertificate => "waiting for the certificate (this can take minutes)",
        ProvisionStage::SavingIdentity => "saving the identity",
        ProvisionStage::Ready => "identity ready",
    }
}

/// Whether an SDK error means the saved credentials are unusable and the
/// identity must not be replaced silently.
pub fn is_credential_rejection(error: &SdkError) -> bool {
    match error {
        SdkError::Api { status, .. } => matches!(status, 401 | 403 | 404),
        SdkError::Attach(code) => code == opentunnel::protocol::bridge::codes::BAD_TOKEN,
        _ => false,
    }
}

/// Renders an SDK error as an `AppError`, keeping operational failures at 1.
///
/// The SDK's `Display` for transport errors is safe, but [`SdkError::Api`]
/// embeds the server's own `message` field, which is untrusted text: a fixture
/// or a misbehaving endpoint can echo the request (and therefore the saved
/// token) straight back. Diagnostics must never carry secrets, so API failures
/// are rendered from the status code and our own fixed wording only.
pub fn sdk_error(context: &str, error: SdkError) -> AppError {
    let hint = if is_credential_rejection(&error) {
        " The saved identity was rejected or no longer exists; it will not be replaced \
         automatically. Use delete to remove it, then run again to create a new identity \
         and URL."
    } else {
        ""
    };
    AppError::fail(format!("{context}: {}{hint}", describe_sdk_error(&error)))
}

/// A secret-free description of an SDK error.
fn describe_sdk_error(error: &SdkError) -> String {
    match error {
        // Never the server's `message` or `tag`: both are untrusted and may
        // quote the request.
        SdkError::Api { status, .. } => format!("the API answered with HTTP status {status}"),
        other => other.to_string(),
    }
}

/// Serializable view of a pending identity, for diagnostics that must never
/// include the token, key, or CSR.
#[derive(Debug, serde::Serialize)]
pub struct PendingSummary {
    pub id: String,
    pub hostname: String,
}

impl From<&PendingIdentity> for PendingSummary {
    fn from(pending: &PendingIdentity) -> Self {
        Self {
            id: pending.id.clone(),
            hostname: pending.hostname.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    #[test]
    fn client_uses_the_state_directory() {
        let temp = TempDir::new();
        let client = client("http://127.0.0.1:1", temp.path(), None).unwrap();
        assert_eq!(client.storage().root(), layout::sdk_root(temp.path()));
        assert_eq!(client.get(layout::PROFILE).unwrap(), None);
    }

    #[test]
    fn inspect_reports_absent_then_pending() {
        let temp = TempDir::new();
        let client = client("http://127.0.0.1:1", temp.path(), None).unwrap();
        assert!(matches!(inspect(&client).unwrap(), Provisioned::Absent));

        let pending = PendingIdentity {
            id: "abc".into(),
            hostname: "abc.test".into(),
            token: "secret-token".into(),
            private_key: "KEY".into(),
            csr: "CSR".into(),
        };
        client
            .storage()
            .save_pending(layout::PROFILE, &pending)
            .unwrap();
        match inspect(&client).unwrap() {
            Provisioned::Pending(found) => assert_eq!(found.id, "abc"),
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn credential_rejections_are_recognized() {
        assert!(is_credential_rejection(&SdkError::Api {
            status: 404,
            tag: "NotFoundError".into(),
            message: String::new(),
        }));
        assert!(is_credential_rejection(&SdkError::Attach(
            "bad_token".into()
        )));
        assert!(!is_credential_rejection(&SdkError::Api {
            status: 500,
            tag: "InternalError".into(),
            message: String::new(),
        }));
        assert!(!is_credential_rejection(&SdkError::Invalid("x".into())));
    }

    #[test]
    fn pending_summary_omits_secrets() {
        let pending = PendingIdentity {
            id: "abc".into(),
            hostname: "abc.test".into(),
            token: "secret-token".into(),
            private_key: "KEY".into(),
            csr: "CSR".into(),
        };
        let json = serde_json::to_string(&PendingSummary::from(&pending)).unwrap();
        assert!(!json.contains("secret-token"));
        assert!(!json.contains("KEY"));
        assert!(!json.contains("CSR"));
        assert!(json.contains("abc.test"));
    }

    /// The API's own `message` is untrusted: an endpoint (or a fixture) can
    /// echo the bearer token back in an error body. Diagnosed errors must never
    /// reproduce it.
    #[test]
    fn api_errors_never_echo_the_request_or_its_token() {
        let token = "token-abcdef123456";
        let error = SdkError::Api {
            status: 401,
            tag: "UnauthorizedError".into(),
            message: format!("rejected bearer {token} for this tunnel"),
        };
        let rendered = sdk_error("deleting the tunnel registration", error);
        assert!(
            !rendered.message.contains(token),
            "token leaked: {}",
            rendered.message
        );
        assert!(
            !rendered.message.contains("UnauthorizedError"),
            "server tag leaked: {}",
            rendered.message
        );
        assert!(!rendered.message.contains("rejected bearer"));
        // It still says something useful, and it keeps the documented hint.
        assert!(rendered.message.contains("HTTP status 401"));
        assert!(rendered.message.contains("will not be replaced"));
        assert_eq!(rendered.code, crate::error::FAILURE);
    }

    #[test]
    fn non_api_errors_keep_their_own_message() {
        // Transport and validation errors are ours or the transport's, and they
        // never carry request bodies, so their text stays available.
        let rendered = sdk_error("starting the bridge", SdkError::Attach("bad_token".into()));
        assert!(rendered.message.contains("bad_token"));
        assert!(rendered.message.contains("starting the bridge"));
    }
}
