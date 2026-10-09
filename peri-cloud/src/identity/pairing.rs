use sqlx::Row;
use uuid::Uuid;

use super::crypto::hash;
use super::{
    Authenticated, ChannelIdentity, IdentityError, IdentityResult, IdentityService, PairClaim,
    PairCode,
};

const PAIR_LIFETIME: i64 = 300;

impl IdentityService {
    pub async fn create_pair_code(&self, actor: &Authenticated) -> IdentityResult<PairCode> {
        self.browser_scope(actor).await?;
        self.rate_limit(&format!("pair-issue:{}", actor.principal.id), 10, 60)
            .await?;
        let code = Uuid::new_v4().simple().to_string()[..12].to_ascii_uppercase();
        let mut tx = self.pool.begin().await?;
        // Generate a replacement code revokes any unconfirmed previous one.
        sqlx::query("UPDATE pair_codes SET state='rejected' WHERE principal=? AND state IN ('issued','claimed')")
            .bind(actor.principal.id.to_string()).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO pair_codes(code_hash,principal,expires_at,state) VALUES(?,?,?,'issued')",
        )
        .bind(hash("pair", &code))
        .bind(actor.principal.id.to_string())
        .bind(self.now() + PAIR_LIFETIME)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(PairCode {
            code,
            expires_in: PAIR_LIFETIME as u64,
        })
    }

    /// Only an authenticated adapter calls this with its observed sender ID.
    /// A public browser/native API must not accept arbitrary external_user_id.
    pub async fn claim_pair_code(
        &self,
        identity: ChannelIdentity,
        code: &str,
    ) -> IdentityResult<PairClaim> {
        if code.len() > 64 {
            return Err(IdentityError::Unauthorized);
        }
        validate_identity(&identity)?;
        self.rate_limit(
            &format!(
                "pair:{}:{}",
                identity.adapter_instance_id, identity.external_user_id
            ),
            8,
            60,
        )
        .await?;
        self.rate_limit(
            &format!("pair-adapter:{}", identity.adapter_instance_id),
            120,
            60,
        )
        .await?;
        let normalized = code.trim().replace('-', "").to_ascii_uppercase();
        if normalized.len() != 12 || !normalized.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(IdentityError::Unauthorized);
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT principal,expires_at FROM pair_codes WHERE code_hash=? AND state='issued' AND expires_at>?")
            .bind(hash("pair", &normalized)).bind(self.now()).fetch_optional(&mut *tx).await?.ok_or(IdentityError::Unauthorized)?;
        let principal: String = row.get("principal");
        if let Some(row) = sqlx::query(
            "SELECT principal FROM channel_identities WHERE adapter_instance=? AND external_user=?",
        )
        .bind(&identity.adapter_instance_id)
        .bind(&identity.external_user_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.get::<String, _>("principal") != principal {
                return Err(IdentityError::OwnershipConflict);
            }
        }
        let claim = PairClaim {
            claim_id: Uuid::new_v4(),
            identity,
            expires_at: row.get("expires_at"),
        };
        sqlx::query(
            "UPDATE pair_codes SET state='claimed',claim_id=?,identity_json=? WHERE code_hash=?",
        )
        .bind(claim.claim_id.to_string())
        .bind(serde_json::to_string(&claim.identity)?)
        .bind(hash("pair", &normalized))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(claim)
    }

    pub async fn pending_pair_claims(
        &self,
        actor: &Authenticated,
    ) -> IdentityResult<Vec<PairClaim>> {
        self.browser_scope(actor).await?;
        let rows = sqlx::query("SELECT claim_id,identity_json,expires_at FROM pair_codes WHERE principal=? AND state='claimed' AND expires_at>? ORDER BY expires_at")
            .bind(actor.principal.id.to_string()).bind(self.now()).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(PairClaim {
                    claim_id: Uuid::parse_str(row.get("claim_id"))
                        .map_err(|_| IdentityError::Invalid)?,
                    identity: serde_json::from_str(row.get("identity_json"))?,
                    expires_at: row.get("expires_at"),
                })
            })
            .collect()
    }

    /// The signed-in PC confirms the exact persisted claim. SDK objects and
    /// user-provided route/identity values cannot alter what is being approved.
    pub async fn confirm_pair_claim(
        &self,
        actor: &Authenticated,
        claim: Uuid,
        allow: bool,
    ) -> IdentityResult<()> {
        self.browser_scope(actor).await?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT identity_json FROM pair_codes WHERE claim_id=? AND principal=? AND state='claimed' AND expires_at>?")
            .bind(claim.to_string()).bind(actor.principal.id.to_string()).bind(self.now()).fetch_optional(&mut *tx).await?.ok_or(IdentityError::Stale)?;
        let identity: ChannelIdentity = serde_json::from_str(row.get("identity_json"))?;
        if allow {
            if let Some(row) = sqlx::query("SELECT principal FROM channel_identities WHERE adapter_instance=? AND external_user=?")
                .bind(&identity.adapter_instance_id).bind(&identity.external_user_id).fetch_optional(&mut *tx).await? {
                if row.get::<String,_>("principal") != actor.principal.id.to_string() { return Err(IdentityError::OwnershipConflict); }
            }
            sqlx::query("INSERT INTO channel_identities(adapter_instance,external_user,principal,revoked) VALUES(?,?,?,0) ON CONFLICT(adapter_instance,external_user) DO UPDATE SET revoked=0")
                .bind(&identity.adapter_instance_id).bind(&identity.external_user_id).bind(actor.principal.id.to_string()).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE pair_codes SET state=? WHERE claim_id=?")
            .bind(if allow { "confirmed" } else { "rejected" })
            .bind(claim.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn resolve_channel(&self, identity: &ChannelIdentity) -> IdentityResult<Uuid> {
        validate_identity(identity)?;
        let row = sqlx::query("SELECT principal FROM channel_identities WHERE adapter_instance=? AND external_user=? AND revoked=0")
            .bind(&identity.adapter_instance_id).bind(&identity.external_user_id).fetch_optional(&self.pool).await?.ok_or(IdentityError::Unauthorized)?;
        Uuid::parse_str(row.get("principal")).map_err(|_| IdentityError::Invalid)
    }

    pub async fn channels(&self, actor: &Authenticated) -> IdentityResult<Vec<ChannelIdentity>> {
        self.browser_scope(actor).await?;
        let rows = sqlx::query("SELECT adapter_instance,external_user FROM channel_identities WHERE principal=? AND revoked=0 ORDER BY adapter_instance,external_user")
            .bind(actor.principal.id.to_string()).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| ChannelIdentity {
                adapter_instance_id: row.get("adapter_instance"),
                external_user_id: row.get("external_user"),
            })
            .collect())
    }

    pub async fn revoke_channel(
        &self,
        actor: &Authenticated,
        identity: &ChannelIdentity,
    ) -> IdentityResult<()> {
        self.browser_scope(actor).await?;
        let result = sqlx::query("UPDATE channel_identities SET revoked=1 WHERE adapter_instance=? AND external_user=? AND principal=?")
            .bind(&identity.adapter_instance_id).bind(&identity.external_user_id).bind(actor.principal.id.to_string()).execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(IdentityError::Forbidden);
        }
        Ok(())
    }
}

fn validate_identity(identity: &ChannelIdentity) -> IdentityResult<()> {
    if identity.adapter_instance_id.is_empty()
        || identity.adapter_instance_id.len() > 128
        || identity.external_user_id.is_empty()
        || identity.external_user_id.len() > 256
        || identity.adapter_instance_id.contains('\0')
        || identity.external_user_id.contains('\0')
    {
        return Err(IdentityError::Invalid);
    }
    Ok(())
}
