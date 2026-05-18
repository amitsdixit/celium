//! W32 — User authentication & sessions, end-to-end.
//!
//! Drives the full auth surface (`set_password`, `authenticate`,
//! `create_session`, `validate_token`, `revoke_token`,
//! `purge_expired_sessions`) through a real on-disk
//! [`celtenancy::FileTenantStore`]. The W32 mantra holds at this
//! layer: plaintext passwords never reach disk; session tokens are
//! stored only as their SHA-256 fingerprint; every credential-
//! related failure surfaces the **same** uniform error
//! (`CelError::CapabilityDenied("auth.credentials")` for
//! password paths, `"auth.session"` for token paths).

#![forbid(unsafe_code)]

use celcommon::CelError;
use celtenancy::{
    auth::SessionToken, FileTenantStore, TenantCaps, TenantQuotas, TenantSpec, TenantStore,
};

fn quotas() -> TenantQuotas {
    TenantQuotas {
        max_vcpus: 8,
        max_memory_mib: 16 * 1024,
        max_storage_bytes: 1024 * 1024 * 1024,
        max_network_mbps: 10_000,
        max_iops: 50_000,
    }
}

/// Bootstrap a store with one tenant `acme` and one user `alice`
/// with VM read+write caps. Returns the open store handle and the
/// `alice` row.
fn fixture(path: &std::path::Path) -> (FileTenantStore, celtenancy::User) {
    let s = FileTenantStore::open(path).unwrap();
    let t = s
        .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
        .unwrap();
    let u = s
        .add_user(
            t.id,
            "alice".into(),
            TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
        )
        .unwrap();
    (s, u)
}

#[test]
fn set_password_login_logout_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let (s, _alice) = fixture(&path);
    s.set_password(_alice_id(&s), _alice.id, "correct horse battery")
        .unwrap();
    // Authenticate ⇒ mint ⇒ validate ⇒ revoke.
    let (tid, uid, caps) = s
        .authenticate("acme", "alice", "correct horse battery")
        .unwrap();
    let (token, session) = s.create_session(tid, uid, caps, Some(3600)).unwrap();
    let got = s.validate_token(&token).unwrap();
    assert_eq!(got.tenant, tid);
    assert_eq!(got.user_name, "alice");
    assert_eq!(got.caps, session.caps);
    s.revoke_token(&token).unwrap();
    let err = s.validate_token(&token).unwrap_err();
    assert!(matches!(err, CelError::CapabilityDenied("auth.session")));
}

fn _alice_id(s: &FileTenantStore) -> celtenancy::TenantId {
    s.get_by_name("acme").unwrap().id
}

#[test]
fn wrong_password_yields_uniform_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let (s, alice) = fixture(&path);
    s.set_password(_alice_id(&s), alice.id, "pw").unwrap();
    let err = s.authenticate("acme", "alice", "WRONG").unwrap_err();
    assert!(matches!(
        err,
        CelError::CapabilityDenied("auth.credentials")
    ));
}

#[test]
fn unknown_tenant_yields_uniform_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let (s, alice) = fixture(&path);
    s.set_password(_alice_id(&s), alice.id, "pw").unwrap();
    let err = s.authenticate("ghost", "alice", "pw").unwrap_err();
    assert!(matches!(
        err,
        CelError::CapabilityDenied("auth.credentials")
    ));
}

#[test]
fn expired_token_rejected_uniformly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let (s, alice) = fixture(&path);
    s.set_password(_alice_id(&s), alice.id, "pw").unwrap();
    let (tid, uid, caps) = s.authenticate("acme", "alice", "pw").unwrap();
    // ttl=0 ⇒ expired immediately.
    let (token, _) = s.create_session(tid, uid, caps, Some(0)).unwrap();
    let err = s.validate_token(&token).unwrap_err();
    assert!(matches!(err, CelError::CapabilityDenied("auth.session")));
}

#[test]
fn revoked_token_rejected_uniformly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let (s, alice) = fixture(&path);
    s.set_password(_alice_id(&s), alice.id, "pw").unwrap();
    let (tid, uid, caps) = s.authenticate("acme", "alice", "pw").unwrap();
    let (token, _) = s.create_session(tid, uid, caps, Some(3600)).unwrap();
    s.revoke_token(&token).unwrap();
    let err = s.validate_token(&token).unwrap_err();
    assert!(matches!(err, CelError::CapabilityDenied("auth.session")));
    // Idempotent — revoking again is Ok.
    s.revoke_token(&token).unwrap();
}

#[test]
fn sessions_persist_across_process_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let token_hex = {
        let (s, alice) = fixture(&path);
        s.set_password(_alice_id(&s), alice.id, "pw").unwrap();
        let (tid, uid, caps) = s.authenticate("acme", "alice", "pw").unwrap();
        let (token, _) = s.create_session(tid, uid, caps, Some(3600)).unwrap();
        token.as_str().to_string()
    };
    // Close + reopen: validate token still works.
    let s2 = FileTenantStore::open(&path).unwrap();
    let token = SessionToken::from_hex(&token_hex).unwrap();
    let session = s2.validate_token(&token).unwrap();
    assert_eq!(session.user_name, "alice");
}

#[test]
fn password_hash_never_persisted_as_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let (s, alice) = fixture(&path);
    let secret = "supersecret-canary-XYZ-1234";
    s.set_password(_alice_id(&s), alice.id, secret).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains(secret),
        "plaintext password leaked into tenants.json"
    );
    // Sanity: the file does mention argon2 (the PHC string starts
    // with `$argon2id$`).
    assert!(raw.contains("$argon2id$"), "expected Argon2id PHC marker");
}

#[test]
fn token_never_persisted_as_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tenants.json");
    let (s, alice) = fixture(&path);
    s.set_password(_alice_id(&s), alice.id, "pw").unwrap();
    let (tid, uid, caps) = s.authenticate("acme", "alice", "pw").unwrap();
    let (token, _) = s.create_session(tid, uid, caps, Some(3600)).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains(token.as_str()),
        "plaintext session token leaked into tenants.json"
    );
}
