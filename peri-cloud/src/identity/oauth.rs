use sqlx::Row;
use url::Url;
use uuid::Uuid;

use super::crypto::{challenge, equal, hash, secret};
use super::service::principal_from_row;
use super::{
    Authenticated, IdentityError, IdentityResult, IdentityService, NativeAuthorization,
    NativeTokens,
};

const NATIVE_CLIENT: &str = "peri-executor";
const ACCESS_LIFETIME: i64 = 30 * 60;
const REFRESH_LIFETIME: i64 = 30 * 24 * 60 * 60;

impl IdentityService {
    /// Called only after explicit browser consent. This is account/connection
    /// authorization; tool permission remains the Peri session policy.
    pub async fn authorize_native(
        &self,
        actor: &Authenticated,
        authorization: NativeAuthorization,
    ) -> IdentityResult<String> {
        self.browser_scope(actor).await?;
        validate_authorization(&authorization)?;
        let mut tx = self.pool.begin().await?;
        if let Some(row) = sqlx::query("SELECT principal FROM identity_devices WHERE id=?")
            .bind(authorization.device_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
        {
            if row.get::<String, _>("principal") != actor.principal.id.to_string() {
                return Err(IdentityError::OwnershipConflict);
            }
        }
        let code = secret();
        sqlx::query("INSERT INTO native_codes(code_hash,principal,device_id,authorization_json,expires_at) VALUES(?,?,?,?,?)")
            .bind(hash("code", &code)).bind(actor.principal.id.to_string()).bind(authorization.device_id.to_string()).bind(serde_json::to_string(&authorization)?).bind(self.now() + 120).execute(&mut *tx).await?;
        tx.commit().await?;
        let mut redirect =
            Url::parse(&authorization.redirect_uri).map_err(|_| IdentityError::Invalid)?;
        redirect
            .query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &authorization.state);
        Ok(redirect.to_string())
    }

