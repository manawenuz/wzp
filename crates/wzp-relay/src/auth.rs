//! featherChat token authentication.
//!
//! When `--auth-url` is configured, the relay validates bearer tokens
//! against featherChat's `POST /v1/auth/validate` endpoint before
//! allowing clients to join rooms.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Request body for featherChat token validation.
#[derive(Serialize)]
struct ValidateRequest {
    token: String,
}

/// Response from featherChat token validation.
#[derive(Deserialize, Debug)]
pub struct ValidateResponse {
    pub valid: bool,
    pub fingerprint: Option<String>,
    pub alias: Option<String>,
}

/// Validated client identity.
#[derive(Clone, Debug)]
pub struct AuthenticatedClient {
    pub fingerprint: String,
    pub alias: Option<String>,
}

/// Validate a bearer token against featherChat's auth endpoint.
///
/// Calls `POST {auth_url}` with `{ "token": "..." }`.
/// Returns the client identity if valid, or an error string.
pub async fn validate_token(
    auth_url: &str,
    token: &str,
) -> Result<AuthenticatedClient, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("http client error: {e}"))?;

    let resp = client
        .post(auth_url)
        .json(&ValidateRequest {
            token: token.to_string(),
        })
        .send()
        .await
        .map_err(|e| format!("auth request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("auth endpoint returned {}", resp.status()));
    }

    let body: ValidateResponse = resp
        .json()
        .await
        .map_err(|e| format!("invalid auth response: {e}"))?;

    if body.valid {
        let fingerprint = body
            .fingerprint
            .ok_or_else(|| "valid response missing fingerprint".to_string())?;
        info!(%fingerprint, alias = ?body.alias, "token validated");
        Ok(AuthenticatedClient {
            fingerprint,
            alias: body.alias,
        })
    } else {
        warn!("token validation failed");
        Err("invalid token".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_request_serializes() {
        let req = ValidateRequest {
            token: "abc123".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("abc123"));
    }

    #[test]
    fn validate_response_deserializes() {
        let json = r#"{"valid": true, "fingerprint": "abcd1234", "alias": "manwe"}"#;
        let resp: ValidateResponse = serde_json::from_str(json).unwrap();
        assert!(resp.valid);
        assert_eq!(resp.fingerprint.unwrap(), "abcd1234");
        assert_eq!(resp.alias.unwrap(), "manwe");
    }

    #[test]
    fn invalid_response_deserializes() {
        let json = r#"{"valid": false}"#;
        let resp: ValidateResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.valid);
        assert!(resp.fingerprint.is_none());
    }
}
