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
