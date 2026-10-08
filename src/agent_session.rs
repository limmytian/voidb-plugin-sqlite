//! Persistent SQLite agent sessions backed by the plugin's `SyncWorker`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use voidb_core::{
    AgentSessionCallRequest, AgentSessionCallResult, AgentSessionOpenContext, PluginAgentSession,
    PluginAgentSessionFactory, PluginSessionError, PluginSessionErrorCode, PluginSessionHealth,
    PluginSessionPurpose, SqlDialect, SqlSessionTransactionTracker, sql_allows_read_only_query,
};

use crate::{SqliteConfig, SqliteService, service::StatementResult};

pub struct SqliteAgentSessionFactory {
    config: SqliteConfig,
}

impl SqliteAgentSessionFactory {
    pub fn new(config: SqliteConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl PluginAgentSessionFactory for SqliteAgentSessionFactory {
    fn plugin_id(&self) -> &str {
        "sqlite"
    }

    async fn open(
        &self,
        context: AgentSessionOpenContext,
    ) -> Result<Arc<dyn PluginAgentSession>, PluginSessionError> {
        if !matches!(
            context.binding.purpose,
            PluginSessionPurpose::DatabaseQuery | PluginSessionPurpose::DatabaseTransaction
        ) {
            return Err(error(
                PluginSessionErrorCode::Unsupported,
                "SQLite persistent sessions require a database purpose.",
            ));
        }
        if context
            .binding
            .allowed_capabilities
            .iter()
            .any(|capability| {
                !matches!(
                    capability.as_str(),
                    "sqlite.query" | "query" | "sqlite.exec" | "exec"
                )
            })
        {
            return Err(error(
                PluginSessionErrorCode::PolicyDenied,
                "SQLite session capability scope is limited to query and exec.",
            ));
        }
        let mut service = SqliteService::new_direct(&self.config).map_err(|_| {
            error(
                PluginSessionErrorCode::OwnerUnavailable,
                "SQLite session connection could not be opened.",
            )
        })?;
        let transaction = SqlSessionTransactionTracker::from_open_input(
            SqlDialect::Sqlite,
            &context.request.input,
        )
        .map_err(|reason| {
            PluginSessionError::new(
                PluginSessionErrorCode::PolicyDenied,
                format!("SQLite session transaction options are invalid: {reason}"),
            )
        })?;
        if let Some(sql) = transaction.isolation_setup_sql() {
            let results = service.execute_query(sql).await.map_err(|_| {
                error(
                    PluginSessionErrorCode::OwnerUnavailable,
                    "SQLite session isolation could not be configured.",
                )
            })?;
            if results_have_error(&results) {
                return Err(error(
                    PluginSessionErrorCode::Unsupported,
                    "SQLite session isolation is unsupported by the target.",
                ));
            }
        }
        Ok(Arc::new(SqliteAgentSession {
            service: Mutex::new(Some(service)),
            transaction: Mutex::new(transaction),
        }))
    }
}

struct SqliteAgentSession {
    service: Mutex<Option<SqliteService>>,
    transaction: Mutex<SqlSessionTransactionTracker>,
}

#[async_trait]
impl PluginAgentSession for SqliteAgentSession {
    async fn call(
        &self,
        request: AgentSessionCallRequest,
    ) -> Result<AgentSessionCallResult, PluginSessionError> {
        let sql = request
            .input
            .get("sql")
            .and_then(Value::as_str)
            .filter(|sql| !sql.trim().is_empty())
            .ok_or_else(|| error(PluginSessionErrorCode::PolicyDenied, "SQL is required."))?;
        match request.capability.as_str() {
            "sqlite.query" if !sql_allows_read_only_query(sql, SqlDialect::Sqlite) => {
                return Err(error(
                    PluginSessionErrorCode::PolicyDenied,
                    "sqlite.query accepts read-oriented SQL only; use sqlite.exec.",
                ));
            }
            "sqlite.exec" if !request.destructive_acknowledged => {
                return Err(error(
                    PluginSessionErrorCode::PolicyDenied,
                    "sqlite.exec requires destructive acknowledgement.",
                ));
            }
            "sqlite.query" | "sqlite.exec" => {}
            _ => {
                return Err(error(
                    PluginSessionErrorCode::PolicyDenied,
                    "SQLite sessions accept sqlite.query and sqlite.exec.",
                ));
            }
        }
        let mut transaction = self.transaction.lock().await;
        let plan = transaction.plan(sql).map_err(|reason| {
            PluginSessionError::new(
                PluginSessionErrorCode::PolicyDenied,
                format!("SQLite transaction transition is invalid: {reason}"),
            )
        })?;
        let mut guard = self.service.lock().await;
        let service = guard.as_mut().ok_or_else(|| {
            error(
                PluginSessionErrorCode::OwnerUnavailable,
                "SQLite session is closed.",
            )
        })?;
        let results = service.execute_query(sql).await.map_err(|_| {
            error(
                PluginSessionErrorCode::OwnerUnavailable,
                "SQLite session SQL failed.",
            )
        })?;
        if results_have_error(&results) {
            let rollback_succeeded = if transaction.failure_requires_rollback(&plan) {
                service
                    .execute_query("ROLLBACK")
                    .await
                    .is_ok_and(|results| !results_have_error(&results))
            } else {
                true
            };
            transaction.record_failure(&plan, rollback_succeeded);
        } else {
            transaction.record_success(plan);
        }
        let transaction_status = transaction.status();
        let max_rows = request
            .input
            .get("max_rows")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 1000) as usize;
        AgentSessionCallResult::bounded(
            request.call_id,
            json!({
                "statements": results_json(results, max_rows),
                "max_rows": max_rows,
                "transaction": transaction_status
            }),
            request.output_limit_bytes,
        )
    }

    async fn health(&self) -> Result<PluginSessionHealth, PluginSessionError> {
        Ok(if self.service.lock().await.is_some() {
            PluginSessionHealth::Ready
        } else {
            PluginSessionHealth::Closed
        })
    }

    async fn cancel(&self, _call_id: &str) -> Result<(), PluginSessionError> {
        close_service(&self.transaction, &self.service).await;
        Ok(())
    }

    async fn close(&self, _reason: String) -> Result<(), PluginSessionError> {
        close_service(&self.transaction, &self.service).await;
        Ok(())
    }
}

async fn close_service(
    transaction: &Mutex<SqlSessionTransactionTracker>,
    service: &Mutex<Option<SqliteService>>,
) {
    let _transaction = transaction.lock().await;
    if let Some(mut service) = service.lock().await.take() {
        let _ = service.execute_query("ROLLBACK").await;
    }
}

fn results_have_error(results: &[StatementResult]) -> bool {
    results
        .iter()
        .any(|result| matches!(result, StatementResult::Error(_)))
}

fn results_json(results: Vec<StatementResult>, max_rows: usize) -> Vec<Value> {
    results
        .into_iter()
        .map(|result| match result {
            StatementResult::Select { columns, rows } => {
                let source_count = rows.len();
                json!({
                    "kind": "rows",
                    "columns": columns.iter().map(|column| column.name.clone()).collect::<Vec<_>>(),
                    "rows": rows.into_iter().take(max_rows).map(|row| row.values.into_iter().map(|value| voidb_core::formatters::cell_value_to_json(&value)).collect::<Vec<_>>()).collect::<Vec<_>>(),
                    "source_count": source_count,
                    "truncated": source_count > max_rows,
                })
            }
            StatementResult::Affected(rows) => json!({ "kind": "affected", "rows": rows }),
            StatementResult::Empty => json!({ "kind": "empty" }),
            StatementResult::Error(_) => json!({ "kind": "error", "message": "SQLite statement failed." }),
        })
        .collect()
}

fn error(code: PluginSessionErrorCode, message: &str) -> PluginSessionError {
    PluginSessionError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use voidb_core::{
        AgentSessionBinding, AgentSessionOpenRequest, AgentSessionRef, PluginAgentSessionFactory,
    };

    #[tokio::test]
    async fn memory_database_temp_state_survives_calls() {
        let factory = SqliteAgentSessionFactory::new(SqliteConfig::new(":memory:".into()));
        let session = factory.open(context()).await.expect("open");
        let created = call(
            &session,
            "sqlite.exec",
            "CREATE TEMP TABLE kept(value TEXT); INSERT INTO kept VALUES ('yes')",
            true,
        )
        .await;
        assert_eq!(created.output["transaction"]["state"], "idle");
        let result = call(&session, "sqlite.query", "SELECT value FROM kept", false).await;
        assert_eq!(result.output["statements"][0]["rows"][0][0], "yes");
        session.close("test".into()).await.expect("close");
        assert_eq!(session.health().await.unwrap(), PluginSessionHealth::Closed);
    }

    #[tokio::test]
    async fn close_rolls_back_open_file_transaction() {
        let path = std::env::temp_dir().join(format!(
            "voidb-sqlite-session-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let config = SqliteConfig::new(path.to_string_lossy().into_owned());
        let factory = SqliteAgentSessionFactory::new(config.clone());
        let session = factory.open(context()).await.expect("open");
        let begun = call(
            &session,
            "sqlite.exec",
            "BEGIN; CREATE TABLE rolled_back(value TEXT)",
            true,
        )
        .await;
        assert_eq!(begun.output["transaction"]["state"], "active");
        session.close("test".into()).await.expect("close");

        let reopened = factory.open(context()).await.expect("reopen");
        let result = call(
            &reopened,
            "sqlite.query",
            "SELECT value FROM rolled_back",
            false,
        )
        .await;
        assert_eq!(result.output["statements"][0]["kind"], "error");
        reopened.close("test".into()).await.expect("close");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn cancel_rolls_back_open_file_transaction() {
        let path = std::env::temp_dir().join(format!(
            "voidb-sqlite-cancel-session-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let config = SqliteConfig::new(path.to_string_lossy().into_owned());
        let factory = SqliteAgentSessionFactory::new(config);
        let session = factory.open(context()).await.expect("open");
        call(
            &session,
            "sqlite.exec",
            "BEGIN; CREATE TABLE cancelled(value TEXT)",
            true,
        )
        .await;
        session
            .cancel("call:transaction")
            .await
            .expect("cancel transaction");
        assert_eq!(
            session.health().await.expect("cancelled health"),
            PluginSessionHealth::Closed
        );

        let reopened = factory.open(context()).await.expect("reopen");
        let result = call(
            &reopened,
            "sqlite.query",
            "SELECT value FROM cancelled",
            false,
        )
        .await;
        assert_eq!(result.output["statements"][0]["kind"], "error");
        reopened.close("test".into()).await.expect("close");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn statement_error_auto_rolls_back_and_reports_transaction_state() {
        let factory = SqliteAgentSessionFactory::new(SqliteConfig::new(":memory:".into()));
        let session = factory.open(context()).await.expect("open");
        let begun = call(
            &session,
            "sqlite.exec",
            "BEGIN; CREATE TABLE transient(value TEXT)",
            true,
        )
        .await;
        assert_eq!(begun.output["transaction"]["state"], "active");

        let failed = call(
            &session,
            "sqlite.exec",
            "INSERT INTO missing_table VALUES ('no')",
            true,
        )
        .await;

        assert_eq!(failed.output["statements"][0]["kind"], "error");
        assert_eq!(failed.output["transaction"]["state"], "idle");
        assert_eq!(
            failed.output["transaction"]["last_transition"],
            "auto_rollback_error"
        );
        let table = call(
            &session,
            "sqlite.query",
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'transient'",
            false,
        )
        .await;
        assert_eq!(table.output["statements"][0]["rows"][0][0], 0);
        session.close("test".into()).await.expect("close");
    }

    fn context() -> AgentSessionOpenContext {
        let purpose = PluginSessionPurpose::DatabaseTransaction;
        AgentSessionOpenContext {
            binding: AgentSessionBinding {
                grant_id: "grant".into(),
                profile_id: "profile".into(),
                plugin_id: "sqlite".into(),
                purpose: purpose.clone(),
                allowed_capabilities: vec!["sqlite.query".into(), "sqlite.exec".into()],
                host_generation: 1,
            },
            request: AgentSessionOpenRequest {
                purpose,
                capabilities: vec!["sqlite.query".into(), "sqlite.exec".into()],
                lease_seconds: 60,
                concurrency: Default::default(),
                destructive_acknowledged: false,
                input: Value::Null,
            },
            lease_expires_at: Utc::now() + Duration::seconds(60),
        }
    }

    async fn call(
        session: &Arc<dyn PluginAgentSession>,
        capability: &str,
        sql: &str,
        ack: bool,
    ) -> AgentSessionCallResult {
        session
            .call(AgentSessionCallRequest {
                session: AgentSessionRef::new("session", 1),
                call_id: sql.into(),
                capability: capability.into(),
                input: json!({"sql": sql}),
                destructive_acknowledged: ack,
                timeout_ms: Some(1000),
                output_limit_bytes: 64 * 1024,
            })
            .await
            .expect("call")
    }
}
