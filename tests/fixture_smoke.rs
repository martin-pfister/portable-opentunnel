//! Fixture self-tests: the fake API must be correct before any lifecycle test
//! can be trusted.
//!
//! These deliberately avoid the SDK's provisioning path, because that writes
//! `private-key.pem`/`certificate.pem`. They cover the HTTP protocol, the CSR
//! signing, and the backend directly, so a fixture bug is caught here rather
//! than as a confusing child-process failure later.

#[path = "support/fake_api.rs"]
mod fake_api;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fake_api::FakeApi;

fn runtime() -> tokio::runtime::Runtime {
    installer::install();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// `reqwest` is built with `rustls-no-provider`, so the process default must be
/// installed before the first client (and before any rustls `ServerConfig`).
mod installer {
    use std::sync::Once;

    static ONCE: Once = Once::new();

    pub fn install() {
        ONCE.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok();
        });
    }
}

/// An HTTP client that never goes through an environment proxy, so a loopback
/// fixture is reached directly even when `HTTPS_PROXY` is set.
fn http() -> reqwest::Client {
    installer::install();
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("building the test HTTP client")
}

/// A real CSR, as the SDK would submit, without touching the filesystem.
fn csr_for(hostname: &str) -> String {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec![hostname.to_owned()]).unwrap();
    params.serialize_request(&key).unwrap().pem().unwrap()
}

