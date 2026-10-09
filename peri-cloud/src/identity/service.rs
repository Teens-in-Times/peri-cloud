use std::sync::Arc;

use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use super::clock::WallClock;
use super::crypto::{equal, hash, password_hash, secret, verify_password};
use super::{
    Authenticated, DeviceRecord, IdentityClock, IdentityError, IdentityResult, LoginReceipt,
    Principal, RegistrationReceipt,
};
use crate::state::CloudJournal;

const BROWSER_LIFETIME: i64 = 12 * 60 * 60;

pub struct IdentityService {
    pub(crate) pool: SqlitePool,
    bootstrap_hash: String,
    dummy_password: String,
    password_gate: Arc<tokio::sync::Semaphore>,
    clock: Arc<dyn IdentityClock>,
}

impl IdentityService {
    /// Setup credential is deployment configuration, used once to initialize
    /// the private cloud account. Public registration is never enabled.
    pub async fn new(journal: &CloudJournal, bootstrap_token: &str) -> IdentityResult<Arc<Self>> {
        Self::with_clock(journal, bootstrap_token, Arc::new(WallClock)).await
    }

    pub async fn with_clock(
        journal: &CloudJournal,
        bootstrap_token: &str,
        clock: Arc<dyn IdentityClock>,
    ) -> IdentityResult<Arc<Self>> {
        if bootstrap_token.len() < 32 {
            return Err(IdentityError::Invalid);
        }
        Ok(Arc::new(Self {
            pool: journal.identity_pool(),
            bootstrap_hash: hash("bootstrap", bootstrap_token),
            dummy_password: password_hash(secret(), None).await?,
            password_gate: Arc::new(tokio::sync::Semaphore::new(4)),
            clock,
        }))
    }

