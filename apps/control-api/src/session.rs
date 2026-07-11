//! Durable session store (memory / SQLite / MySQL / Postgres).
//!
//! Cookies are signed as `{session_id}.{hmac_sha256_base64url}` when a session
//! secret is configured. DB backends use row-level upsert/delete.

use data_encoding::BASE64URL_NOPAD;
use halolake_control_plane::ManagementError;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, RwLock},
};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Matches new-api SecureVerificationTimeout (5 minutes).
pub(crate) const SECURE_VERIFICATION_TIMEOUT_SECS: i64 = 300;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionRecord {
    pub(crate) user_id:            Option<u64>,
    pub(crate) pending_user_id:    Option<u64>,
    pub(crate) secure_verified_at: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SecureVerificationError {
    Required,
    Expired,
    Unavailable,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSigner {
    secret: Arc<[u8]>,
}

impl SessionSigner {
    pub(crate) fn new(secret: impl Into<String>) -> Self {
        let secret = secret.into();
        Self {
            secret: Arc::from(secret.into_bytes().into_boxed_slice()),
        }
    }

    pub(crate) fn sign(&self, session_id: &str) -> String {
        if self.secret.is_empty() {
            return session_id.to_string();
        }
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(session_id.as_bytes());
        let sig = BASE64URL_NOPAD.encode(&mac.finalize().into_bytes());
        format!("{session_id}.{sig}")
    }

    pub(crate) fn verify_cookie_value<'a>(&self, value: &'a str) -> Option<&'a str> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if self.secret.is_empty() {
            // Dev / memory mode without secret: raw session id only.
            return (!value.contains('.')).then_some(value);
        }
        let (session_id, sig) = value.rsplit_once('.')?;
        if session_id.is_empty() || sig.is_empty() {
            return None;
        }
        let mut mac = HmacSha256::new_from_slice(&self.secret).ok()?;
        mac.update(session_id.as_bytes());
        let expected = BASE64URL_NOPAD.encode(&mac.finalize().into_bytes());
        if constant_time_eq(sig.as_bytes(), expected.as_bytes()) {
            Some(session_id)
        } else {
            None
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, Clone)]
pub(crate) enum SessionStore {
    Memory(MemorySessionStore),
    Sqlite(SqliteSessionStore),
    MySql(MySqlSessionStore),
    Postgres(PostgresSessionStore),
}