#[test]
fn the_protocol_create_bind_ready_delete_cycle_works() {
    let api = FakeApi::start_in_memory(None);
    let rt = runtime();
    let base = api.api_url();

    let (id, token, hostname) = rt.block_on(async {
        let http = http();
        // 1. Create.
        let created: serde_json::Value = http
            .post(format!("{base}/api/tunnel"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("create request")
            .json()
            .await
            .expect("create response");
        let id = created["tunnel"]["id"].as_str().unwrap().to_owned();
        let hostname = created["tunnel"]["hostname"].as_str().unwrap().to_owned();
        let token = created["token"].as_str().unwrap().to_owned();

        // 2. Bind the CSR, which the fixture signs.
        let csr = csr_for(&hostname);
        let bound: serde_json::Value = http
            .post(format!("{base}/api/tunnel/{id}/certificate"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "csr": csr }))
            .send()
            .await
            .expect("bind request")
            .json()
            .await
            .expect("bind response");
        assert_eq!(bound["state"]["type"], "ready", "{bound}");
        assert!(
            bound["state"]["certificate"]
                .as_str()
                .unwrap()
                .contains("BEGIN CERTIFICATE"),
            "{bound}"
        );

        // 3. Poll it back: the same certificate is returned.
        let polled: serde_json::Value = http
            .get(format!("{base}/api/tunnel/{id}/certificate"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("poll request")
            .json()
            .await
            .expect("poll response");
        assert_eq!(polled["state"]["type"], "ready", "{polled}");
        assert_eq!(
            polled["state"]["certificate"],
            bound["state"]["certificate"]
        );

        // 4. Delete, then confirm the registration is gone.
        let deleted = http
            .delete(format!("{base}/api/tunnel/{id}"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("delete request");
        assert_eq!(deleted.status(), 204);

        let gone = http
            .get(format!("{base}/api/tunnel/{id}"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("get after delete");
        assert_eq!(gone.status(), 401, "a deleted tunnel must not authorize");
        (id, token, hostname)
    });

    assert!(!id.is_empty() && !token.is_empty());
    assert!(hostname.ends_with(".example.test"), "{hostname}");
    assert_eq!(api.creates(), 1);
    assert_eq!(api.binds(), 1);
    assert_eq!(api.deletes(), 1);
    assert!(api.all_deleted());
}

#[test]
fn an_unparseable_csr_is_a_client_error_not_a_panic() {
    let api = FakeApi::start_in_memory(None);
    let rt = runtime();
    let base = api.api_url();

    rt.block_on(async {
        let http = http();
        let created: serde_json::Value = http
            .post(format!("{base}/api/tunnel"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["tunnel"]["id"].as_str().unwrap().to_owned();
        let token = created["token"].as_str().unwrap().to_owned();

        let response = http
            .post(format!("{base}/api/tunnel/{id}/certificate"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "csr": "-----BEGIN CERTIFICATE REQUEST-----\nnot a csr\n-----END CERTIFICATE REQUEST-----\n" }))
            .send()
            .await
            .expect("bind request");
        assert_eq!(response.status(), 400, "a bad CSR must be rejected");
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["_tag"], "InvalidCsrError", "{body}");
    });
}

#[test]
fn certificate_failure_and_delete_failure_are_configurable() {
    let api = FakeApi::start_in_memory(None);
    api.change(|behaviour| {
        behaviour.certificate_failure = Some("ca said no".to_owned());
        behaviour.delete_status = Some(500);
    });
    let rt = runtime();
    let base = api.api_url();

    rt.block_on(async {
        let http = http();
        let created: serde_json::Value = http
            .post(format!("{base}/api/tunnel"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["tunnel"]["id"].as_str().unwrap().to_owned();
        let token = created["token"].as_str().unwrap().to_owned();

        let polled: serde_json::Value = http
            .get(format!("{base}/api/tunnel/{id}/certificate"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(polled["state"]["type"], "failed", "{polled}");
        assert_eq!(polled["state"]["reason"], "ca said no");

        let deleted = http
            .delete(format!("{base}/api/tunnel/{id}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(deleted.status(), 500, "the delete knob must be honoured");
    });
}

#[test]
fn an_unknown_tunnel_needs_no_valid_token_to_404() {
    let api = FakeApi::start_in_memory(None);
    let rt = runtime();
    let base = api.api_url();
    rt.block_on(async {
        let http = http();
        let response = http
            .delete(format!("{base}/api/tunnel/does-not-exist"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    });
}

#[test]
fn a_delete_needs_the_tunnel_token() {
    let api = FakeApi::start_in_memory(None);
    let rt = runtime();
    let base = api.api_url();
    rt.block_on(async {
        let http = http();
        let created: serde_json::Value = http
            .post(format!("{base}/api/tunnel"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["tunnel"]["id"].as_str().unwrap().to_owned();
        let response = http
            .delete(format!("{base}/api/tunnel/{id}"))
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    });
    assert_eq!(api.deletes(), 0);
    assert!(!api.all_deleted());
}

#[test]
fn backend_answers_a_plain_http_request() {
    let counter = Arc::new(AtomicUsize::new(0));
    let rt = runtime();
    let requests = Arc::clone(&counter);
    let body = rt.block_on(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(fake_api::serve_backend(listener, requests));
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: backend\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    });

    assert!(body.contains("200 OK"), "{body}");
    assert!(body.contains(fake_api::BACKEND_BODY), "{body}");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "one backend request");
}

#[test]
fn the_ca_signs_and_can_stay_in_memory() {
    let ca = fake_api::TestCa::in_memory();
    assert!(!ca.has_file(), "the in-memory CA writes nothing");
    let (leaf, chain, expiry) = ca
        .sign_csr(&csr_for("demo.example.test"), "demo.example.test")
        .expect("an in-memory CA must sign a CSR");
    assert!(leaf.contains("BEGIN CERTIFICATE"));
    assert!(chain.contains("BEGIN CERTIFICATE"));
    assert_eq!(expiry, "2035-01-01T00:00:00.000Z");

    // A malformed CSR is reported, never a panic.
    let error = ca.sign_csr("nonsense", "demo.example.test").unwrap_err();
    assert!(error.contains("unparseable"), "{error}");
}

#[test]
fn the_fixture_stays_within_loopback() {
    let api = FakeApi::start_in_memory(None);
    assert!(
        api.api_url().contains("127.0.0.1"),
        "the fixture must never bind a routable address: {}",
        api.api_url()
    );
}
