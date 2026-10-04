// Feature assertions for the fork's JWT support (KEEP patch 0014).

use hbbs::jwt;

/// SECRET is a `Lazy<String>`: it latches the first time any jwt function runs.
/// Every test in this binary therefore has to agree on the key, and it has to be
/// in the environment before the first call.
fn init_secret() {
    std::env::set_var("RUSTDESK_API_JWT_KEY", "testjwt");
}

#[test]
fn generates_a_token() {
    init_secret();
    let token = jwt::generate_token(1, 3600).unwrap();
    assert!(!token.is_empty(), "generated token should not be empty");
    assert_eq!(token.split('.').count(), 3, "expected a three-part JWS");
}

#[test]
fn accepts_its_own_token() {
    init_secret();
    let token = jwt::generate_token(7, 3600).unwrap();
    let claims = jwt::verify_token(&token).expect("freshly minted token should verify");
    assert_eq!(claims.user_id, 7);
}

#[test]
fn rejects_a_bad_signature() {
    init_secret();
    let token = jwt::generate_token(1, 3600).unwrap();
    let mut parts: Vec<String> = token.split('.').map(|s| s.to_string()).collect();
    assert_eq!(parts.len(), 3);
    // Flip one character of the signature. Any change invalidates the MAC.
    let sig = &parts[2];
    let first = sig.chars().next().unwrap();
    let replacement = if first == 'A' { 'B' } else { 'A' };
    parts[2] = format!("{}{}", replacement, &sig[first.len_utf8()..]);
    let forged = parts.join(".");

    assert!(
        jwt::verify_token(&forged).is_err(),
        "a token with a tampered signature must be rejected"
    );
}

#[test]
fn rejects_an_expired_token() {
    init_secret();
    // exp one hour in the past.
    let token = jwt::generate_token(1, -3600).unwrap();
    assert!(
        jwt::verify_token(&token).is_err(),
        "an expired token must be rejected"
    );
}

#[test]
fn rejects_garbage() {
    init_secret();
    assert!(jwt::verify_token("").is_err());
    assert!(jwt::verify_token("not-a-jwt").is_err());
    assert!(jwt::verify_token("a.b.c").is_err());
}

/// A token issued by the real rustdesk-api (lejianwen/rustdesk-api, image of
/// 2025-09-28) started with RUSTDESK_API_JWT_KEY=interop-test-secret, logging in as
/// its initial admin. Kept byte for byte, so that what hbbs verifies is what that
/// API really emits and not what this repository assumes it emits:
///
///     header {"alg":"HS256","typ":"JWT"}   claims {"user_id":1,"exp":<7 days on>}
///
/// Its exp has since passed, so expiry validation is switched off: this checks the
/// signature scheme and that the claims fit `Claims`, which is the part that could
/// silently drift, and does not go stale.
const REAL_API_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJ1c2VyX2lkIjoxLCJleHAiOjE3OTE3MjAyMjB9.QcsTexdocTy2GmMRbSTL_y5RABjAhaajLhCt73ZkzSM";
const REAL_API_SECRET: &str = "interop-test-secret";

#[test]
fn a_token_the_real_api_issued_is_readable_by_this_verifier() {
    use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = false;
    let data = decode::<jwt::Claims>(
        REAL_API_TOKEN,
        &DecodingKey::from_secret(REAL_API_SECRET.as_ref()),
        &validation,
    )
    .expect("the real API's token should verify with its own secret");
    assert_eq!(data.claims.user_id, 1);

    // And the wrong secret must not.
    assert!(decode::<jwt::Claims>(
        REAL_API_TOKEN,
        &DecodingKey::from_secret(b"some-other-secret"),
        &validation,
    )
    .is_err());
}
