use super::*;
use serde_json::json;
use sqlx::{Row, SqlitePool};

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if version == 7 {
        sqlx::query("CREATE TABLE IF NOT EXISTS model_selections (principal TEXT NOT NULL REFERENCES principals(id), scope TEXT NOT NULL, model TEXT, revision INTEGER NOT NULL, PRIMARY KEY(principal,scope))").execute(&mut *tx).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS control_operations (id TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), kind TEXT NOT NULL, target TEXT NOT NULL, request_hash TEXT NOT NULL, state TEXT NOT NULL, result_json TEXT NOT NULL, created_at INTEGER NOT NULL)").execute(&mut *tx).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS turn_models (turn_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, principal TEXT NOT NULL, model TEXT NOT NULL)").execute(&mut *tx).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS model_endpoints (principal TEXT PRIMARY KEY REFERENCES principals(id), api_base TEXT NOT NULL, protocol TEXT NOT NULL)").execute(&mut *tx).await?;
        sqlx::query("PRAGMA user_version=8")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
impl Aggregation {
    pub(crate) async fn inherit_selection(
        &self,
        principal: Uuid,
        from: Uuid,
        to: Uuid,
    ) -> AggregateResult<()> {
        self.scope(principal, Some(from)).await?;
        self.scope(principal, Some(to)).await?;
        sqlx::query("INSERT OR IGNORE INTO model_selections(principal,scope,model,revision) SELECT principal,?,model,1 FROM model_selections WHERE principal=? AND scope=?")
            .bind(to.to_string()).bind(principal.to_string()).bind(from.to_string()).execute(&self.journal.identity_pool()).await?;
        Ok(())
    }
    async fn scope(&self, principal: Uuid, session: Option<Uuid>) -> AggregateResult<String> {
        if let Some(id) = session {
            self.journal
                .session(principal, id)
                .await
                .map_err(|_| AggregateError::Ownership)?;
        }
        Ok(session.map(|s| s.to_string()).unwrap_or_default())
    }
    pub async fn selection(
        &self,
        actor: &ControlActor<'_>,
        session: Option<Uuid>,
    ) -> AggregateResult<ModelSelection> {
        let principal = self.principal(actor, false).await?;
        let scope = self.scope(principal, session).await?;
        self.read_selection(principal, &scope).await
    }
    async fn read_selection(
        &self,
        principal: Uuid,
        scope: &str,
    ) -> AggregateResult<ModelSelection> {
        let row = sqlx::query(
            "SELECT model,revision FROM model_selections WHERE principal=? AND scope=?",
        )
        .bind(principal.to_string())
        .bind(scope)
        .fetch_optional(&self.journal.identity_pool())
        .await?;
        Ok(row
            .map(|r| ModelSelection {
                model: r.get("model"),
                revision: r.get("revision"),
            })
            .unwrap_or(ModelSelection {
                model: None,
                revision: 0,
            }))
    }
    pub async fn choose_model(
        &self,
        actor: &ControlActor<'_>,
        session: Option<Uuid>,
        input: ModelSelection,
    ) -> AggregateResult<ModelSelection> {
        let principal = self.principal(actor, false).await?;
        let scope = self.scope(principal, session).await?;
        if let Some(name) = &input.model {
            if !client::valid_model(name) || !self.client.models().await?.contains(name) {
                return Err(AggregateError::Invalid);
            }
        }
        // Recheck transport identity after the external catalog request.
        self.principal(actor, false).await?;
        let pool = self.journal.identity_pool();
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO model_selections(principal,scope,model,revision) VALUES(?,?,NULL,0)").bind(principal.to_string()).bind(&scope).execute(&mut *tx).await?;
        let result=sqlx::query("UPDATE model_selections SET model=?,revision=revision+1 WHERE principal=? AND scope=? AND revision=?")
            .bind(&input.model).bind(principal.to_string()).bind(&scope).bind(input.revision).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return Err(AggregateError::Conflict);
        }
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO control_operations(id,principal,kind,target,request_hash,state,result_json,created_at) VALUES(?,?, 'model.select',?,'','succeeded',?,?)")
            .bind(id.to_string()).bind(principal.to_string()).bind(scope).bind(json!({"model":input.model,"revision":input.revision+1}).to_string()).bind(self.identity.now()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(ModelSelection {
            model: input.model,
            revision: input.revision + 1,
        })
    }
    pub(crate) async fn model_for(
        &self,
        principal: Uuid,
        session: Option<Uuid>,
    ) -> AggregateResult<Arc<dyn peri_model::Model>> {
        self.model_for_turn(principal, session, None).await
    }
    pub(crate) async fn model_for_turn(
        &self,
        principal: Uuid,
        session: Option<Uuid>,
        turn: Option<Uuid>,
    ) -> AggregateResult<Arc<dyn peri_model::Model>> {
        let name = self.current_model(principal, session).await?;
        if let (Some(session), Some(turn)) = (session, turn) {
            sqlx::query(
                "INSERT INTO turn_models(turn_id,session_id,principal,model) VALUES(?,?,?,?)",
            )
            .bind(turn.to_string())
            .bind(session.to_string())
            .bind(principal.to_string())
            .bind(&name)
            .execute(&self.journal.identity_pool())
            .await?;
        }
        Ok(Arc::new(peri_model::OpenAiModel::new(
            peri_model::OpenAiConfig::new(
                self.client.inference_base(),
                self.client.inference_key(),
                name,
            )
            .with_max_tokens(self.max_tokens),
        )))
    }
    pub(crate) async fn current_model(
        &self,
        principal: Uuid,
        session: Option<Uuid>,
    ) -> AggregateResult<String> {
        let scope = self.scope(principal, session).await?;
        let selected = self.read_selection(principal, &scope).await?;
        let account = if selected.model.is_none() && !scope.is_empty() {
            self.read_selection(principal, "").await?.model
        } else {
            None
        };
        let name = selected
            .model
            .or(account)
            .unwrap_or_else(|| self.default_model.clone());
        Ok(name)
    }
}
