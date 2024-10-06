use serde::{Deserialize, Serialize};
use chrono::{Duration, DateTime, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation, errors::Error as JWTError};
use crate::ClientId;

const RECONNECT_TOKEN_VALID_DURATION: Duration = Duration::minutes(5);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReconnectTokenPayload {
    pub client_id: ClientId,
}

impl From<ReconnectTokenClaims> for ReconnectTokenPayload {
    fn from(claims: ReconnectTokenClaims) -> Self {
        claims.payload
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReconnectTokenClaims {
    payload: ReconnectTokenPayload,
    iat: i64,
    exp: i64,
}

impl ReconnectTokenPayload {
    pub fn new(client_id: ClientId) -> Self {
        Self {
            client_id,
        }
    }

    pub fn into_token(self, secret: &str) -> Result<String, JWTError> {
        self.into_token_raw(secret, RECONNECT_TOKEN_VALID_DURATION)
    }

    fn into_token_raw(self, secret: &str, duration: Duration) -> Result<String, JWTError> {
        let header = Header {
            typ: Some("JWT".to_string()),
            alg: Algorithm::HS256,
            ..Default::default()
        };
        let now = Utc::now();
        let iat = now.timestamp();
        let exp = (now + duration).timestamp();
        let my_claims = ReconnectTokenClaims {
            payload: self,
            iat,
            exp,
        };
    
        encode(
            &header,
            &my_claims,
            &EncodingKey::from_secret(secret.as_ref()),
        )
    }

    pub fn decode_token(secret: &str, token: &str) -> Result<Self, JWTError> {
        let token_message = decode::<ReconnectTokenClaims>(
            token,
            &DecodingKey::from_secret(secret.as_ref()),
            &Validation::new(Algorithm::HS256),
        );
        token_message.map(|data| data.claims.into())
    }
}




#[cfg(test)]
mod tests {
    use tokio::time::sleep;

    use super::*;

    #[test]
    fn test_encode_decode() -> Result<(), Box<dyn std::error::Error>> {
        let secret = "foobarbaz";
        let client_id = ClientId::new();

        let token = ReconnectTokenPayload::new(client_id).into_token(secret)?;
        let decoded = ReconnectTokenPayload::decode_token(secret, &token)?;

        assert_eq!(decoded.client_id, client_id);
        Ok(())
    }

    #[test]
    fn test_decode_with_invalid_secret() -> Result<(), Box<dyn std::error::Error>> {
        let client_id = ClientId::new();

        let token = ReconnectTokenPayload::new(client_id).into_token("foobarbaz")?;
        let decode_result = ReconnectTokenPayload::decode_token("hogepiyo", &token);

        assert!(decode_result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_decode_after_expired() -> Result<(), Box<dyn std::error::Error>> {
        let secret = "foobarbaz";
        let client_id = ClientId::new();

        let token = ReconnectTokenPayload::new(client_id).into_token_raw(secret, Duration::seconds(3))?;
        sleep(std::time::Duration::from_secs(5)).await;

        let decode_result = ReconnectTokenPayload::decode_token(secret, &token);
        assert!(decode_result.is_err());

        Ok(())
    }

}