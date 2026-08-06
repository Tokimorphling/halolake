use anyhow::{Context, Result, bail};
use halolake_tokiame_protocol::{DispatchJob, JobEvent};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct AgentJournal {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct JournalJob {
    pub dispatch:       DispatchJob,
    pub status:         String,
    pub prompt_id:      Option<String>,
    pub event_sequence: u64,
    pub pending_event:  Option<JobEvent>,
}

impl AgentJournal {
    pub async fn open(url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(url)
            .with_context(|| format!("parse agent SQLite URL {url}"))?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .context("open agent journal")?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tokiame_jobs (job_id TEXT PRIMARY KEY, dispatch_json TEXT \
             NOT NULL, status TEXT NOT NULL, prompt_id TEXT NULL, event_sequence BIGINT NOT NULL \
             DEFAULT 0, pending_event_json TEXT NULL, updated_at BIGINT NOT NULL)",
        )
        .execute(&pool)
        .await
        .context("migrate agent journal")?;
        Ok(Self { pool })
    }

    pub async fn upsert_dispatch(&self, dispatch: &DispatchJob) -> Result<JournalJob> {
        let payload = serde_json::to_string(dispatch)?;
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query(
            "SELECT dispatch_json, status, prompt_id, event_sequence, pending_event_json FROM \
             tokiame_jobs WHERE job_id = ?",
        )
        .bind(&dispatch.job_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(decode_job)
        .transpose()?;

        let record = if let Some(mut record) = existing {
            if !same_dispatch_identity(&record.dispatch, dispatch) {
                bail!("job id already exists with different immutable dispatch fields");
            }
            if !matches!(record.status.as_str(), "completed" | "failed" | "cancelled") {
                sqlx::query(
                    "UPDATE tokiame_jobs SET dispatch_json = ?, updated_at = ? WHERE job_id = ?",
                )
                .bind(payload)
                .bind(now_unix_ms())
                .bind(&dispatch.job_id)
                .execute(&mut *tx)
                .await?;
                record.dispatch = dispatch.clone();
            }
            record
        } else {
            sqlx::query(
                "INSERT INTO tokiame_jobs (job_id, dispatch_json, status, event_sequence, \
                 updated_at) VALUES (?, ?, 'received', 0, ?)",
            )
            .bind(&dispatch.job_id)
            .bind(payload)
            .bind(now_unix_ms())
            .execute(&mut *tx)
            .await?;
            JournalJob {
                dispatch:       dispatch.clone(),
                status:         "received".into(),
                prompt_id:      None,
                event_sequence: 0,
                pending_event:  None,
            }
        };
        tx.commit().await?;
        Ok(record)
    }

    pub async fn get(&self, job_id: &str) -> Result<Option<JournalJob>> {
        let row = sqlx::query(
            "SELECT dispatch_json, status, prompt_id, event_sequence, pending_event_json FROM \
             tokiame_jobs WHERE job_id = ?",
        )
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(decode_job).transpose()
    }

    pub async fn unfinished(&self) -> Result<Vec<JournalJob>> {
        let rows = sqlx::query(
            "SELECT dispatch_json, status, prompt_id, event_sequence, pending_event_json FROM \
             tokiame_jobs WHERE status NOT IN ('completed', 'failed', 'cancelled') OR \
             pending_event_json IS NOT NULL ORDER BY updated_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(decode_job).collect()
    }

    pub async fn set_status(&self, job_id: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE tokiame_jobs SET status = ?, updated_at = ? WHERE job_id = ?")
            .bind(status)
            .bind(now_unix_ms())
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_prompt_id(&self, job_id: &str, prompt_id: &str) -> Result<()> {
        sqlx::query("UPDATE tokiame_jobs SET prompt_id = ?, updated_at = ? WHERE job_id = ?")
            .bind(prompt_id)
            .bind(now_unix_ms())
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_pending_event(&self, event: &JobEvent, status: Option<&str>) -> Result<()> {
        let sequence = i64::try_from(event.sequence).unwrap_or(i64::MAX);
        let payload = serde_json::to_string(event)?;
        if let Some(status) = status {
            sqlx::query(
                "UPDATE tokiame_jobs SET status = ?, event_sequence = ?, pending_event_json = ?, \
                 updated_at = ? WHERE job_id = ?",
            )
            .bind(status)
            .bind(sequence)
            .bind(payload)
            .bind(now_unix_ms())
            .bind(&event.job_id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE tokiame_jobs SET event_sequence = ?, pending_event_json = ?, updated_at = \
                 ? WHERE job_id = ?",
            )
            .bind(sequence)
            .bind(payload)
            .bind(now_unix_ms())
            .bind(&event.job_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn clear_pending_event(&self, job_id: &str, sequence: u64) -> Result<()> {
        sqlx::query(
            "UPDATE tokiame_jobs SET pending_event_json = NULL, updated_at = ? WHERE job_id = ? \
             AND event_sequence = ?",
        )
        .bind(now_unix_ms())
        .bind(job_id)
        .bind(i64::try_from(sequence).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn same_dispatch_identity(previous: &DispatchJob, next: &DispatchJob) -> bool {
    previous.job_id == next.job_id
        && previous.attempt == next.attempt
        && previous.model == next.model
        && previous.workflow_id == next.workflow_id
        && previous.workflow_version == next.workflow_version
        && previous.params == next.params
        && previous.inputs.len() == next.inputs.len()
        && previous
            .inputs
            .iter()
            .zip(&next.inputs)
            .all(|(left, right)| {
                left.artifact_id == right.artifact_id
                    && left.name == right.name
                    && left.content_type == right.content_type
                    && left.size_bytes == right.size_bytes
                    && left.sha256.eq_ignore_ascii_case(&right.sha256)
            })
}

fn decode_job(row: sqlx::sqlite::SqliteRow) -> Result<JournalJob> {
    let dispatch_json: String = row.try_get("dispatch_json")?;
    let pending_event_json: Option<String> = row.try_get("pending_event_json")?;
    let sequence: i64 = row.try_get("event_sequence")?;
    Ok(JournalJob {
        dispatch:       serde_json::from_str(&dispatch_json)?,
        status:         row.try_get("status")?,
        prompt_id:      row.try_get("prompt_id")?,
        event_sequence: sequence.max(0) as u64,
        pending_event:  pending_event_json
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?,
    })
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_tokiame_protocol::{JobInput, PresignedRequest};
    use std::collections::BTreeMap;

    fn dispatch(command_id: &str, url: &str) -> DispatchJob {
        DispatchJob {
            command_id:       command_id.into(),
            job_id:           "job-1".into(),
            attempt:          1,
            model:            "comfy-edit".into(),
            workflow_id:      "image-edit".into(),
            workflow_version: "v1".into(),
            params:           serde_json::json!({"prompt":"hello"}),
            inputs:           vec![JobInput {
                artifact_id:  "input-1".into(),
                name:         "source.png".into(),
                content_type: "image/png".into(),
                size_bytes:   42,
                sha256:       "a".repeat(64),
                download:     PresignedRequest {
                    method:             "GET".into(),
                    url:                url.into(),
                    headers:            BTreeMap::new(),
                    expires_at_unix_ms: 1,
                },
            }],
        }
    }

    #[tokio::test]
    async fn retry_refreshes_ephemeral_dispatch_fields_but_rejects_identity_changes() {
        let journal = AgentJournal::open("sqlite::memory:").await.unwrap();
        journal
            .upsert_dispatch(&dispatch("command-1", "https://objects/first"))
            .await
            .unwrap();

        let refreshed = journal
            .upsert_dispatch(&dispatch("command-2", "https://objects/second"))
            .await
            .unwrap();
        assert_eq!(refreshed.dispatch.command_id, "command-2");
        assert_eq!(
            refreshed.dispatch.inputs[0].download.url,
            "https://objects/second"
        );

        let mut conflicting = dispatch("command-3", "https://objects/third");
        conflicting.params = serde_json::json!({"prompt":"changed"});
        assert!(journal.upsert_dispatch(&conflicting).await.is_err());
    }
}
