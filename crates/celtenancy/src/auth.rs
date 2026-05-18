//! W32 — User authentication & session management.
//!
//! Lives entirely inside the Tenancy Layer. The Core Layer
//! (CelHyper / CelMesh / CelVault) never sees passwords, tokens,
//! or sessions — only the attenuated [`crate::TenantCaps`] that
//! the session carries.
//!
//! ## Threat model
//!
//! * **Passwords** are hashed with Argon2id (PHC-encoded). The
//!   plaintext is never persisted and never appears in logs.
//! * **Session tokens** are 32 random bytes minted from the OS
//!   CSPRNG and rendered as 64 hex chars. The plaintext token is
//!   returned **once** by [`crate::TenantStore::create_session`]
//!   and is never re-derivable from disk — only its SHA-256 hash
//!   is stored.
//! * **Constant-time comparison.** Argon2's `verify_password` is
//!   constant-time. Token lookup hashes the candidate before the
//!   `HashMap` lookup so the timing reveals only "valid format /
//!   invalid format", not "first byte matched".
//! * **Failure messages are uniform.** Bad username, bad password,
//!   and unknown-tenant all surface as
//!   `CelError::CapabilityDenied("auth.credentials")`. Expired or
//!   revoked tokens surface as `CapabilityDenied("auth.session")`.
//!
//! ## Discipline
//!
//! `#![forbid(unsafe_code)]` at the crate root. Every fallible API
//! returns `CelResult<T>`. No `unwrap()` or `panic!()` on any
//! production path.

use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::{
    rand_core::OsRng as PhcOsRng, PasswordHash as ArgonHash, PasswordHasher, PasswordVerifier,
    SaltString,
};
use argon2::Argon2;
use celcommon::{CelError, CelResult};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::caps::TenantCaps;
use crate::tenant::TenantId;
use crate::user::UserId;

/// PHC-encoded Argon2id password hash. Persisted verbatim inside
/// the tenant store; never compared to anything except via
/// [`verify_password`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PasswordHashStr(String);

impl PasswordHashStr {
    /// PHC string for inspection (e.g. CLI debug). The struct is
    /// already serde-`transparent`, so callers normally don't need
    /// this — it exists so tests can assert that nothing looks
    /// like plaintext.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Hash a plaintext password with Argon2id and the default
/// parameters bundled with the `argon2` crate (currently
/// `m_cost = 19_456`, `t_cost = 2`, `p_cost = 1`). A fresh
/// 16-byte salt is drawn from the OS CSPRNG per call.
///
/// # Errors
///
/// * [`CelError::Invalid`] on an empty password.
/// * [`CelError::Internal`] if the underlying Argon2 implementation
///   fails to produce a hash (e.g. transient allocation failure).
pub fn hash_password(plain: &str) -> CelResult<PasswordHashStr> {
    if plain.is_empty() {
        return Err(CelError::Invalid("password must not be empty"));
    }
    let salt = SaltString::generate(&mut PhcOsRng);
    let argon = Argon2::default();
    let phc = argon
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|_| CelError::Internal("argon2 hash_password failed"))?
        .to_string();
    Ok(PasswordHashStr(phc))
}

/// Verify a plaintext password against a stored Argon2id PHC
/// string. The comparison is constant-time inside `argon2`.
///
/// Returns `Ok(())` on match; `Err(CelError::CapabilityDenied)` on
/// mismatch or malformed hash. Never logs the plaintext or hash.
///
/// # Errors
///
/// Surfaces `CelError::CapabilityDenied("auth.credentials")` on
/// any failure path so callers cannot use the error code to
/// distinguish "user unknown" from "password wrong".
pub fn verify_password(plain: &str, stored: &PasswordHashStr) -> CelResult<()> {
    let parsed = ArgonHash::new(&stored.0)
        .map_err(|_| CelError::CapabilityDenied("auth.credentials"))?;
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .map_err(|_| CelError::CapabilityDenied("auth.credentials"))?;
    Ok(())
}

/// Opaque session token. 32 random bytes from the OS CSPRNG,
/// rendered as 64 hex characters. Returned to the caller of
/// [`crate::TenantStore::create_session`] **once**; the store
/// keeps only the SHA-256 hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToken(String);

impl SessionToken {
    /// Borrow the hex string for transmission over a CLI / wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reconstruct a token from a hex string (e.g. read from a
    /// session file). Validates length + hex alphabet so callers
    /// can't feed arbitrary data into the token path.
    ///
    /// # Errors
    ///
    /// Returns `CelError::Invalid("token format")` on length /
    /// alphabet violations.
    pub fn from_hex(s: &str) -> CelResult<Self> {
        if s.len() != TOKEN_HEX_LEN
            || !s.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(CelError::Invalid("token format"));
        }
        Ok(Self(s.to_ascii_lowercase()))
    }
}

