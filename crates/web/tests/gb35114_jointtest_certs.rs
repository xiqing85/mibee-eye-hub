//! Pins that a freshly openssl-generated SM2 identity (as opposed to the
//! lib-inherited fixtures) loads through the product builder — guards the
//! parser against drift in openssl's PKCS#8 output. Fixture from the
//! 2026-10-22 NVR A-level joint test (test-only key, safe to commit).
use web::protocol_runtime::{Gb35114RuntimeConfig, build_gb35114_authenticator};

#[test]
fn joint_test_sm2_identity_builds() {
    let dir = format!("{}/tests/fixtures/gb35114-jt", env!("CARGO_MANIFEST_DIR"));
    let cfg = Gb35114RuntimeConfig {
        enabled: true,
        device_cert_file: format!("{dir}/device_cert.pem"),
        device_key_file: format!("{dir}/device_key.pem"),
        platform_cert_file: format!(
            "{}/tests/fixtures/gb35114/platform_cert.pem",
            env!("CARGO_MANIFEST_DIR")
        ),
        server_id: "34020000002000000001".into(),
    };
    let auth = build_gb35114_authenticator(&cfg, "34020000001320000136")
        .expect("joint-test identity must build")
        .expect("enabled must yield an authenticator");
    assert!(
        auth.initial_authorization()
            .starts_with("Capability algorithm=\"A:SM2")
    );
}