    pub async fn initialize(
        &self,
        bootstrap_token: &str,
        login: &str,
        password: String,
        display_name: &str,
    ) -> IdentityResult<Principal> {
        if !equal(&self.bootstrap_hash, &hash("bootstrap", bootstrap_token)) {
            return Err(IdentityError::Unauthorized);
        }
        if self.initialized().await? {
            return Err(IdentityError::AlreadyInitialized);
        }
        self.rate_limit("setup", 4, 60).await?;
        let login = normalized_login(login)?;
        if !(12..=1024).contains(&password.len())
            || display_name.trim().is_empty()
            || display_name.len() > 128
        {
            return Err(IdentityError::Invalid);
        }
        let permit = self
            .password_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IdentityError::Crypto)?;
        let encoded = password_hash(password, Some(permit)).await?;
        let mut tx = self.pool.begin().await?;
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM principals")
            .fetch_one(&mut *tx)
            .await?;
        if count != 0 {
            return Err(IdentityError::AlreadyInitialized);
        }
        let principal = Principal {
            id: Uuid::new_v4(),
            login,
            display_name: display_name.to_owned(),
        };
        sqlx::query("INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,?)")
            .bind(principal.id.to_string())
            .bind(&principal.login)
            .bind(&principal.display_name)
            .bind(encoded)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(principal)
    }

    pub async fn initialized(&self) -> IdentityResult<bool> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM principals")
            .fetch_one(&self.pool)
            .await?;
        Ok(count != 0)
    }

    pub async fn login(&self, login: &str, password: String) -> IdentityResult<LoginReceipt> {
        let normalized = normalized_login(login).map_err(|_| IdentityError::Unauthorized)?;
        if password.len() > 1024 {
            return Err(IdentityError::Unauthorized);
        }
        self.rate_limit("login-global", 32, 60).await?;
        self.rate_limit(&format!("login:{normalized}"), 8, 60)
            .await?;
        let permit = self
            .password_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IdentityError::Crypto)?;
        let row =
            sqlx::query("SELECT id,login,display_name,password_hash FROM principals WHERE login=?")
                .bind(normalized)
                .fetch_optional(&self.pool)
                .await?;
        let encoded = row
            .as_ref()
            .map(|row| row.get::<String, _>("password_hash"))
            .unwrap_or_else(|| self.dummy_password.clone());
        let correct = verify_password(password, encoded, permit).await?;
        let row = row.filter(|_| correct).ok_or(IdentityError::Unauthorized)?;
        let principal = principal_from_row(&row)?;
        let session_token = secret();
        let csrf_token = secret();
        sqlx::query("INSERT INTO browser_sessions(token_hash,principal,csrf_hash,expires_at) VALUES(?,?,?,?)")
            .bind(hash("browser", &session_token)).bind(principal.id.to_string()).bind(hash("csrf", &csrf_token)).bind(self.now() + BROWSER_LIFETIME).execute(&self.pool).await?;
        Ok(LoginReceipt {
            principal,
            session_token,
            csrf_token,
            expires_in: BROWSER_LIFETIME as u64,
        })
    }

    pub async fn authenticate_browser(&self, token: &str) -> IdentityResult<Authenticated> {
        let credential_hash = hash("browser", token);
        let row = sqlx::query("SELECT p.id,p.login,p.display_name FROM browser_sessions s JOIN principals p ON p.id=s.principal WHERE s.token_hash=? AND s.expires_at>?")
            .bind(&credential_hash).bind(self.now()).fetch_optional(&self.pool).await?.ok_or(IdentityError::Unauthorized)?;
        Ok(Authenticated {
            principal: principal_from_row(&row)?,
            browser: true,
            device_id: None,
            credential_hash,
        })
    }

    pub(crate) async fn browser_scope(&self, actor: &Authenticated) -> IdentityResult<()> {
        if !actor.browser {
            return Err(IdentityError::Forbidden);
        }
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM browser_sessions WHERE token_hash=? AND principal=? AND expires_at>?")
            .bind(&actor.credential_hash).bind(actor.principal.id.to_string()).bind(self.now()).fetch_one(&self.pool).await?;
        if count == 0 {
            return Err(IdentityError::Unauthorized);
        }
        Ok(())
    }

    pub async fn verify_csrf(&self, actor: &Authenticated, csrf: &str) -> IdentityResult<()> {
        self.browser_scope(actor).await?;
        let row = sqlx::query("SELECT csrf_hash FROM browser_sessions WHERE token_hash=?")
            .bind(&actor.credential_hash)
            .fetch_one(&self.pool)
            .await?;
        if !equal(&row.get::<String, _>("csrf_hash"), &hash("csrf", csrf)) {
            return Err(IdentityError::Unauthorized);
        }
        Ok(())
    }

    pub async fn logout(&self, actor: &Authenticated) -> IdentityResult<()> {
        self.browser_scope(actor).await?;
        sqlx::query("DELETE FROM browser_sessions WHERE token_hash=?")
            .bind(&actor.credential_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn register_device(
        &self,
        actor: &Authenticated,
        mut device: DeviceRecord,
    ) -> IdentityResult<RegistrationReceipt> {
        if actor.browser || actor.device_id != Some(device.id) {
            return Err(IdentityError::Forbidden);
        }
        self.native_scope(actor).await?;
        if device.name.trim().is_empty()
            || device.name.len() > 128
            || !matches!(device.platform.as_str(), "windows" | "linux")
            || device.default_workspace.is_empty()
            || device.default_workspace.len() > 4096
            || device.default_workspace.contains('\0')
            || device.connection_id.is_some()
        {
            return Err(IdentityError::Invalid);
        }
        device.revoked = false;
        let mut tx = self.pool.begin().await?;
        if let Some(row) =
            sqlx::query("SELECT principal,record_json FROM identity_devices WHERE id=?")
                .bind(device.id.to_string())
                .fetch_optional(&mut *tx)
                .await?
        {
            if row.get::<String, _>("principal") != actor.principal.id.to_string() {
                return Err(IdentityError::OwnershipConflict);
            }
            // The native grant can update its metadata. Connection configuration
            // is deployment-owned and cannot be replaced through registration.
            let existing: DeviceRecord = serde_json::from_str(row.get("record_json"))?;
            device.connection_id = existing.connection_id;
        }
        sqlx::query("INSERT INTO identity_devices(id,principal,record_json) VALUES(?,?,?) ON CONFLICT(id) DO UPDATE SET record_json=excluded.record_json")
            .bind(device.id.to_string()).bind(actor.principal.id.to_string()).bind(serde_json::to_string(&device)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(RegistrationReceipt {
            principal_id: actor.principal.id,
            device,
        })
    }

    pub async fn devices(&self, actor: &Authenticated) -> IdentityResult<Vec<DeviceRecord>> {
        self.browser_scope(actor).await?;
        let rows =
            sqlx::query("SELECT record_json FROM identity_devices WHERE principal=? ORDER BY id")
                .bind(actor.principal.id.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|row| serde_json::from_str(row.get("record_json")).map_err(IdentityError::from))
            .collect()
    }

    pub async fn revoke_device(&self, actor: &Authenticated, device: Uuid) -> IdentityResult<()> {
        self.browser_scope(actor).await?;
        let mut tx = self.pool.begin().await?;
        let row =
            sqlx::query("SELECT record_json FROM identity_devices WHERE id=? AND principal=?")
                .bind(device.to_string())
                .bind(actor.principal.id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(IdentityError::Forbidden)?;
        let mut record: DeviceRecord = serde_json::from_str(row.get("record_json"))?;
        record.revoked = true;
        sqlx::query("UPDATE identity_devices SET record_json=? WHERE id=?")
            .bind(serde_json::to_string(&record)?)
            .bind(device.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE native_grants SET revoked=1 WHERE device_id=? AND principal=?")
            .bind(device.to_string())
            .bind(actor.principal.id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE native_codes SET used=1 WHERE device_id=? AND principal=?")
            .bind(device.to_string())
            .bind(actor.principal.id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn rate_limit(
        &self,
        key: &str,
        limit: i64,
        window: i64,
    ) -> IdentityResult<()> {
        let mut tx = self.pool.begin().await?;
        let current = self.now();
        sqlx::query("DELETE FROM identity_rates WHERE window_started<?")
            .bind(current - 3600)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO identity_rates(rate_key,window_started,attempts) VALUES(?,?,1) ON CONFLICT(rate_key) DO UPDATE SET attempts=CASE WHEN window_started<=? THEN 1 ELSE attempts+1 END, window_started=CASE WHEN window_started<=? THEN excluded.window_started ELSE window_started END")
            .bind(hash("rate", key)).bind(current).bind(current - window).bind(current - window).execute(&mut *tx).await?;
        let (attempts,): (i64,) =
            sqlx::query_as("SELECT attempts FROM identity_rates WHERE rate_key=?")
                .bind(hash("rate", key))
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        if attempts > limit {
            return Err(IdentityError::RateLimited);
        }
        Ok(())
    }

    pub(crate) fn now(&self) -> i64 {
        self.clock.unix_seconds()
    }
}

pub(crate) fn principal_from_row(row: &sqlx::sqlite::SqliteRow) -> IdentityResult<Principal> {
    Ok(Principal {
        id: Uuid::parse_str(row.get("id")).map_err(|_| IdentityError::Invalid)?,
        login: row.get("login"),
        display_name: row.get("display_name"),
    })
}

fn normalized_login(login: &str) -> IdentityResult<String> {
    let login = login.trim().to_ascii_lowercase();
    if login.is_empty()
        || login.len() > 64
        || !login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        return Err(IdentityError::Invalid);
    }
    Ok(login)
}