impl SessionStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemorySessionStore::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteSessionStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlSessionStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(PostgresSessionStore::connect(url).await?))
    }

    pub(crate) fn create_anonymous(&self) -> Result<String, ManagementError> {
        match self {
            Self::Memory(s) => s.create_anonymous(),
            Self::Sqlite(s) => s.create_anonymous(),
            Self::MySql(s) => s.create_anonymous(),
            Self::Postgres(s) => s.create_anonymous(),
        }
    }

    pub(crate) fn create(&self, user_id: u64) -> Result<String, ManagementError> {
        match self {
            Self::Memory(s) => s.create(user_id),
            Self::Sqlite(s) => s.create(user_id),
            Self::MySql(s) => s.create(user_id),
            Self::Postgres(s) => s.create(user_id),
        }
    }

    pub(crate) fn create_pending(&self, user_id: u64) -> Result<String, ManagementError> {
        match self {
            Self::Memory(s) => s.create_pending(user_id),
            Self::Sqlite(s) => s.create_pending(user_id),
            Self::MySql(s) => s.create_pending(user_id),
            Self::Postgres(s) => s.create_pending(user_id),
        }
    }

    pub(crate) fn get(&self, session_id: &str) -> Result<Option<u64>, ManagementError> {
        match self {
            Self::Memory(s) => s.get(session_id),
            Self::Sqlite(s) => s.memory.get(session_id),
            Self::MySql(s) => s.memory.get(session_id),
            Self::Postgres(s) => s.memory.get(session_id),
        }
    }

    pub(crate) fn has_session(&self, session_id: &str) -> Result<bool, ManagementError> {
        match self {
            Self::Memory(s) => s.has_session(session_id),
            Self::Sqlite(s) => s.memory.has_session(session_id),
            Self::MySql(s) => s.memory.has_session(session_id),
            Self::Postgres(s) => s.memory.has_session(session_id),
        }
    }

    pub(crate) fn mark_secure_verified(&self, session_id: &str) -> Result<(), ManagementError> {
        match self {
            Self::Memory(s) => s.mark_secure_verified(session_id),
            Self::Sqlite(s) => s.mark_secure_verified(session_id),
            Self::MySql(s) => s.mark_secure_verified(session_id),
            Self::Postgres(s) => s.mark_secure_verified(session_id),
        }
    }

    pub(crate) fn require_secure_verified(
        &self,
        session_id: &str,
    ) -> Result<(), SecureVerificationError> {
        match self {
            Self::Memory(s) => s.require_secure_verified(session_id),
            Self::Sqlite(s) => s.require_secure_verified(session_id),
            Self::MySql(s) => s.require_secure_verified(session_id),
            Self::Postgres(s) => s.require_secure_verified(session_id),
        }
    }

    pub(crate) fn passkey_session_id_from_headers(
        &self,
        headers: &http::HeaderMap,
        create_if_missing: bool,
        signer: &SessionSigner,
    ) -> Result<Option<(String, bool)>, ManagementError> {
        if let Some(session_id) = cookie_session_id(headers, signer) {
            if self.has_session(session_id)? {
                return Ok(Some((session_id.to_string(), false)));
            }
        }
        if create_if_missing {
            return self
                .create_anonymous()
                .map(|session_id| Some((session_id, true)));
        }
        Ok(None)
    }

    pub(crate) fn pending_user_id_from_headers(
        &self,
        headers: &http::HeaderMap,
        signer: &SessionSigner,
    ) -> Result<Option<u64>, ManagementError> {
        let Some(session_id) = cookie_session_id(headers, signer) else {
            return Ok(None);
        };
        match self {
            Self::Memory(s) => s.pending_user_id(session_id),
            Self::Sqlite(s) => s.memory.pending_user_id(session_id),
            Self::MySql(s) => s.memory.pending_user_id(session_id),
            Self::Postgres(s) => s.memory.pending_user_id(session_id),
        }
    }

    pub(crate) fn promote_pending_from_headers(
        &self,
        headers: &http::HeaderMap,
        signer: &SessionSigner,
    ) -> Result<Option<String>, ManagementError> {
        match self {
            Self::Memory(s) => s.promote_pending_from_headers(headers, signer),
            Self::Sqlite(s) => s.promote_pending_from_headers(headers, signer),
            Self::MySql(s) => s.promote_pending_from_headers(headers, signer),
            Self::Postgres(s) => s.promote_pending_from_headers(headers, signer),
        }
    }

    pub(crate) fn remove(&self, session_id: &str) -> Result<(), ManagementError> {
        match self {
            Self::Memory(s) => s.remove(session_id),
            Self::Sqlite(s) => s.remove(session_id),
            Self::MySql(s) => s.remove(session_id),
            Self::Postgres(s) => s.remove(session_id),
        }
    }

    pub(crate) fn user_id_from_headers(
        &self,
        headers: &http::HeaderMap,
        signer: &SessionSigner,
    ) -> Result<Option<u64>, ManagementError> {
        let Some(session_id) = cookie_session_id(headers, signer) else {
            return Ok(None);
        };
        self.get(session_id)
    }

    pub(crate) fn remove_from_headers(&self, headers: &http::HeaderMap, signer: &SessionSigner) {
        if let Some(session_id) = cookie_session_id(headers, signer) {
            let _ = self.remove(session_id);
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemorySessionStore {
    inner: Arc<RwLock<HashMap<String, SessionRecord>>>,
}

impl MemorySessionStore {
    fn insert(&self, session_id: String, record: SessionRecord) -> Result<(), ManagementError> {
        self.inner
            .write()
            .map_err(|_| ManagementError::Poisoned("sessions"))?
            .insert(session_id, record);
        Ok(())
    }

    pub(crate) fn create_anonymous(&self) -> Result<String, ManagementError> {
        let session_id = Uuid::new_v4().simple().to_string();
        self.insert(session_id.clone(), SessionRecord {
            user_id:            None,
            pending_user_id:    None,
            secure_verified_at: None,
        })?;
        Ok(session_id)
    }

    pub(crate) fn create(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = Uuid::new_v4().simple().to_string();
        self.insert(session_id.clone(), SessionRecord {
            user_id:            Some(user_id),
            pending_user_id:    None,
            secure_verified_at: None,
        })?;
        Ok(session_id)
    }

    pub(crate) fn create_pending(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = Uuid::new_v4().simple().to_string();
        self.insert(session_id.clone(), SessionRecord {
            user_id:            None,
            pending_user_id:    Some(user_id),
            secure_verified_at: None,
        })?;
        Ok(session_id)
    }

    pub(crate) fn get(&self, session_id: &str) -> Result<Option<u64>, ManagementError> {
        self.inner
            .read()
            .map(|sessions| sessions.get(session_id).and_then(|r| r.user_id))
            .map_err(|_| ManagementError::Poisoned("sessions"))
    }

    pub(crate) fn has_session(&self, session_id: &str) -> Result<bool, ManagementError> {
        self.inner
            .read()
            .map(|sessions| sessions.contains_key(session_id))
            .map_err(|_| ManagementError::Poisoned("sessions"))
    }

    pub(crate) fn mark_secure_verified(&self, session_id: &str) -> Result<(), ManagementError> {
        let mut sessions = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("sessions"))?;
        if let Some(record) = sessions.get_mut(session_id) {
            record.secure_verified_at = Some(now_unix());
        }
        Ok(())
    }

    pub(crate) fn require_secure_verified(
        &self,
        session_id: &str,
    ) -> Result<(), SecureVerificationError> {
        let mut sessions = self
            .inner
            .write()
            .map_err(|_| SecureVerificationError::Unavailable)?;
        let Some(record) = sessions.get_mut(session_id) else {
            return Err(SecureVerificationError::Required);
        };
        let Some(verified_at) = record.secure_verified_at else {
            return Err(SecureVerificationError::Required);
        };
        if now_unix().saturating_sub(verified_at) >= SECURE_VERIFICATION_TIMEOUT_SECS {
            record.secure_verified_at = None;
            return Err(SecureVerificationError::Expired);
        }
        Ok(())
    }

    fn pending_user_id(&self, session_id: &str) -> Result<Option<u64>, ManagementError> {
        self.inner
            .read()
            .map(|s| s.get(session_id).and_then(|r| r.pending_user_id))
            .map_err(|_| ManagementError::Poisoned("sessions"))
    }

    pub(crate) fn promote_pending_from_headers(
        &self,
        headers: &http::HeaderMap,
        signer: &SessionSigner,
    ) -> Result<Option<String>, ManagementError> {
        let Some(session_id) = cookie_session_id(headers, signer) else {
            return Ok(None);
        };
        let mut sessions = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("sessions"))?;
        let Some(record) = sessions.get_mut(session_id) else {
            return Ok(None);
        };
        let Some(user_id) = record.pending_user_id.take() else {
            return Ok(None);
        };
        record.user_id = Some(user_id);
        Ok(Some(session_id.to_string()))
    }

    pub(crate) fn remove(&self, session_id: &str) -> Result<(), ManagementError> {
        self.inner
            .write()
            .map_err(|_| ManagementError::Poisoned("sessions"))?
            .remove(session_id);
        Ok(())
    }

    fn get_record(&self, session_id: &str) -> Result<Option<SessionRecord>, ManagementError> {
        self.inner
            .read()
            .map(|s| s.get(session_id).copied())
            .map_err(|_| ManagementError::Poisoned("sessions"))
    }

    fn load_all(&self, map: HashMap<String, SessionRecord>) -> Result<(), ManagementError> {
        *self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("sessions"))? = map;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteSessionStore {
    pool:   SqlitePool,
    memory: MemorySessionStore,
}

impl SqliteSessionStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(storage_err)?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                user_id INTEGER,
                pending_user_id INTEGER,
                secure_verified_at INTEGER,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .map_err(storage_err)?;
        let map = load_sessions_sqlite(&pool).await?;
        let memory = MemorySessionStore::default();
        memory.load_all(map)?;
        Ok(Self { pool, memory })
    }

    fn create_anonymous(&self) -> Result<String, ManagementError> {
        let session_id = self.memory.create_anonymous()?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn create(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = self.memory.create(user_id)?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn create_pending(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = self.memory.create_pending(user_id)?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn mark_secure_verified(&self, session_id: &str) -> Result<(), ManagementError> {
        self.memory.mark_secure_verified(session_id)?;
        self.upsert_row(session_id)
    }

    fn require_secure_verified(&self, session_id: &str) -> Result<(), SecureVerificationError> {
        let result = self.memory.require_secure_verified(session_id);
        if matches!(result, Err(SecureVerificationError::Expired)) {
            let _ = self.upsert_row(session_id);
        }
        result
    }

    fn promote_pending_from_headers(
        &self,
        headers: &http::HeaderMap,
        signer: &SessionSigner,
    ) -> Result<Option<String>, ManagementError> {
        let out = self.memory.promote_pending_from_headers(headers, signer)?;
        if let Some(ref session_id) = out {
            self.upsert_row(session_id)?;
        }
        Ok(out)
    }

    fn remove(&self, session_id: &str) -> Result<(), ManagementError> {
        self.memory.remove(session_id)?;
        block_on_db(async {
            sqlx::query("DELETE FROM sessions WHERE session_id = ?")
                .bind(session_id)
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
            Ok(())
        })
    }

    fn upsert_row(&self, session_id: &str) -> Result<(), ManagementError> {
        let Some(record) = self.memory.get_record(session_id)? else {
            return Ok(());
        };
        let now = now_unix();
        block_on_db(async {
            sqlx::query(
                "INSERT INTO sessions (session_id, user_id, pending_user_id, secure_verified_at, \
                 updated_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(session_id) DO UPDATE SET
                    user_id = excluded.user_id,
                    pending_user_id = excluded.pending_user_id,
                    secure_verified_at = excluded.secure_verified_at,
                    updated_at = excluded.updated_at",
            )
            .bind(session_id)
            .bind(record.user_id.map(|v| v as i64))
            .bind(record.pending_user_id.map(|v| v as i64))
            .bind(record.secure_verified_at)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
            Ok(())
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlSessionStore {
    pool:   MySqlPool,
    memory: MemorySessionStore,
}

impl MySqlSessionStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                session_id VARCHAR(64) PRIMARY KEY,
                user_id BIGINT,
                pending_user_id BIGINT,
                secure_verified_at BIGINT,
                updated_at BIGINT NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .map_err(storage_err)?;
        let map = load_sessions_mysql(&pool).await?;
        let memory = MemorySessionStore::default();
        memory.load_all(map)?;
        Ok(Self { pool, memory })
    }

    fn create_anonymous(&self) -> Result<String, ManagementError> {
        let session_id = self.memory.create_anonymous()?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn create(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = self.memory.create(user_id)?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn create_pending(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = self.memory.create_pending(user_id)?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn mark_secure_verified(&self, session_id: &str) -> Result<(), ManagementError> {
        self.memory.mark_secure_verified(session_id)?;
        self.upsert_row(session_id)
    }

    fn require_secure_verified(&self, session_id: &str) -> Result<(), SecureVerificationError> {
        let result = self.memory.require_secure_verified(session_id);
        if matches!(result, Err(SecureVerificationError::Expired)) {
            let _ = self.upsert_row(session_id);
        }
        result
    }

    fn promote_pending_from_headers(
        &self,
        headers: &http::HeaderMap,
        signer: &SessionSigner,
    ) -> Result<Option<String>, ManagementError> {
        let out = self.memory.promote_pending_from_headers(headers, signer)?;
        if let Some(ref session_id) = out {
            self.upsert_row(session_id)?;
        }
        Ok(out)
    }

    fn remove(&self, session_id: &str) -> Result<(), ManagementError> {
        self.memory.remove(session_id)?;
        block_on_db(async {
            sqlx::query("DELETE FROM sessions WHERE session_id = ?")
                .bind(session_id)
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
            Ok(())
        })
    }

    fn upsert_row(&self, session_id: &str) -> Result<(), ManagementError> {
        let Some(record) = self.memory.get_record(session_id)? else {
            return Ok(());
        };
        let now = now_unix();
        block_on_db(async {
            sqlx::query(
                "INSERT INTO sessions (session_id, user_id, pending_user_id, secure_verified_at, \
                 updated_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON DUPLICATE KEY UPDATE
                    user_id = VALUES(user_id),
                    pending_user_id = VALUES(pending_user_id),
                    secure_verified_at = VALUES(secure_verified_at),
                    updated_at = VALUES(updated_at)",
            )
            .bind(session_id)
            .bind(record.user_id.map(|v| v as i64))
            .bind(record.pending_user_id.map(|v| v as i64))
            .bind(record.secure_verified_at)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
            Ok(())
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresSessionStore {
    pool:   PgPool,
    memory: MemorySessionStore,
}

impl PostgresSessionStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                user_id BIGINT,
                pending_user_id BIGINT,
                secure_verified_at BIGINT,
                updated_at BIGINT NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .map_err(storage_err)?;
        let map = load_sessions_pg(&pool).await?;
        let memory = MemorySessionStore::default();
        memory.load_all(map)?;
        Ok(Self { pool, memory })
    }

    fn create_anonymous(&self) -> Result<String, ManagementError> {
        let session_id = self.memory.create_anonymous()?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn create(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = self.memory.create(user_id)?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn create_pending(&self, user_id: u64) -> Result<String, ManagementError> {
        let session_id = self.memory.create_pending(user_id)?;
        self.upsert_row(&session_id)?;
        Ok(session_id)
    }

    fn mark_secure_verified(&self, session_id: &str) -> Result<(), ManagementError> {
        self.memory.mark_secure_verified(session_id)?;
        self.upsert_row(session_id)
    }

    fn require_secure_verified(&self, session_id: &str) -> Result<(), SecureVerificationError> {
        let result = self.memory.require_secure_verified(session_id);
        if matches!(result, Err(SecureVerificationError::Expired)) {
            let _ = self.upsert_row(session_id);
        }
        result
    }

    fn promote_pending_from_headers(
        &self,
        headers: &http::HeaderMap,
        signer: &SessionSigner,
    ) -> Result<Option<String>, ManagementError> {
        let out = self.memory.promote_pending_from_headers(headers, signer)?;
        if let Some(ref session_id) = out {
            self.upsert_row(session_id)?;
        }
        Ok(out)
    }

    fn remove(&self, session_id: &str) -> Result<(), ManagementError> {
        self.memory.remove(session_id)?;
        block_on_db(async {
            sqlx::query("DELETE FROM sessions WHERE session_id = $1")
                .bind(session_id)
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
            Ok(())
        })
    }

    fn upsert_row(&self, session_id: &str) -> Result<(), ManagementError> {
        let Some(record) = self.memory.get_record(session_id)? else {
            return Ok(());
        };
        let now = now_unix();
        block_on_db(async {
            sqlx::query(
                "INSERT INTO sessions (session_id, user_id, pending_user_id, secure_verified_at, \
                 updated_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (session_id) DO UPDATE SET
                    user_id = EXCLUDED.user_id,
                    pending_user_id = EXCLUDED.pending_user_id,
                    secure_verified_at = EXCLUDED.secure_verified_at,
                    updated_at = EXCLUDED.updated_at",
            )
            .bind(session_id)
            .bind(record.user_id.map(|v| v as i64))
            .bind(record.pending_user_id.map(|v| v as i64))
            .bind(record.secure_verified_at)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
            Ok(())
        })
    }
}

async fn load_sessions_sqlite(
    pool: &SqlitePool,
) -> Result<HashMap<String, SessionRecord>, ManagementError> {
    let rows = sqlx::query(
        "SELECT session_id, user_id, pending_user_id, secure_verified_at FROM sessions",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?;
    let mut map = HashMap::new();
    for row in rows {
        let session_id: String = row.try_get("session_id").map_err(storage_err)?;
        let user_id: Option<i64> = row.try_get("user_id").map_err(storage_err)?;
        let pending_user_id: Option<i64> = row.try_get("pending_user_id").map_err(storage_err)?;
        let secure_verified_at: Option<i64> =
            row.try_get("secure_verified_at").map_err(storage_err)?;
        map.insert(session_id, SessionRecord {
            user_id: user_id.map(|v| v.max(0) as u64),
            pending_user_id: pending_user_id.map(|v| v.max(0) as u64),
            secure_verified_at,
        });
    }
    Ok(map)
}

async fn load_sessions_mysql(
    pool: &MySqlPool,
) -> Result<HashMap<String, SessionRecord>, ManagementError> {
    let rows = sqlx::query(
        "SELECT session_id, user_id, pending_user_id, secure_verified_at FROM sessions",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?;
    let mut map = HashMap::new();
    for row in rows {
        let session_id: String = row.try_get("session_id").map_err(storage_err)?;
        let user_id: Option<i64> = row.try_get("user_id").map_err(storage_err)?;
        let pending_user_id: Option<i64> = row.try_get("pending_user_id").map_err(storage_err)?;
        let secure_verified_at: Option<i64> =
            row.try_get("secure_verified_at").map_err(storage_err)?;
        map.insert(session_id, SessionRecord {
            user_id: user_id.map(|v| v.max(0) as u64),
            pending_user_id: pending_user_id.map(|v| v.max(0) as u64),
            secure_verified_at,
        });
    }
    Ok(map)
}

async fn load_sessions_pg(
    pool: &PgPool,
) -> Result<HashMap<String, SessionRecord>, ManagementError> {
    let rows = sqlx::query(
        "SELECT session_id, user_id, pending_user_id, secure_verified_at FROM sessions",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?;
    let mut map = HashMap::new();
    for row in rows {
        let session_id: String = row.try_get("session_id").map_err(storage_err)?;
        let user_id: Option<i64> = row.try_get("user_id").map_err(storage_err)?;
        let pending_user_id: Option<i64> = row.try_get("pending_user_id").map_err(storage_err)?;
        let secure_verified_at: Option<i64> =
            row.try_get("secure_verified_at").map_err(storage_err)?;
        map.insert(session_id, SessionRecord {
            user_id: user_id.map(|v| v.max(0) as u64),
            pending_user_id: pending_user_id.map(|v| v.max(0) as u64),
            secure_verified_at,
        });
    }
    Ok(map)
}

fn cookie_session_id<'a>(headers: &'a http::HeaderMap, signer: &SessionSigner) -> Option<&'a str> {
    let cookie = headers
        .get(http::header::COOKIE)
        .and_then(|value| value.to_str().ok())?;
    cookie.split(';').find_map(|part| {
        let part = part.trim();
        let value = part.strip_prefix("session=").map(str::trim)?;
        signer.verify_cookie_value(value)
    })
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn block_on_db<F, T>(fut: F) -> Result<T, ManagementError>
where
    F: std::future::Future<Output = Result<T, ManagementError>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| ManagementError::Storage(err.to_string()))?;
            rt.block_on(fut)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_session_cookie() {
        let signer = SessionSigner::new("test-secret");
        let cookie = signer.sign("abc123");
        assert!(cookie.starts_with("abc123."));
        assert_eq!(signer.verify_cookie_value(&cookie), Some("abc123"));
        assert_eq!(signer.verify_cookie_value("abc123"), None);
        assert_eq!(signer.verify_cookie_value("abc123.bad"), None);
    }

    #[test]
    fn empty_secret_accepts_raw_id_only() {
        let signer = SessionSigner::new("");
        assert_eq!(signer.sign("raw"), "raw");
        assert_eq!(signer.verify_cookie_value("raw"), Some("raw"));
        assert_eq!(signer.verify_cookie_value("raw.sig"), None);
    }
}