    pub async fn exchange_code(
        &self,
        client_id: &str,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> IdentityResult<NativeTokens> {
        if client_id != NATIVE_CLIENT {
            return Err(IdentityError::Unauthorized);
        }
        let actual = challenge(verifier)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT principal,authorization_json FROM native_codes WHERE code_hash=? AND used=0 AND expires_at>?")
            .bind(hash("code", code)).bind(self.now()).fetch_optional(&mut *tx).await?.ok_or(IdentityError::Unauthorized)?;
        let authorization: NativeAuthorization =
            serde_json::from_str(row.get("authorization_json"))?;
        if authorization.client_id != client_id
            || authorization.redirect_uri != redirect_uri
            || !equal(&authorization.code_challenge, &actual)
        {
            return Err(IdentityError::Unauthorized);
        }
        let principal: String = row.get("principal");
        // Prevent overlapping enrollments from assigning one device to two
        // accounts, even before its metadata registration arrives.
        let (conflicting,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM native_grants WHERE device_id=? AND principal<>? AND revoked=0",
        )
        .bind(authorization.device_id.to_string())
        .bind(&principal)
        .fetch_one(&mut *tx)
        .await?;
        if conflicting != 0 {
            return Err(IdentityError::OwnershipConflict);
        }
        let access_token = secret();
        let refresh_token = secret();
        let current = self.now();
        sqlx::query("UPDATE native_codes SET used=1 WHERE code_hash=?")
            .bind(hash("code", code))
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO native_grants(id,principal,device_id,access_hash,refresh_hash,access_expires,refresh_expires) VALUES(?,?,?,?,?,?,?)")
            .bind(Uuid::new_v4().to_string()).bind(&principal).bind(authorization.device_id.to_string())
            .bind(hash("access", &access_token)).bind(hash("refresh", &refresh_token)).bind(current + ACCESS_LIFETIME).bind(current + REFRESH_LIFETIME).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(NativeTokens {
            access_token,
            refresh_token,
            token_type: "Bearer".into(),
            expires_in: ACCESS_LIFETIME as u64,
            principal_id: Uuid::parse_str(&principal).map_err(|_| IdentityError::Invalid)?,
            device_id: authorization.device_id,
        })
    }

    /// Refresh tokens rotate. A replay revokes that grant family rather than
    /// producing a second independently usable branch of refresh credentials.
    pub async fn refresh_native(
        &self,
        client_id: &str,
        refresh: &str,
    ) -> IdentityResult<NativeTokens> {
        if client_id != NATIVE_CLIENT {
            return Err(IdentityError::Unauthorized);
        }
        let token_hash = hash("refresh", refresh);
        let mut tx = self.pool.begin().await?;
        if let Some(row) =
            sqlx::query("SELECT grant_id FROM retired_refresh_tokens WHERE token_hash=?")
                .bind(&token_hash)
                .fetch_optional(&mut *tx)
                .await?
        {
            sqlx::query("UPDATE native_grants SET revoked=1 WHERE id=?")
                .bind(row.get::<String, _>("grant_id"))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(IdentityError::Unauthorized);
        }
        let row = sqlx::query("SELECT id,principal,device_id FROM native_grants WHERE refresh_hash=? AND revoked=0 AND refresh_expires>?")
            .bind(&token_hash).bind(self.now()).fetch_optional(&mut *tx).await?.ok_or(IdentityError::Unauthorized)?;
        let id: String = row.get("id");
        let access_token = secret();
        let refresh_token = secret();
        sqlx::query("INSERT INTO retired_refresh_tokens(token_hash,grant_id) VALUES(?,?)")
            .bind(&token_hash)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE native_grants SET access_hash=?,refresh_hash=?,access_expires=? WHERE id=?",
        )
        .bind(hash("access", &access_token))
        .bind(hash("refresh", &refresh_token))
        .bind(self.now() + ACCESS_LIFETIME)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let principal_id =
            Uuid::parse_str(row.get("principal")).map_err(|_| IdentityError::Invalid)?;
        let device_id =
            Uuid::parse_str(row.get("device_id")).map_err(|_| IdentityError::Invalid)?;
        tx.commit().await?;
        Ok(NativeTokens {
            access_token,
            refresh_token,
            token_type: "Bearer".into(),
            expires_in: ACCESS_LIFETIME as u64,
            principal_id,
            device_id,
        })
    }

    pub async fn authenticate_native(&self, token: &str) -> IdentityResult<Authenticated> {
        let credential_hash = hash("access", token);
        let row = sqlx::query("SELECT p.id,p.login,p.display_name,g.device_id FROM native_grants g JOIN principals p ON p.id=g.principal WHERE g.access_hash=? AND g.revoked=0 AND g.access_expires>?")
            .bind(&credential_hash).bind(self.now()).fetch_optional(&self.pool).await?.ok_or(IdentityError::Unauthorized)?;
        Ok(Authenticated {
            principal: principal_from_row(&row)?,
            browser: false,
            device_id: Some(
                Uuid::parse_str(row.get("device_id")).map_err(|_| IdentityError::Invalid)?,
            ),
            credential_hash,
        })
    }

    pub(crate) async fn native_scope(&self, actor: &Authenticated) -> IdentityResult<()> {
        if actor.browser {
            return Err(IdentityError::Forbidden);
        }
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM native_grants WHERE access_hash=? AND principal=? AND device_id=? AND revoked=0 AND access_expires>?")
            .bind(&actor.credential_hash).bind(actor.principal.id.to_string()).bind(actor.device_id.ok_or(IdentityError::Forbidden)?.to_string()).bind(self.now()).fetch_one(&self.pool).await?;
        if count == 0 {
            return Err(IdentityError::Unauthorized);
        }
        Ok(())
    }
}

pub(crate) fn validate_authorization(input: &NativeAuthorization) -> IdentityResult<()> {
    input.validate().map_err(|_| IdentityError::Invalid)
}