/// Token length in raw bytes (CSPRNG output).
pub const TOKEN_BYTES: usize = 32;
/// Token length when hex-encoded.
pub const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// Stable on-disk fingerprint of a [`SessionToken`]. Stored on the
/// `StoreState`; **never** the plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenHash(pub [u8; 32]);

/// SHA-256 of the token's hex string. We hash the hex form (not
/// the raw bytes) so that a session file leak — which only ever
/// contains the hex — cannot be replayed against any other format.
#[must_use]
pub fn hash_token(token: &SessionToken) -> TokenHash {
    let mut h = Sha256::new();
    h.update(token.0.as_bytes());
    let digest = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    TokenHash(out)
}

/// Mint a fresh random token; return both the plaintext (to hand
/// back to the caller) and its hash (to persist).
#[must_use]
pub fn mint_token() -> (SessionToken, TokenHash) {
    let mut raw = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut raw);
    let mut s = String::with_capacity(TOKEN_HEX_LEN);
    for byte in raw {
        // Two lowercase hex chars per byte, manually emitted to
        // avoid pulling in another crate just for `to_hex`.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    let token = SessionToken(s);
    let th = hash_token(&token);
    (token, th)
}

/// Default session lifetime applied when a caller passes `None`.
/// 12 hours mirrors a workday shift.
pub const DEFAULT_SESSION_TTL_SECS: u64 = 12 * 60 * 60;

/// A live session record. Stored in the tenant store keyed by
/// [`TokenHash`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Hash of the plaintext token. Same value as the map key.
    pub token_hash: TokenHash,
    /// Tenant the session belongs to.
    pub tenant: TenantId,
    /// User the session authenticates.
    pub user: UserId,
    /// User name (cached so callers don't have to round-trip the
    /// tenant snapshot just to print a friendly identifier).
    pub user_name: String,
    /// Effective capabilities for the session. Always a subset of
    /// the user's caps at mint time (attenuation goes through
    /// [`crate::attenuate`]).
    pub caps: TenantCaps,
    /// Unix-millis wall-clock issuance time.
    pub created_ms: u64,
    /// Unix-millis wall-clock expiry. The session is invalid the
    /// instant `now_ms() >= expires_ms`.
    pub expires_ms: u64,
}

impl Session {
    /// Whether `now_ms` is at or past [`Self::expires_ms`].
    #[must_use]
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_ms
    }
}

/// Current wall clock in unix milliseconds. Saturates to 0 if the
/// host clock is somehow before 1970 (this is impossible on any
/// production machine; we just refuse to panic).
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_password_verifies() {
        let h = hash_password("hunter2").unwrap();
        assert!(verify_password("hunter2", &h).is_ok());
        // PHC strings never embed the plaintext.
        assert!(!h.as_str().contains("hunter2"));
        // Wrong password rejected with the uniform error tag.
        let err = verify_password("hunter3", &h).unwrap_err();
        assert!(matches!(err, CelError::CapabilityDenied("auth.credentials")));
    }

    #[test]
    fn empty_password_refused() {
        let err = hash_password("").unwrap_err();
        assert!(matches!(err, CelError::Invalid(_)));
    }

    #[test]
    fn two_hashes_of_same_password_differ() {
        // Different salts → different PHC strings.
        let h1 = hash_password("hunter2").unwrap();
        let h2 = hash_password("hunter2").unwrap();
        assert_ne!(h1.as_str(), h2.as_str());
        assert!(verify_password("hunter2", &h1).is_ok());
        assert!(verify_password("hunter2", &h2).is_ok());
    }

    #[test]
    fn token_mint_is_unique_and_hex() {
        let (t1, _) = mint_token();
        let (t2, _) = mint_token();
        assert_ne!(t1.as_str(), t2.as_str());
        assert_eq!(t1.as_str().len(), TOKEN_HEX_LEN);
        assert!(t1.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn token_hash_is_deterministic() {
        let (t, h1) = mint_token();
        let h2 = hash_token(&t);
        assert_eq!(h1, h2);
    }

    #[test]
    fn token_from_hex_validates() {
        let (t, _) = mint_token();
        let parsed = SessionToken::from_hex(t.as_str()).unwrap();
        assert_eq!(parsed, t);
        // Too short.
        assert!(SessionToken::from_hex("deadbeef").is_err());
        // Wrong alphabet.
        let mut bogus = "z".repeat(TOKEN_HEX_LEN);
        bogus.truncate(TOKEN_HEX_LEN);
        assert!(SessionToken::from_hex(&bogus).is_err());
    }

    #[test]
    fn session_expiry_arithmetic() {
        let s = Session {
            token_hash: TokenHash([0; 32]),
            tenant: TenantId(1),
            user: UserId(1),
            user_name: "alice".to_string(),
            caps: TenantCaps::NONE,
            created_ms: 1000,
            expires_ms: 2000,
        };
        assert!(!s.is_expired_at(1999));
        assert!(s.is_expired_at(2000));
        assert!(s.is_expired_at(9999));
    }
}
