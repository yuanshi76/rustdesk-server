// JWT validation for tokens minted by the companion API server
// (https://github.com/lejianwen/rustdesk-api).
//
// The API server and this server share a symmetric secret, passed in the
// environment as RUSTDESK_API_JWT_KEY. When it is set and MUST_LOGIN is on, a
// punch-hole request must carry a token this module accepts.

use hbb_common::log;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::env;

/// Read once, on first use. Set RUSTDESK_API_JWT_KEY before the first token is
/// checked; changing it later has no effect until restart.
pub static SECRET: Lazy<String> =
    Lazy::new(|| env::var("RUSTDESK_API_JWT_KEY").unwrap_or_default());

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub user_id: u32,
    pub exp: usize,
}

pub fn generate_token(user_id: u32, exp: i64) -> Result<String, String> {
    let claims = Claims {
        user_id,
        exp: (chrono::Utc::now() + chrono::Duration::seconds(exp)).timestamp() as usize,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(SECRET.as_ref()),
    )
    .map_err(|e| e.to_string())
}

/// Verify an HS256 token against SECRET. `jsonwebtoken` already rejects an
/// expired token; the explicit `exp` comparison below is kept so that a token
/// expiring within the default leeway is still refused.
pub fn verify_token(token: &str) -> Result<Claims, String> {
    let validation = Validation::new(Algorithm::HS256);

    match decode::<Claims>(
        token,
        &DecodingKey::from_secret(SECRET.as_ref()),
        &validation,
    ) {
        Ok(token_data) => {
            let now = chrono::Utc::now().timestamp() as usize;
            if token_data.claims.exp > now {
                Ok(token_data.claims)
            } else {
                Err("Token status invalid or expired".to_string())
            }
        }
        Err(e) => {
            // The error kind is safe to log; the token and the secret are not.
            log::debug!("jwt rejected: {:?}", e.kind());
            Err("Invalid token".to_string())
        }
    }
}
