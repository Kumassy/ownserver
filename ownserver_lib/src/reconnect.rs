use serde::{Deserialize, Serialize};
use chrono::{Duration, Utc};
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

    fn _into_token(self, secret: &str, duration: Duration) -> Result<String, JWTError> {
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

    pub fn into_token(self, secret: &str) -> Result<String, JWTError> {
        self._into_token(secret, RECONNECT_TOKEN_VALID_DURATION)
    }

    fn _decode_token(secret: &str, token: &str, leeway: u64) -> Result<Self, JWTError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = leeway;

        let token_message: Result<jsonwebtoken::TokenData<ReconnectTokenClaims>, JWTError> = decode::<ReconnectTokenClaims>(
            token,
            &DecodingKey::from_secret(secret.as_ref()),
            &validation,
        );
        token_message.map(|data| data.claims.into())
    }

    pub fn decode_token(secret: &str, token: &str) -> Result<Self, JWTError> {
        Self::_decode_token(secret, token, 60)
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

        let token = ReconnectTokenPayload::new(client_id)._into_token(secret, Duration::seconds(3))?;
        sleep(std::time::Duration::from_secs(5)).await;

        let decode_result = ReconnectTokenPayload::_decode_token(secret, &token, 0);
        assert!(decode_result.is_err());

        Ok(())
    }

}