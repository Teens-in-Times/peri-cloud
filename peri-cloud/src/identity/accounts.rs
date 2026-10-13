use super::crypto::{hash, password_hash, secret};
use super::service::normalized_login;
use super::{Authenticated, IdentityError, IdentityResult, IdentityService, Principal};
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

/// Returned once; only a hash is retained in the database.
#[derive(Serialize)]
pub struct Invite {
    pub id: Uuid,
    pub code: String,
    pub expires_in: u64,
}

impl IdentityService {
    pub async fn is_admin(&self, actor: &Authenticated) -> IdentityResult<bool> {
        self.browser_scope(actor).await?;
        let row = sqlx::query("SELECT admin FROM account_roles WHERE principal=?")
            .bind(actor.principal.id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some_and(|r| r.get::<i64, _>("admin") == 1))
    }

    pub async fn require_admin(&self, actor: &Authenticated) -> IdentityResult<()> {
        if !self.is_admin(actor).await? {
            return Err(IdentityError::Forbidden);
        }
        Ok(())
    }

    pub async fn create_invite(&self, actor: &Authenticated) -> IdentityResult<Invite> {
        self.require_admin(actor).await?;
        self.rate_limit("invite-create", 16, 60).await?;
        let id = Uuid::new_v4();
        let code = secret();
        sqlx::query("DELETE FROM account_invites WHERE expires_at<? OR used=1")
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        sqlx::query("INSERT INTO account_invites(id,code_hash,issuer,expires_at) VALUES(?,?,?,?)")
            .bind(id.to_string())
            .bind(hash("invite", &code))
            .bind(actor.principal.id.to_string())
            .bind(self.now() + 86400)
            .execute(&self.pool)
            .await?;
        Ok(Invite {
            id,
            code,
            expires_in: 86400,
        })
    }

    pub async fn revoke_invite(&self, actor: &Authenticated, id: Uuid) -> IdentityResult<()> {
        self.require_admin(actor).await?;
        sqlx::query("UPDATE account_invites SET used=1 WHERE id=?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn register_invited(
        &self,
        code: &str,
        login: &str,
        password: String,
        display_name: &str,
    ) -> IdentityResult<Principal> {
        if code.len() != 64 {
            return Err(IdentityError::Unauthorized);
        }
        self.rate_limit("register-global", 16, 60).await?;
        let login = normalized_login(login)?;
        if !(12..=1024).contains(&password.len())
            || display_name.trim().is_empty()
            || display_name.len() > 128
        {
            return Err(IdentityError::Invalid);
        }
        let code_hash = hash("invite", code);
        let (valid,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM account_invites WHERE code_hash=? AND used=0 AND expires_at>?",
        )
        .bind(&code_hash)
        .bind(self.now())
        .fetch_one(&self.pool)
        .await?;
        if valid == 0 {
            return Err(IdentityError::Unauthorized);
        }
        let permit = self
            .password_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IdentityError::Crypto)?;
        let encoded = password_hash(password, Some(permit)).await?;
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE account_invites SET used=1 WHERE code_hash=? AND used=0 AND expires_at>?",
        )
        .bind(&code_hash)
        .bind(self.now())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(IdentityError::Unauthorized);
        }
        let (exists,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM principals WHERE login=?")
            .bind(&login)
            .fetch_one(&mut *tx)
            .await?;
        if exists != 0 {
            return Err(IdentityError::OwnershipConflict);
        }
        let principal = Principal {
            id: Uuid::new_v4(),
            login,
            display_name: display_name.trim().into(),
        };
        sqlx::query("INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,?)")
            .bind(principal.id.to_string())
            .bind(&principal.login)
            .bind(&principal.display_name)
            .bind(encoded)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO account_roles(principal,admin) VALUES(?,0)")
            .bind(principal.id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(principal)
    }
}
