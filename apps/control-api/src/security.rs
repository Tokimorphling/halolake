use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};

use bcrypt::{DEFAULT_COST, hash, verify};
use data_encoding::{BASE32_NOPAD, BASE64URL_NOPAD};
use halolake_control_plane::ManagementError;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use service_async::Service;
use sha1::Sha1;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;
use webauthn_rs::prelude::{
    DiscoverableAuthentication, DiscoverableKey, Passkey, PasskeyAuthentication,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential, Url, Webauthn,
    WebauthnBuilder,
};

use crate::storage::OptionStore;

const PASSKEY_DISABLED_MESSAGE: &str = "管理员未启用 Passkey 登录";
const PASSKEY_NOT_BOUND_MESSAGE: &str = "该用户尚未绑定 Passkey";
const BACKUP_CODE_LENGTH: usize = 8;
const BACKUP_CODE_COUNT: usize = 4;
const MAX_FAIL_ATTEMPTS: i32 = 5;
const LOCKOUT_DURATION_SECONDS: i64 = 300;
const TOTP_PERIOD_SECONDS: i64 = 30;
const TOTP_DIGITS: u32 = 6;
const TOTP_WINDOW: i64 = 1;
const PASSKEY_SESSION_TTL_SECONDS: i64 = 120;
const PASSKEY_READY_TTL_SECONDS: i64 = 300;
const PASSKEY_REGISTRATION_SESSION: &str = "passkey_registration_session";
const PASSKEY_LOGIN_SESSION: &str = "passkey_login_session";
const PASSKEY_VERIFY_SESSION: &str = "passkey_verify_session";
const PASSKEY_READY_SESSION: &str = "secure_passkey_ready_at";

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Clone)]
pub(crate) struct SecurityService {
    options: OptionStore,
    store: SecurityStore,
}

impl SecurityService {
    pub(crate) fn new(options: OptionStore, store: SecurityStore) -> Self {
        Self { options, store }
    }

    fn options(&self) -> Result<BTreeMap<String, String>, SecurityError> {
        self.options.values().map_err(SecurityError::Management)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SecurityStore {
    Memory(MemorySecurityStore),
    Sqlite(SqliteSecurityStore),
    MySql(MySqlSecurityStore),
    Postgres(PostgresSecurityStore),
}

impl SecurityStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemorySecurityStore::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteSecurityStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlSecurityStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(PostgresSecurityStore::connect(url).await?))
    }

    async fn get_two_fa(&self, user_id: u64) -> Result<Option<TwoFaRecord>, SecurityError> {
        match self {
            Self::Memory(store) => store.get_two_fa(user_id),
            Self::Sqlite(store) => store.get_two_fa(user_id).await,

            Self::MySql(store) => store.get_two_fa(user_id).await,

            Self::Postgres(store) => store.get_two_fa(user_id).await,
        }
    }

    async fn upsert_two_fa(&self, record: TwoFaRecord) -> Result<TwoFaRecord, SecurityError> {
        match self {
            Self::Memory(store) => store.upsert_two_fa(record),
            Self::Sqlite(store) => store.upsert_two_fa(record).await,

            Self::MySql(store) => store.upsert_two_fa(record).await,

            Self::Postgres(store) => store.upsert_two_fa(record).await,
        }
    }

    async fn delete_two_fa(&self, user_id: u64) -> Result<bool, SecurityError> {
        match self {
            Self::Memory(store) => store.delete_two_fa(user_id),
            Self::Sqlite(store) => store.delete_two_fa(user_id).await,

            Self::MySql(store) => store.delete_two_fa(user_id).await,

            Self::Postgres(store) => store.delete_two_fa(user_id).await,
        }
    }

    async fn replace_backup_codes(
        &self,
        user_id: u64,
        codes: &[String],
        now: i64,
    ) -> Result<(), SecurityError> {
        match self {
            Self::Memory(store) => store.replace_backup_codes(user_id, codes, now),
            Self::Sqlite(store) => store.replace_backup_codes(user_id, codes, now).await,

            Self::MySql(store) => store.replace_backup_codes(user_id, codes, now).await,

            Self::Postgres(store) => store.replace_backup_codes(user_id, codes, now).await,
        }
    }

    async fn unused_backup_code_count(&self, user_id: u64) -> Result<usize, SecurityError> {
        match self {
            Self::Memory(store) => store.unused_backup_code_count(user_id),
            Self::Sqlite(store) => store.unused_backup_code_count(user_id).await,

            Self::MySql(store) => store.unused_backup_code_count(user_id).await,

            Self::Postgres(store) => store.unused_backup_code_count(user_id).await,
        }
    }

    async fn consume_backup_code(&self, user_id: u64, code: &str) -> Result<bool, SecurityError> {
        match self {
            Self::Memory(store) => store.consume_backup_code(user_id, code),
            Self::Sqlite(store) => store.consume_backup_code(user_id, code).await,

            Self::MySql(store) => store.consume_backup_code(user_id, code).await,

            Self::Postgres(store) => store.consume_backup_code(user_id, code).await,
        }
    }

    async fn two_fa_stats(&self, total_users: usize) -> Result<TwoFaStats, SecurityError> {
        match self {
            Self::Memory(store) => store.two_fa_stats(total_users),
            Self::Sqlite(store) => store.two_fa_stats(total_users).await,

            Self::MySql(store) => store.two_fa_stats(total_users).await,

            Self::Postgres(store) => store.two_fa_stats(total_users).await,
        }
    }

    async fn get_passkey_by_user(
        &self,
        user_id: u64,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        match self {
            Self::Memory(store) => store.get_passkey_by_user(user_id),
            Self::Sqlite(store) => store.get_passkey_by_user(user_id).await,

            Self::MySql(store) => store.get_passkey_by_user(user_id).await,

            Self::Postgres(store) => store.get_passkey_by_user(user_id).await,
        }
    }

    async fn get_passkey_by_credential_id(
        &self,
        credential_id: &str,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        match self {
            Self::Memory(store) => store.get_passkey_by_credential_id(credential_id),
            Self::Sqlite(store) => store.get_passkey_by_credential_id(credential_id).await,

            Self::MySql(store) => store.get_passkey_by_credential_id(credential_id).await,

            Self::Postgres(store) => store.get_passkey_by_credential_id(credential_id).await,
        }
    }

    async fn upsert_passkey(&self, record: PasskeyRecord) -> Result<PasskeyRecord, SecurityError> {
        match self {
            Self::Memory(store) => store.upsert_passkey(record),
            Self::Sqlite(store) => store.upsert_passkey(record).await,

            Self::MySql(store) => store.upsert_passkey(record).await,

            Self::Postgres(store) => store.upsert_passkey(record).await,
        }
    }

    async fn delete_passkey(&self, user_id: u64) -> Result<bool, SecurityError> {
        match self {
            Self::Memory(store) => store.delete_passkey(user_id),
            Self::Sqlite(store) => store.delete_passkey(user_id).await,

            Self::MySql(store) => store.delete_passkey(user_id).await,

            Self::Postgres(store) => store.delete_passkey(user_id).await,
        }
    }

    async fn save_passkey_session(
        &self,
        session: PasskeySessionRecord,
    ) -> Result<(), SecurityError> {
        match self {
            Self::Memory(store) => store.save_passkey_session(session),
            Self::Sqlite(store) => store.save_passkey_session(session).await,

            Self::MySql(store) => store.save_passkey_session(session).await,

            Self::Postgres(store) => store.save_passkey_session(session).await,
        }
    }

    async fn pop_passkey_session(
        &self,
        session_id: &str,
        kind: &'static str,
    ) -> Result<Option<PasskeySessionRecord>, SecurityError> {
        match self {
            Self::Memory(store) => store.pop_passkey_session(session_id, kind),
            Self::Sqlite(store) => store.pop_passkey_session(session_id, kind).await,

            Self::MySql(store) => store.pop_passkey_session(session_id, kind).await,

            Self::Postgres(store) => store.pop_passkey_session(session_id, kind).await,
        }
    }

    async fn consume_passkey_ready(&self, session_id: &str) -> Result<bool, SecurityError> {
        match self {
            Self::Memory(store) => store.consume_passkey_ready(session_id),
            Self::Sqlite(store) => store.consume_passkey_ready(session_id).await,

            Self::MySql(store) => store.consume_passkey_ready(session_id).await,

            Self::Postgres(store) => store.consume_passkey_ready(session_id).await,
        }
    }
}

#[derive(Debug)]
pub(crate) enum SecurityError {
    Business(String),
    Management(ManagementError),
}

impl SecurityError {
    pub(crate) fn business(message: impl Into<String>) -> Self {
        Self::Business(message.into())
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Business(message) => message.clone(),
            Self::Management(err) => err.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GetTwoFaStatusRequest {
    pub(crate) user_id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct StartTwoFaSetupRequest {
    pub(crate) user_id: u64,
    pub(crate) username: String,
    pub(crate) issuer: String,
}

#[derive(Debug, Clone)]
pub(crate) struct EnableTwoFaRequest {
    pub(crate) user_id: u64,
    pub(crate) code: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DisableTwoFaRequest {
    pub(crate) user_id: u64,
    pub(crate) code: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RegenerateTwoFaBackupCodesRequest {
    pub(crate) user_id: u64,
    pub(crate) code: String,
}

#[derive(Debug, Clone)]
pub(crate) struct UniversalVerifyRequest {
    pub(crate) user_id: u64,
    pub(crate) method: VerificationMethod,
    pub(crate) code: Option<String>,
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GetPasskeyStatusRequest {
    pub(crate) user_id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct PasskeyFlowRequest {
    pub(crate) user: Option<PasskeyUser>,
    pub(crate) flow: PasskeyFlow,
    pub(crate) session_id: String,
    pub(crate) request: PasskeyRequestContext,
    pub(crate) payload: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeletePasskeyRequest {
    pub(crate) user_id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AdminResetPasskeyRequest {
    pub(crate) actor_role: i32,
    pub(crate) target_role: i32,
    pub(crate) target_user_id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AdminDisableTwoFaRequest {
    pub(crate) actor_role: i32,
    pub(crate) target_role: i32,
    pub(crate) target_user_id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AdminTwoFaStatsRequest {
    pub(crate) total_users: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PasskeyUser {
    pub(crate) id: u64,
    pub(crate) username: String,
    pub(crate) display_name: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PasskeyRequestContext {
    pub(crate) host: Option<String>,
    pub(crate) forwarded_proto: Option<String>,
    pub(crate) uri_scheme: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TwoFaStatus {
    pub(crate) enabled: bool,
    pub(crate) locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backup_codes_remaining: Option<usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TwoFaSetup {
    pub(crate) secret: String,
    pub(crate) qr_code_data: String,
    pub(crate) backup_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct VerificationStatus {
    pub(crate) verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PasskeyStatus {
    pub(crate) enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_used_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PasskeyBegin {
    pub(crate) options: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) enum PasskeyFlowResponse {
    Begin(PasskeyBegin),
    Finished { user_id: Option<u64> },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct TwoFaStats {
    pub(crate) total_users: usize,
    pub(crate) enabled_users: usize,
    pub(crate) enabled_rate: String,
    pub(crate) locked_users: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum VerificationMethod {
    #[serde(rename = "2fa")]
    TwoFa,
    Passkey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasskeyFlow {
    RegisterBegin,
    RegisterFinish,
    LoginBegin,
    LoginFinish,
    VerifyBegin,
    VerifyFinish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TwoFaRecord {
    id: u64,
    user_id: u64,
    secret: String,
    is_enabled: bool,
    failed_attempts: i32,
    locked_until: Option<i64>,
    last_used_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackupCodeRecord {
    id: u64,
    user_id: u64,
    code_hash: String,
    is_used: bool,
    used_at: Option<i64>,
    created_at: i64,
}

#[derive(Debug, Clone)]
struct PasskeyRecord {
    id: u64,
    user_id: u64,
    user_uuid: Uuid,
    credential_id: String,
    passkey: Passkey,
    last_used_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PasskeySessionRecord {
    session_id: String,
    kind: &'static str,
    payload: String,
    user_id: Option<u64>,
    expires_at: i64,
    created_at: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemorySecurityStore {
    inner: Arc<RwLock<MemorySecurityData>>,
}

#[derive(Debug, Clone, Default)]
struct MemorySecurityData {
    next_two_fa_id: u64,
    next_backup_code_id: u64,
    next_passkey_id: u64,
    two_fa: BTreeMap<u64, TwoFaRecord>,
    backup_codes: Vec<BackupCodeRecord>,
    passkeys: BTreeMap<u64, PasskeyRecord>,
    passkeys_by_credential_id: BTreeMap<String, u64>,
    passkey_sessions: BTreeMap<(String, &'static str), PasskeySessionRecord>,
}

impl MemorySecurityStore {
    fn get_two_fa(&self, user_id: u64) -> Result<Option<TwoFaRecord>, SecurityError> {
        self.inner
            .read()
            .map(|data| data.two_fa.get(&user_id).cloned())
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))
    }

    fn upsert_two_fa(&self, mut record: TwoFaRecord) -> Result<TwoFaRecord, SecurityError> {
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        if let Some(current) = data.two_fa.get(&record.user_id) {
            record.id = current.id;
            if record.created_at == 0 {
                record.created_at = current.created_at;
            }
        }
        if record.id == 0 {
            data.next_two_fa_id = data.next_two_fa_id.saturating_add(1);
            record.id = data.next_two_fa_id;
        }
        data.two_fa.insert(record.user_id, record.clone());
        Ok(record)
    }

    fn delete_two_fa(&self, user_id: u64) -> Result<bool, SecurityError> {
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        let existed = data.two_fa.remove(&user_id).is_some();
        data.backup_codes.retain(|code| code.user_id != user_id);
        Ok(existed)
    }

    fn replace_backup_codes(
        &self,
        user_id: u64,
        codes: &[String],
        now: i64,
    ) -> Result<(), SecurityError> {
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        data.backup_codes.retain(|code| code.user_id != user_id);
        for code in codes {
            data.next_backup_code_id = data.next_backup_code_id.saturating_add(1);
            let id = data.next_backup_code_id;
            data.backup_codes.push(BackupCodeRecord {
                id,
                user_id,
                code_hash: hash_backup_code(code)?,
                is_used: false,
                used_at: None,
                created_at: now,
            });
        }
        Ok(())
    }

    fn unused_backup_code_count(&self, user_id: u64) -> Result<usize, SecurityError> {
        self.inner
            .read()
            .map(|data| {
                data.backup_codes
                    .iter()
                    .filter(|code| code.user_id == user_id && !code.is_used)
                    .count()
            })
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))
    }

    fn consume_backup_code(&self, user_id: u64, code: &str) -> Result<bool, SecurityError> {
        if !validate_backup_code_format(code) {
            return Err(SecurityError::business("验证码或备用码不正确"));
        }
        let normalized = normalize_backup_code(code);
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        for stored in data
            .backup_codes
            .iter_mut()
            .filter(|stored| stored.user_id == user_id && !stored.is_used)
        {
            if verify(&normalized, &stored.code_hash).unwrap_or(false) {
                stored.is_used = true;
                stored.used_at = Some(now_unix());
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn two_fa_stats(&self, total_users: usize) -> Result<TwoFaStats, SecurityError> {
        self.inner
            .read()
            .map(|data| two_fa_stats_from_records(total_users, data.two_fa.values().cloned()))
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))
    }

    fn get_passkey_by_user(&self, user_id: u64) -> Result<Option<PasskeyRecord>, SecurityError> {
        self.inner
            .read()
            .map(|data| data.passkeys.get(&user_id).cloned())
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))
    }

    fn get_passkey_by_credential_id(
        &self,
        credential_id: &str,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        self.inner
            .read()
            .map(|data| {
                data.passkeys_by_credential_id
                    .get(credential_id)
                    .and_then(|user_id| data.passkeys.get(user_id))
                    .cloned()
            })
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))
    }

    fn upsert_passkey(&self, mut record: PasskeyRecord) -> Result<PasskeyRecord, SecurityError> {
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        if let Some(current) = data.passkeys.remove(&record.user_id) {
            data.passkeys_by_credential_id
                .remove(&current.credential_id);
            record.id = current.id;
            if record.created_at == 0 {
                record.created_at = current.created_at;
            }
        }
        if record.id == 0 {
            data.next_passkey_id = data.next_passkey_id.saturating_add(1);
            record.id = data.next_passkey_id;
        }
        data.passkeys_by_credential_id
            .insert(record.credential_id.clone(), record.user_id);
        data.passkeys.insert(record.user_id, record.clone());
        Ok(record)
    }

    fn delete_passkey(&self, user_id: u64) -> Result<bool, SecurityError> {
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        let Some(record) = data.passkeys.remove(&user_id) else {
            return Ok(false);
        };
        data.passkeys_by_credential_id.remove(&record.credential_id);
        Ok(true)
    }

    fn save_passkey_session(&self, session: PasskeySessionRecord) -> Result<(), SecurityError> {
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        data.passkey_sessions
            .insert((session.session_id.clone(), session.kind), session);
        Ok(())
    }

    fn pop_passkey_session(
        &self,
        session_id: &str,
        kind: &'static str,
    ) -> Result<Option<PasskeySessionRecord>, SecurityError> {
        let mut data = self
            .inner
            .write()
            .map_err(|_| SecurityError::Management(ManagementError::Poisoned("security")))?;
        let Some(session) = data
            .passkey_sessions
            .remove(&(session_id.to_string(), kind))
        else {
            return Ok(None);
        };
        if session.expires_at < now_unix() {
            return Ok(None);
        }
        Ok(Some(session))
    }

    fn consume_passkey_ready(&self, session_id: &str) -> Result<bool, SecurityError> {
        self.pop_passkey_session(session_id, PASSKEY_READY_SESSION)
            .map(|session| session.is_some())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteSecurityStore {
    pool: SqlitePool,
}

impl SqliteSecurityStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = url
            .parse::<SqliteConnectOptions>()
            .map_err(|err| ManagementError::Storage(err.to_string()))?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), ManagementError> {
        for sql in [
            r#"CREATE TABLE IF NOT EXISTS two_fas (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL UNIQUE,
                secret TEXT NOT NULL,
                is_enabled INTEGER NOT NULL DEFAULT 0,
                failed_attempts INTEGER NOT NULL DEFAULT 0,
                locked_until INTEGER,
                last_used_at INTEGER,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_two_fas_user_id ON two_fas(user_id)",
            r#"CREATE TABLE IF NOT EXISTS two_fa_backup_codes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL,
                code_hash TEXT NOT NULL,
                is_used INTEGER NOT NULL DEFAULT 0,
                used_at INTEGER,
                created_at INTEGER NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_two_fa_backup_codes_user_id ON two_fa_backup_codes(user_id)",
            r#"CREATE TABLE IF NOT EXISTS passkey_credentials (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL UNIQUE,
                user_uuid TEXT NOT NULL,
                credential_id TEXT NOT NULL UNIQUE,
                passkey_json TEXT NOT NULL,
                last_used_at INTEGER,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_user_id ON passkey_credentials(user_id)",
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_credential_id ON passkey_credentials(credential_id)",
            r#"CREATE TABLE IF NOT EXISTS passkey_sessions (
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                user_id INTEGER,
                expires_at INTEGER NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(session_id, kind)
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_passkey_sessions_expires_at ON passkey_sessions(expires_at)",
        ] {
            sqlx::query(sql)
                .execute(&self.pool)
                .await
                .map_err(|err| ManagementError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn get_two_fa(&self, user_id: u64) -> Result<Option<TwoFaRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, secret, is_enabled, failed_attempts, locked_until,
                last_used_at, created_at, updated_at
            FROM two_fas WHERE user_id = ?",
        )
        .bind(user_id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| {
            Ok(TwoFaRecord {
                id: u64_col(&row, "id")?,
                user_id: u64_col(&row, "user_id")?,
                secret: string_col(&row, "secret")?,
                is_enabled: bool_col(&row, "is_enabled")?,
                failed_attempts: i32_col(&row, "failed_attempts")?,
                locked_until: optional_i64_col(&row, "locked_until"),
                last_used_at: optional_i64_col(&row, "last_used_at"),
                created_at: i64_col(&row, "created_at")?,
                updated_at: i64_col(&row, "updated_at")?,
            })
        })
        .transpose()
    }

    async fn upsert_two_fa(&self, record: TwoFaRecord) -> Result<TwoFaRecord, SecurityError> {
        sqlx::query(
            r#"INSERT INTO two_fas
                (user_id, secret, is_enabled, failed_attempts, locked_until, last_used_at, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(user_id) DO UPDATE SET
                secret = excluded.secret,
                is_enabled = excluded.is_enabled,
                failed_attempts = excluded.failed_attempts,
                locked_until = excluded.locked_until,
                last_used_at = excluded.last_used_at,
                updated_at = excluded.updated_at"#,
        )
        .bind(record.user_id as i64)
        .bind(&record.secret)
        .bind(i64::from(record.is_enabled))
        .bind(i64::from(record.failed_attempts))
        .bind(record.locked_until)
        .bind(record.last_used_at)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        self.get_two_fa(record.user_id).await?.ok_or_else(|| {
            SecurityError::Management(ManagementError::Storage("2FA upsert failed".into()))
        })
    }

    async fn delete_two_fa(&self, user_id: u64) -> Result<bool, SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM two_fa_backup_codes WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        let result = sqlx::query("DELETE FROM two_fas WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        tx.commit().await.map_err(sqlx_security_error)?;
        Ok(result.rows_affected() > 0)
    }

    async fn replace_backup_codes(
        &self,
        user_id: u64,
        codes: &[String],
        now: i64,
    ) -> Result<(), SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM two_fa_backup_codes WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        for code in codes {
            sqlx::query(
                "INSERT INTO two_fa_backup_codes (user_id, code_hash, is_used, used_at, created_at)
                VALUES (?, ?, 0, NULL, ?)",
            )
            .bind(user_id as i64)
            .bind(hash_backup_code(code)?)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        }
        tx.commit().await.map_err(sqlx_security_error)?;
        Ok(())
    }

    async fn unused_backup_code_count(&self, user_id: u64) -> Result<usize, SecurityError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM two_fa_backup_codes WHERE user_id = ? AND is_used = 0",
        )
        .bind(user_id as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        Ok(count.max(0) as usize)
    }

    async fn consume_backup_code(&self, user_id: u64, code: &str) -> Result<bool, SecurityError> {
        if !validate_backup_code_format(code) {
            return Err(SecurityError::business("验证码或备用码不正确"));
        }
        let normalized = normalize_backup_code(code);
        let rows = sqlx::query(
            "SELECT id, code_hash FROM two_fa_backup_codes WHERE user_id = ? AND is_used = 0",
        )
        .bind(user_id as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        for row in rows {
            let id = u64_col(&row, "id")?;
            let code_hash = string_col(&row, "code_hash")?;
            if verify(&normalized, &code_hash).unwrap_or(false) {
                sqlx::query("UPDATE two_fa_backup_codes SET is_used = 1, used_at = ? WHERE id = ?")
                    .bind(now_unix())
                    .bind(id as i64)
                    .execute(&self.pool)
                    .await
                    .map_err(sqlx_security_error)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn two_fa_stats(&self, total_users: usize) -> Result<TwoFaStats, SecurityError> {
        let rows = sqlx::query("SELECT is_enabled, locked_until FROM two_fas")
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_security_error)?;
        let records = rows.into_iter().map(|row| TwoFaRecord {
            id: 0,
            user_id: 0,
            secret: String::new(),
            is_enabled: bool_col(&row, "is_enabled").unwrap_or(false),
            failed_attempts: 0,
            locked_until: optional_i64_col(&row, "locked_until"),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        });
        Ok(two_fa_stats_from_records(total_users, records))
    }

    async fn get_passkey_by_user(
        &self,
        user_id: u64,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at
            FROM passkey_credentials WHERE user_id = ?",
        )
        .bind(user_id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| passkey_record_from_row(&row)).transpose()
    }

    async fn get_passkey_by_credential_id(
        &self,
        credential_id: &str,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at
            FROM passkey_credentials WHERE credential_id = ?",
        )
        .bind(credential_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| passkey_record_from_row(&row)).transpose()
    }

    async fn upsert_passkey(&self, record: PasskeyRecord) -> Result<PasskeyRecord, SecurityError> {
        sqlx::query(
            r#"INSERT INTO passkey_credentials
                (user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(user_id) DO UPDATE SET
                user_uuid = excluded.user_uuid,
                credential_id = excluded.credential_id,
                passkey_json = excluded.passkey_json,
                last_used_at = excluded.last_used_at,
                updated_at = excluded.updated_at"#,
        )
        .bind(record.user_id as i64)
        .bind(record.user_uuid.to_string())
        .bind(&record.credential_id)
        .bind(serde_json::to_string(&record.passkey).map_err(serde_security_error)?)
        .bind(record.last_used_at)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        self.get_passkey_by_user(record.user_id)
            .await?
            .ok_or_else(|| {
                SecurityError::Management(ManagementError::Storage("passkey upsert failed".into()))
            })
    }

    async fn delete_passkey(&self, user_id: u64) -> Result<bool, SecurityError> {
        let result = sqlx::query("DELETE FROM passkey_credentials WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&self.pool)
            .await
            .map_err(sqlx_security_error)?;
        Ok(result.rows_affected() > 0)
    }

    async fn save_passkey_session(
        &self,
        session: PasskeySessionRecord,
    ) -> Result<(), SecurityError> {
        sqlx::query(
            r#"INSERT INTO passkey_sessions
                (session_id, kind, payload, user_id, expires_at, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(session_id, kind) DO UPDATE SET
                payload = excluded.payload,
                user_id = excluded.user_id,
                expires_at = excluded.expires_at,
                created_at = excluded.created_at"#,
        )
        .bind(&session.session_id)
        .bind(session.kind)
        .bind(&session.payload)
        .bind(session.user_id.map(|user_id| user_id as i64))
        .bind(session.expires_at)
        .bind(session.created_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        Ok(())
    }

    async fn pop_passkey_session(
        &self,
        session_id: &str,
        kind: &'static str,
    ) -> Result<Option<PasskeySessionRecord>, SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        let row = sqlx::query(
            "SELECT session_id, kind, payload, user_id, expires_at, created_at
            FROM passkey_sessions WHERE session_id = ? AND kind = ?",
        )
        .bind(session_id)
        .bind(kind)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM passkey_sessions WHERE session_id = ? AND kind = ?")
            .bind(session_id)
            .bind(kind)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        tx.commit().await.map_err(sqlx_security_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session = passkey_session_from_row(&row)?;
        if session.expires_at < now_unix() {
            return Ok(None);
        }
        Ok(Some(session))
    }

    async fn consume_passkey_ready(&self, session_id: &str) -> Result<bool, SecurityError> {
        self.pop_passkey_session(session_id, PASSKEY_READY_SESSION)
            .await
            .map(|session| session.is_some())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlSecurityStore {
    pool: MySqlPool,
}

impl MySqlSecurityStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url)
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), ManagementError> {
        for sql in [
            r#"CREATE TABLE IF NOT EXISTS two_fas (
                id BIGINT PRIMARY KEY AUTO_INCREMENT,
                user_id BIGINT NOT NULL UNIQUE,
                secret TEXT NOT NULL,
                is_enabled INTEGER NOT NULL DEFAULT 0,
                failed_attempts INTEGER NOT NULL DEFAULT 0,
                locked_until BIGINT,
                last_used_at BIGINT,
                created_at BIGINT NOT NULL DEFAULT 0,
                updated_at BIGINT NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_two_fas_user_id ON two_fas(user_id)",
            r#"CREATE TABLE IF NOT EXISTS two_fa_backup_codes (
                id BIGINT PRIMARY KEY AUTO_INCREMENT,
                user_id BIGINT NOT NULL,
                code_hash TEXT NOT NULL,
                is_used INTEGER NOT NULL DEFAULT 0,
                used_at BIGINT,
                created_at BIGINT NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_two_fa_backup_codes_user_id ON two_fa_backup_codes(user_id)",
            r#"CREATE TABLE IF NOT EXISTS passkey_credentials (
                id BIGINT PRIMARY KEY AUTO_INCREMENT,
                user_id BIGINT NOT NULL UNIQUE,
                user_uuid TEXT NOT NULL,
                credential_id TEXT NOT NULL UNIQUE,
                passkey_json TEXT NOT NULL,
                last_used_at BIGINT,
                created_at BIGINT NOT NULL DEFAULT 0,
                updated_at BIGINT NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_user_id ON passkey_credentials(user_id)",
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_credential_id ON passkey_credentials(credential_id)",
            r#"CREATE TABLE IF NOT EXISTS passkey_sessions (
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                user_id BIGINT,
                expires_at BIGINT NOT NULL,
                created_at BIGINT NOT NULL DEFAULT 0,
                PRIMARY KEY(session_id, kind)
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_passkey_sessions_expires_at ON passkey_sessions(expires_at)",
        ] {
            sqlx::query(sql)
                .execute(&self.pool)
                .await
                .map_err(|err| ManagementError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn get_two_fa(&self, user_id: u64) -> Result<Option<TwoFaRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, secret, is_enabled, failed_attempts, locked_until,
                last_used_at, created_at, updated_at
            FROM two_fas WHERE user_id = ?",
        )
        .bind(user_id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| {
            Ok(TwoFaRecord {
                id: mysql_u64_col(&row, "id")?,
                user_id: mysql_u64_col(&row, "user_id")?,
                secret: mysql_string_col(&row, "secret")?,
                is_enabled: mysql_bool_col(&row, "is_enabled")?,
                failed_attempts: mysql_i32_col(&row, "failed_attempts")?,
                locked_until: optional_mysql_i64_col(&row, "locked_until"),
                last_used_at: optional_mysql_i64_col(&row, "last_used_at"),
                created_at: mysql_i64_col(&row, "created_at")?,
                updated_at: mysql_i64_col(&row, "updated_at")?,
            })
        })
        .transpose()
    }

    async fn upsert_two_fa(&self, record: TwoFaRecord) -> Result<TwoFaRecord, SecurityError> {
        sqlx::query(
            r#"INSERT INTO two_fas
                (user_id, secret, is_enabled, failed_attempts, locked_until, last_used_at, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON DUPLICATE KEY UPDATE
                secret = VALUES(secret),
                is_enabled = VALUES(is_enabled),
                failed_attempts = VALUES(failed_attempts),
                locked_until = VALUES(locked_until),
                last_used_at = VALUES(last_used_at),
                updated_at = VALUES(updated_at)"#,
        )
        .bind(record.user_id as i64)
        .bind(&record.secret)
        .bind(i64::from(record.is_enabled))
        .bind(i64::from(record.failed_attempts))
        .bind(record.locked_until)
        .bind(record.last_used_at)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        self.get_two_fa(record.user_id).await?.ok_or_else(|| {
            SecurityError::Management(ManagementError::Storage("2FA upsert failed".into()))
        })
    }

    async fn delete_two_fa(&self, user_id: u64) -> Result<bool, SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM two_fa_backup_codes WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        let result = sqlx::query("DELETE FROM two_fas WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        tx.commit().await.map_err(sqlx_security_error)?;
        Ok(result.rows_affected() > 0)
    }

    async fn replace_backup_codes(
        &self,
        user_id: u64,
        codes: &[String],
        now: i64,
    ) -> Result<(), SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM two_fa_backup_codes WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        for code in codes {
            sqlx::query(
                "INSERT INTO two_fa_backup_codes (user_id, code_hash, is_used, used_at, created_at)
                VALUES (?, ?, 0, NULL, ?)",
            )
            .bind(user_id as i64)
            .bind(hash_backup_code(code)?)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        }
        tx.commit().await.map_err(sqlx_security_error)?;
        Ok(())
    }

    async fn unused_backup_code_count(&self, user_id: u64) -> Result<usize, SecurityError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM two_fa_backup_codes WHERE user_id = ? AND is_used = 0",
        )
        .bind(user_id as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        Ok(count.max(0) as usize)
    }

    async fn consume_backup_code(&self, user_id: u64, code: &str) -> Result<bool, SecurityError> {
        if !validate_backup_code_format(code) {
            return Err(SecurityError::business("验证码或备用码不正确"));
        }
        let normalized = normalize_backup_code(code);
        let rows = sqlx::query(
            "SELECT id, code_hash FROM two_fa_backup_codes WHERE user_id = ? AND is_used = 0",
        )
        .bind(user_id as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        for row in rows {
            let id = mysql_u64_col(&row, "id")?;
            let code_hash = mysql_string_col(&row, "code_hash")?;
            if verify(&normalized, &code_hash).unwrap_or(false) {
                sqlx::query("UPDATE two_fa_backup_codes SET is_used = 1, used_at = ? WHERE id = ?")
                    .bind(now_unix())
                    .bind(id as i64)
                    .execute(&self.pool)
                    .await
                    .map_err(sqlx_security_error)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn two_fa_stats(&self, total_users: usize) -> Result<TwoFaStats, SecurityError> {
        let rows = sqlx::query("SELECT is_enabled, locked_until FROM two_fas")
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_security_error)?;
        let records = rows.into_iter().map(|row| TwoFaRecord {
            id: 0,
            user_id: 0,
            secret: String::new(),
            is_enabled: mysql_bool_col(&row, "is_enabled").unwrap_or(false),
            failed_attempts: 0,
            locked_until: optional_mysql_i64_col(&row, "locked_until"),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        });
        Ok(two_fa_stats_from_records(total_users, records))
    }

    async fn get_passkey_by_user(
        &self,
        user_id: u64,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at
            FROM passkey_credentials WHERE user_id = ?",
        )
        .bind(user_id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| passkey_record_from_mysql_row(&row)).transpose()
    }

    async fn get_passkey_by_credential_id(
        &self,
        credential_id: &str,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at
            FROM passkey_credentials WHERE credential_id = ?",
        )
        .bind(credential_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| passkey_record_from_mysql_row(&row)).transpose()
    }

    async fn upsert_passkey(&self, record: PasskeyRecord) -> Result<PasskeyRecord, SecurityError> {
        sqlx::query(
            r#"INSERT INTO passkey_credentials
                (user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON DUPLICATE KEY UPDATE
                user_uuid = VALUES(user_uuid),
                credential_id = VALUES(credential_id),
                passkey_json = VALUES(passkey_json),
                last_used_at = VALUES(last_used_at),
                updated_at = VALUES(updated_at)"#,
        )
        .bind(record.user_id as i64)
        .bind(record.user_uuid.to_string())
        .bind(&record.credential_id)
        .bind(serde_json::to_string(&record.passkey).map_err(serde_security_error)?)
        .bind(record.last_used_at)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        self.get_passkey_by_user(record.user_id)
            .await?
            .ok_or_else(|| {
                SecurityError::Management(ManagementError::Storage("passkey upsert failed".into()))
            })
    }

    async fn delete_passkey(&self, user_id: u64) -> Result<bool, SecurityError> {
        let result = sqlx::query("DELETE FROM passkey_credentials WHERE user_id = ?")
            .bind(user_id as i64)
            .execute(&self.pool)
            .await
            .map_err(sqlx_security_error)?;
        Ok(result.rows_affected() > 0)
    }

    async fn save_passkey_session(
        &self,
        session: PasskeySessionRecord,
    ) -> Result<(), SecurityError> {
        sqlx::query(
            r#"INSERT INTO passkey_sessions
                (session_id, kind, payload, user_id, expires_at, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON DUPLICATE KEY UPDATE
                payload = VALUES(payload),
                user_id = VALUES(user_id),
                expires_at = VALUES(expires_at),
                created_at = VALUES(created_at)"#,
        )
        .bind(&session.session_id)
        .bind(session.kind)
        .bind(&session.payload)
        .bind(session.user_id.map(|user_id| user_id as i64))
        .bind(session.expires_at)
        .bind(session.created_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        Ok(())
    }

    async fn pop_passkey_session(
        &self,
        session_id: &str,
        kind: &'static str,
    ) -> Result<Option<PasskeySessionRecord>, SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        let row = sqlx::query(
            "SELECT session_id, kind, payload, user_id, expires_at, created_at
            FROM passkey_sessions WHERE session_id = ? AND kind = ?",
        )
        .bind(session_id)
        .bind(kind)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM passkey_sessions WHERE session_id = ? AND kind = ?")
            .bind(session_id)
            .bind(kind)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        tx.commit().await.map_err(sqlx_security_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session = passkey_session_from_mysql_row(&row)?;
        if session.expires_at < now_unix() {
            return Ok(None);
        }
        Ok(Some(session))
    }

    async fn consume_passkey_ready(&self, session_id: &str) -> Result<bool, SecurityError> {
        self.pop_passkey_session(session_id, PASSKEY_READY_SESSION)
            .await
            .map(|session| session.is_some())
    }
}


#[derive(Debug, Clone)]
pub(crate) struct PostgresSecurityStore {
    pool: PgPool,
}

impl PostgresSecurityStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), ManagementError> {
        for sql in [
            r#"CREATE TABLE IF NOT EXISTS two_fas (
                id BIGSERIAL PRIMARY KEY,
                user_id BIGINT NOT NULL UNIQUE,
                secret TEXT NOT NULL,
                is_enabled INTEGER NOT NULL DEFAULT 0,
                failed_attempts INTEGER NOT NULL DEFAULT 0,
                locked_until BIGINT,
                last_used_at BIGINT,
                created_at BIGINT NOT NULL DEFAULT 0,
                updated_at BIGINT NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_two_fas_user_id ON two_fas(user_id)",
            r#"CREATE TABLE IF NOT EXISTS two_fa_backup_codes (
                id BIGSERIAL PRIMARY KEY,
                user_id BIGINT NOT NULL,
                code_hash TEXT NOT NULL,
                is_used INTEGER NOT NULL DEFAULT 0,
                used_at BIGINT,
                created_at BIGINT NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_two_fa_backup_codes_user_id ON two_fa_backup_codes(user_id)",
            r#"CREATE TABLE IF NOT EXISTS passkey_credentials (
                id BIGSERIAL PRIMARY KEY,
                user_id BIGINT NOT NULL UNIQUE,
                user_uuid TEXT NOT NULL,
                credential_id TEXT NOT NULL UNIQUE,
                passkey_json TEXT NOT NULL,
                last_used_at BIGINT,
                created_at BIGINT NOT NULL DEFAULT 0,
                updated_at BIGINT NOT NULL DEFAULT 0
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_user_id ON passkey_credentials(user_id)",
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_credential_id ON passkey_credentials(credential_id)",
            r#"CREATE TABLE IF NOT EXISTS passkey_sessions (
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                user_id BIGINT,
                expires_at BIGINT NOT NULL,
                created_at BIGINT NOT NULL DEFAULT 0,
                PRIMARY KEY(session_id, kind)
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_passkey_sessions_expires_at ON passkey_sessions(expires_at)",
        ] {
            sqlx::query(sql)
                .execute(&self.pool)
                .await
                .map_err(|err| ManagementError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn get_two_fa(&self, user_id: u64) -> Result<Option<TwoFaRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, secret, is_enabled, failed_attempts, locked_until,
                last_used_at, created_at, updated_at
            FROM two_fas WHERE user_id = $1",
        )
        .bind(user_id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| {
            Ok(TwoFaRecord {
                id: pg_u64_col(&row, "id")?,
                user_id: pg_u64_col(&row, "user_id")?,
                secret: pg_string_col(&row, "secret")?,
                is_enabled: pg_bool_col(&row, "is_enabled")?,
                failed_attempts: pg_i32_col(&row, "failed_attempts")?,
                locked_until: pg_optional_i64_col(&row, "locked_until"),
                last_used_at: pg_optional_i64_col(&row, "last_used_at"),
                created_at: pg_i64_col(&row, "created_at")?,
                updated_at: pg_i64_col(&row, "updated_at")?,
            })
        })
        .transpose()
    }

    async fn upsert_two_fa(&self, record: TwoFaRecord) -> Result<TwoFaRecord, SecurityError> {
        sqlx::query(
            r#"INSERT INTO two_fas
                (user_id, secret, is_enabled, failed_attempts, locked_until, last_used_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT(user_id) DO UPDATE SET
                secret = excluded.secret,
                is_enabled = excluded.is_enabled,
                failed_attempts = excluded.failed_attempts,
                locked_until = excluded.locked_until,
                last_used_at = excluded.last_used_at,
                updated_at = excluded.updated_at"#,
        )
        .bind(record.user_id as i64)
        .bind(&record.secret)
        .bind(i32::from(record.is_enabled))
        .bind(record.failed_attempts)
        .bind(record.locked_until)
        .bind(record.last_used_at)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        self.get_two_fa(record.user_id).await?.ok_or_else(|| {
            SecurityError::Management(ManagementError::Storage("2FA upsert failed".into()))
        })
    }

    async fn delete_two_fa(&self, user_id: u64) -> Result<bool, SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM two_fa_backup_codes WHERE user_id = $1")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        let result = sqlx::query("DELETE FROM two_fas WHERE user_id = $1")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        tx.commit().await.map_err(sqlx_security_error)?;
        Ok(result.rows_affected() > 0)
    }

    async fn replace_backup_codes(
        &self,
        user_id: u64,
        codes: &[String],
        now: i64,
    ) -> Result<(), SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM two_fa_backup_codes WHERE user_id = $1")
            .bind(user_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        for code in codes {
            sqlx::query(
                "INSERT INTO two_fa_backup_codes (user_id, code_hash, is_used, used_at, created_at)
                VALUES ($1, $2, 0, NULL, $3)",
            )
            .bind(user_id as i64)
            .bind(hash_backup_code(code)?)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        }
        tx.commit().await.map_err(sqlx_security_error)?;
        Ok(())
    }

    async fn unused_backup_code_count(&self, user_id: u64) -> Result<usize, SecurityError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM two_fa_backup_codes WHERE user_id = $1 AND is_used = 0",
        )
        .bind(user_id as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        Ok(count.max(0) as usize)
    }

    async fn consume_backup_code(&self, user_id: u64, code: &str) -> Result<bool, SecurityError> {
        if !validate_backup_code_format(code) {
            return Err(SecurityError::business("验证码或备用码不正确"));
        }
        let normalized = normalize_backup_code(code);
        let rows = sqlx::query(
            "SELECT id, code_hash FROM two_fa_backup_codes WHERE user_id = $1 AND is_used = 0",
        )
        .bind(user_id as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        for row in rows {
            let id = pg_u64_col(&row, "id")?;
            let code_hash = pg_string_col(&row, "code_hash")?;
            if verify(&normalized, &code_hash).unwrap_or(false) {
                sqlx::query(
                    "UPDATE two_fa_backup_codes SET is_used = 1, used_at = $1 WHERE id = $2",
                )
                .bind(now_unix())
                .bind(id as i64)
                .execute(&self.pool)
                .await
                .map_err(sqlx_security_error)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn two_fa_stats(&self, total_users: usize) -> Result<TwoFaStats, SecurityError> {
        let rows = sqlx::query("SELECT is_enabled, locked_until FROM two_fas")
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_security_error)?;
        let records = rows.into_iter().map(|row| TwoFaRecord {
            id: 0,
            user_id: 0,
            secret: String::new(),
            is_enabled: pg_bool_col(&row, "is_enabled").unwrap_or(false),
            failed_attempts: 0,
            locked_until: pg_optional_i64_col(&row, "locked_until"),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        });
        Ok(two_fa_stats_from_records(total_users, records))
    }

    async fn get_passkey_by_user(
        &self,
        user_id: u64,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at
            FROM passkey_credentials WHERE user_id = $1",
        )
        .bind(user_id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| passkey_record_from_pg_row(&row)).transpose()
    }

    async fn get_passkey_by_credential_id(
        &self,
        credential_id: &str,
    ) -> Result<Option<PasskeyRecord>, SecurityError> {
        let row = sqlx::query(
            "SELECT id, user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at
            FROM passkey_credentials WHERE credential_id = $1",
        )
        .bind(credential_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        row.map(|row| passkey_record_from_pg_row(&row)).transpose()
    }

    async fn upsert_passkey(&self, record: PasskeyRecord) -> Result<PasskeyRecord, SecurityError> {
        sqlx::query(
            r#"INSERT INTO passkey_credentials
                (user_id, user_uuid, credential_id, passkey_json, last_used_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT(user_id) DO UPDATE SET
                user_uuid = excluded.user_uuid,
                credential_id = excluded.credential_id,
                passkey_json = excluded.passkey_json,
                last_used_at = excluded.last_used_at,
                updated_at = excluded.updated_at"#,
        )
        .bind(record.user_id as i64)
        .bind(record.user_uuid.to_string())
        .bind(&record.credential_id)
        .bind(serde_json::to_string(&record.passkey).map_err(serde_security_error)?)
        .bind(record.last_used_at)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        self.get_passkey_by_user(record.user_id)
            .await?
            .ok_or_else(|| {
                SecurityError::Management(ManagementError::Storage("passkey upsert failed".into()))
            })
    }

    async fn delete_passkey(&self, user_id: u64) -> Result<bool, SecurityError> {
        let result = sqlx::query("DELETE FROM passkey_credentials WHERE user_id = $1")
            .bind(user_id as i64)
            .execute(&self.pool)
            .await
            .map_err(sqlx_security_error)?;
        Ok(result.rows_affected() > 0)
    }

    async fn save_passkey_session(
        &self,
        session: PasskeySessionRecord,
    ) -> Result<(), SecurityError> {
        sqlx::query(
            r#"INSERT INTO passkey_sessions
                (session_id, kind, payload, user_id, expires_at, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT(session_id, kind) DO UPDATE SET
                payload = excluded.payload,
                user_id = excluded.user_id,
                expires_at = excluded.expires_at,
                created_at = excluded.created_at"#,
        )
        .bind(&session.session_id)
        .bind(session.kind)
        .bind(&session.payload)
        .bind(session.user_id.map(|user_id| user_id as i64))
        .bind(session.expires_at)
        .bind(session.created_at)
        .execute(&self.pool)
        .await
        .map_err(sqlx_security_error)?;
        Ok(())
    }

    async fn pop_passkey_session(
        &self,
        session_id: &str,
        kind: &'static str,
    ) -> Result<Option<PasskeySessionRecord>, SecurityError> {
        let mut tx = self.pool.begin().await.map_err(sqlx_security_error)?;
        let row = sqlx::query(
            "SELECT session_id, kind, payload, user_id, expires_at, created_at
            FROM passkey_sessions WHERE session_id = $1 AND kind = $2",
        )
        .bind(session_id)
        .bind(kind)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_security_error)?;
        sqlx::query("DELETE FROM passkey_sessions WHERE session_id = $1 AND kind = $2")
            .bind(session_id)
            .bind(kind)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_security_error)?;
        tx.commit().await.map_err(sqlx_security_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session = passkey_session_from_pg_row(&row)?;
        if session.expires_at < now_unix() {
            return Ok(None);
        }
        Ok(Some(session))
    }

    async fn consume_passkey_ready(&self, session_id: &str) -> Result<bool, SecurityError> {
        self.pop_passkey_session(session_id, PASSKEY_READY_SESSION)
            .await
            .map(|session| session.is_some())
    }
}

impl Service<GetTwoFaStatusRequest> for SecurityService {
    type Response = TwoFaStatus;
    type Error = SecurityError;

    async fn call(&self, req: GetTwoFaStatusRequest) -> Result<Self::Response, Self::Error> {
        let Some(two_fa) = self.store.get_two_fa(req.user_id).await? else {
            return Ok(TwoFaStatus {
                enabled: false,
                locked: false,
                backup_codes_remaining: None,
            });
        };
        let backup_codes_remaining = if two_fa.is_enabled {
            Some(self.store.unused_backup_code_count(req.user_id).await?)
        } else {
            None
        };
        Ok(TwoFaStatus {
            enabled: two_fa.is_enabled,
            locked: two_fa.is_locked(now_unix()),
            backup_codes_remaining,
        })
    }
}

impl Service<StartTwoFaSetupRequest> for SecurityService {
    type Response = TwoFaSetup;
    type Error = SecurityError;

    async fn call(&self, req: StartTwoFaSetupRequest) -> Result<Self::Response, Self::Error> {
        if let Some(existing) = self.store.get_two_fa(req.user_id).await? {
            if existing.is_enabled {
                return Err(SecurityError::business("用户已启用2FA，请先禁用后重新设置"));
            }
            self.store.delete_two_fa(req.user_id).await?;
        }

        let now = now_unix();
        let secret = generate_totp_secret();
        let backup_codes = generate_backup_codes();
        self.store
            .upsert_two_fa(TwoFaRecord {
                id: 0,
                user_id: req.user_id,
                secret: secret.clone(),
                is_enabled: false,
                failed_attempts: 0,
                locked_until: None,
                last_used_at: None,
                created_at: now,
                updated_at: now,
            })
            .await?;
        self.store
            .replace_backup_codes(req.user_id, &backup_codes, now)
            .await?;
        Ok(TwoFaSetup {
            qr_code_data: generate_qr_code_data(&secret, &req.username, &req.issuer),
            secret,
            backup_codes,
        })
    }
}

impl Service<EnableTwoFaRequest> for SecurityService {
    type Response = ();
    type Error = SecurityError;

    async fn call(&self, req: EnableTwoFaRequest) -> Result<Self::Response, Self::Error> {
        let Some(mut two_fa) = self.store.get_two_fa(req.user_id).await? else {
            return Err(SecurityError::business("请先完成2FA初始化设置"));
        };
        if two_fa.is_enabled {
            return Err(SecurityError::business("2FA已经启用"));
        }
        let clean_code = validate_numeric_code(&req.code)?;
        if !validate_totp_code(&two_fa.secret, &clean_code, now_unix()) {
            return Err(SecurityError::business("验证码或备用码错误，请重试"));
        }
        let now = now_unix();
        two_fa.is_enabled = true;
        two_fa.failed_attempts = 0;
        two_fa.locked_until = None;
        two_fa.updated_at = now;
        self.store.upsert_two_fa(two_fa).await?;
        Ok(())
    }
}

impl Service<DisableTwoFaRequest> for SecurityService {
    type Response = ();
    type Error = SecurityError;

    async fn call(&self, req: DisableTwoFaRequest) -> Result<Self::Response, Self::Error> {
        let Some(mut two_fa) = self.store.get_two_fa(req.user_id).await? else {
            return Err(SecurityError::business("用户未启用2FA"));
        };
        if !two_fa.is_enabled {
            return Err(SecurityError::business("用户未启用2FA"));
        }
        if !self.verify_two_fa_or_backup(&mut two_fa, &req.code).await? {
            return Err(SecurityError::business("验证码或备用码错误，请重试"));
        }
        self.store.delete_two_fa(req.user_id).await?;
        Ok(())
    }
}

impl Service<RegenerateTwoFaBackupCodesRequest> for SecurityService {
    type Response = Vec<String>;
    type Error = SecurityError;

    async fn call(
        &self,
        req: RegenerateTwoFaBackupCodesRequest,
    ) -> Result<Self::Response, Self::Error> {
        let Some(mut two_fa) = self.store.get_two_fa(req.user_id).await? else {
            return Err(SecurityError::business("用户未启用2FA"));
        };
        if !two_fa.is_enabled {
            return Err(SecurityError::business("用户未启用2FA"));
        }
        let clean_code = validate_numeric_code(&req.code)?;
        let valid = self
            .validate_totp_and_update_usage(&mut two_fa, &clean_code)
            .await?;
        if !valid {
            return Err(SecurityError::business("验证码或备用码错误，请重试"));
        }
        let backup_codes = generate_backup_codes();
        self.store
            .replace_backup_codes(req.user_id, &backup_codes, now_unix())
            .await?;
        Ok(backup_codes)
    }
}

impl Service<UniversalVerifyRequest> for SecurityService {
    type Response = VerificationStatus;
    type Error = SecurityError;

    async fn call(&self, req: UniversalVerifyRequest) -> Result<Self::Response, Self::Error> {
        let has_two_fa = self
            .store
            .get_two_fa(req.user_id)
            .await?
            .is_some_and(|two_fa| two_fa.is_enabled);
        let has_passkey = self.store.get_passkey_by_user(req.user_id).await?.is_some();
        if !has_two_fa && !has_passkey {
            return Err(SecurityError::business("用户未启用2FA或Passkey"));
        }

        match req.method {
            VerificationMethod::TwoFa => {
                if !has_two_fa {
                    return Err(SecurityError::business("用户未启用2FA"));
                }
                let Some(code) = req.code.filter(|code| !code.trim().is_empty()) else {
                    return Err(SecurityError::business("验证码不能为空"));
                };
                let Some(mut two_fa) = self.store.get_two_fa(req.user_id).await? else {
                    return Err(SecurityError::business("用户未启用2FA"));
                };
                if !self.verify_two_fa_or_backup(&mut two_fa, &code).await? {
                    return Err(SecurityError::business("验证失败，请检查验证码"));
                }
                Ok(VerificationStatus {
                    verified: true,
                    expires_at: Some(now_unix().saturating_add(300)),
                })
            }
            VerificationMethod::Passkey => {
                if !has_passkey {
                    return Err(SecurityError::business("用户未启用Passkey"));
                }
                let Some(session_id) = req.session_id.filter(|session_id| !session_id.is_empty())
                else {
                    return Err(SecurityError::business("Passkey 验证会话不存在或已过期"));
                };
                if !self.store.consume_passkey_ready(&session_id).await? {
                    return Err(SecurityError::business("Passkey 验证会话不存在或已过期"));
                }
                Ok(VerificationStatus {
                    verified: true,
                    expires_at: Some(now_unix().saturating_add(PASSKEY_READY_TTL_SECONDS)),
                })
            }
        }
    }
}

impl Service<GetPasskeyStatusRequest> for SecurityService {
    type Response = PasskeyStatus;
    type Error = SecurityError;

    async fn call(&self, req: GetPasskeyStatusRequest) -> Result<Self::Response, Self::Error> {
        let Some(passkey) = self.store.get_passkey_by_user(req.user_id).await? else {
            return Ok(PasskeyStatus {
                enabled: false,
                last_used_at: None,
            });
        };
        Ok(PasskeyStatus {
            enabled: true,
            last_used_at: passkey.last_used_at.map(|value| value.to_string()),
        })
    }
}

impl Service<PasskeyFlowRequest> for SecurityService {
    type Response = PasskeyFlowResponse;
    type Error = SecurityError;

    async fn call(&self, req: PasskeyFlowRequest) -> Result<Self::Response, Self::Error> {
        let options = self.options()?;
        if !PasskeySettings::from_options(&options).enabled {
            return Err(SecurityError::business(PASSKEY_DISABLED_MESSAGE));
        }
        match req.flow {
            PasskeyFlow::RegisterBegin => self.passkey_register_begin(req, &options).await,
            PasskeyFlow::RegisterFinish => self.passkey_register_finish(req, &options).await,
            PasskeyFlow::LoginBegin => self.passkey_login_begin(req, &options).await,
            PasskeyFlow::LoginFinish => self.passkey_login_finish(req, &options).await,
            PasskeyFlow::VerifyBegin => self.passkey_verify_begin(req, &options).await,
            PasskeyFlow::VerifyFinish => self.passkey_verify_finish(req, &options).await,
        }
    }
}

impl Service<DeletePasskeyRequest> for SecurityService {
    type Response = ();
    type Error = SecurityError;

    async fn call(&self, req: DeletePasskeyRequest) -> Result<Self::Response, Self::Error> {
        if !self.store.delete_passkey(req.user_id).await? {
            return Err(SecurityError::business(PASSKEY_NOT_BOUND_MESSAGE));
        }
        Ok(())
    }
}

impl Service<AdminResetPasskeyRequest> for SecurityService {
    type Response = ();
    type Error = SecurityError;

    async fn call(&self, req: AdminResetPasskeyRequest) -> Result<Self::Response, Self::Error> {
        if !can_manage_target_role(req.actor_role, req.target_role) {
            return Err(SecurityError::business("no permission"));
        }
        if !self.store.delete_passkey(req.target_user_id).await? {
            return Err(SecurityError::business(PASSKEY_NOT_BOUND_MESSAGE));
        }
        Ok(())
    }
}

impl Service<AdminDisableTwoFaRequest> for SecurityService {
    type Response = ();
    type Error = SecurityError;

    async fn call(&self, req: AdminDisableTwoFaRequest) -> Result<Self::Response, Self::Error> {
        if !can_manage_target_role(req.actor_role, req.target_role) {
            return Err(SecurityError::business("无权操作同级或更高级用户的2FA设置"));
        }
        if !self.store.delete_two_fa(req.target_user_id).await? {
            return Err(SecurityError::business("用户未启用2FA"));
        }
        Ok(())
    }
}

impl Service<AdminTwoFaStatsRequest> for SecurityService {
    type Response = TwoFaStats;
    type Error = SecurityError;

    async fn call(&self, req: AdminTwoFaStatsRequest) -> Result<Self::Response, Self::Error> {
        self.store.two_fa_stats(req.total_users).await
    }
}

impl SecurityService {
    async fn passkey_register_begin(
        &self,
        req: PasskeyFlowRequest,
        options: &BTreeMap<String, String>,
    ) -> Result<PasskeyFlowResponse, SecurityError> {
        let user = req.user.ok_or_else(|| SecurityError::business("未登录"))?;
        let webauthn = build_webauthn(options, &req.request)?;
        let existing = self.store.get_passkey_by_user(user.id).await?;
        let exclude_credentials = existing
            .as_ref()
            .map(|record| vec![record.passkey.cred_id().clone()]);
        let user_name = passkey_user_name(&user);
        let display_name = passkey_user_display_name(&user);
        let (options, state) = webauthn
            .start_passkey_registration(
                passkey_user_uuid(user.id),
                &user_name,
                &display_name,
                exclude_credentials,
            )
            .map_err(passkey_webauthn_error)?;
        let session = encode_passkey_session(
            req.session_id,
            PASSKEY_REGISTRATION_SESSION,
            Some(user.id),
            &state,
        )?;
        self.store.save_passkey_session(session).await?;
        Ok(PasskeyFlowResponse::Begin(PasskeyBegin {
            options: serde_json::to_value(options).map_err(serde_security_error)?,
        }))
    }

    async fn passkey_register_finish(
        &self,
        req: PasskeyFlowRequest,
        options: &BTreeMap<String, String>,
    ) -> Result<PasskeyFlowResponse, SecurityError> {
        let user = req.user.ok_or_else(|| SecurityError::business("未登录"))?;
        let webauthn = build_webauthn(options, &req.request)?;
        let registration = parse_passkey_payload::<RegisterPublicKeyCredential>(req.payload)?;
        let session = self
            .store
            .pop_passkey_session(&req.session_id, PASSKEY_REGISTRATION_SESSION)
            .await?
            .ok_or_else(|| SecurityError::business("Passkey 会话不存在或已过期"))?;
        if session.user_id != Some(user.id) {
            return Err(SecurityError::business("Passkey 会话用户不匹配"));
        }
        let state = decode_passkey_session::<PasskeyRegistration>(session)?;
        let passkey = webauthn
            .finish_passkey_registration(&registration, &state)
            .map_err(passkey_webauthn_error)?;
        let credential_id = passkey_credential_id(&passkey);
        if let Some(existing) = self
            .store
            .get_passkey_by_credential_id(&credential_id)
            .await?
            .filter(|record| record.user_id != user.id)
        {
            return Err(SecurityError::business(format!(
                "Passkey 已被用户 {} 绑定",
                existing.user_id
            )));
        }
        let now = now_unix();
        self.store
            .upsert_passkey(PasskeyRecord {
                id: 0,
                user_id: user.id,
                user_uuid: passkey_user_uuid(user.id),
                credential_id,
                passkey,
                last_used_at: None,
                created_at: now,
                updated_at: now,
            })
            .await?;
        Ok(PasskeyFlowResponse::Finished { user_id: None })
    }

    async fn passkey_login_begin(
        &self,
        req: PasskeyFlowRequest,
        options: &BTreeMap<String, String>,
    ) -> Result<PasskeyFlowResponse, SecurityError> {
        let webauthn = build_webauthn(options, &req.request)?;
        let (options, state) = webauthn
            .start_discoverable_authentication()
            .map_err(passkey_webauthn_error)?;
        let session = encode_passkey_session(req.session_id, PASSKEY_LOGIN_SESSION, None, &state)?;
        self.store.save_passkey_session(session).await?;
        Ok(PasskeyFlowResponse::Begin(PasskeyBegin {
            options: serde_json::to_value(options).map_err(serde_security_error)?,
        }))
    }

    async fn passkey_login_finish(
        &self,
        req: PasskeyFlowRequest,
        options: &BTreeMap<String, String>,
    ) -> Result<PasskeyFlowResponse, SecurityError> {
        let webauthn = build_webauthn(options, &req.request)?;
        let credential = parse_passkey_payload::<PublicKeyCredential>(req.payload)?;
        let session = self
            .store
            .pop_passkey_session(&req.session_id, PASSKEY_LOGIN_SESSION)
            .await?
            .ok_or_else(|| SecurityError::business("Passkey 会话不存在或已过期"))?;
        let state = decode_passkey_session::<DiscoverableAuthentication>(session)?;
        let identified = webauthn
            .identify_discoverable_authentication(&credential)
            .map_err(passkey_webauthn_error)?;
        let credential_id = raw_credential_id(identified.1);
        let Some(mut record) = self
            .store
            .get_passkey_by_credential_id(&credential_id)
            .await?
        else {
            return Err(SecurityError::business(PASSKEY_NOT_BOUND_MESSAGE));
        };
        if record.user_uuid != identified.0 {
            return Err(SecurityError::business("用户句柄与凭证不匹配"));
        }
        let keys = [DiscoverableKey::from(record.passkey.clone())];
        let result = webauthn
            .finish_discoverable_authentication(&credential, state, &keys)
            .map_err(passkey_webauthn_error)?;
        record.passkey.update_credential(&result);
        record.last_used_at = Some(now_unix());
        record.updated_at = now_unix();
        self.store.upsert_passkey(record.clone()).await?;
        Ok(PasskeyFlowResponse::Finished {
            user_id: Some(record.user_id),
        })
    }

    async fn passkey_verify_begin(
        &self,
        req: PasskeyFlowRequest,
        options: &BTreeMap<String, String>,
    ) -> Result<PasskeyFlowResponse, SecurityError> {
        let user = req.user.ok_or_else(|| SecurityError::business("未登录"))?;
        let Some(record) = self.store.get_passkey_by_user(user.id).await? else {
            return Err(SecurityError::business(PASSKEY_NOT_BOUND_MESSAGE));
        };
        let webauthn = build_webauthn(options, &req.request)?;
        let (options, state) = webauthn
            .start_passkey_authentication(&[record.passkey])
            .map_err(passkey_webauthn_error)?;
        let session = encode_passkey_session(
            req.session_id,
            PASSKEY_VERIFY_SESSION,
            Some(user.id),
            &state,
        )?;
        self.store.save_passkey_session(session).await?;
        Ok(PasskeyFlowResponse::Begin(PasskeyBegin {
            options: serde_json::to_value(options).map_err(serde_security_error)?,
        }))
    }

    async fn passkey_verify_finish(
        &self,
        req: PasskeyFlowRequest,
        options: &BTreeMap<String, String>,
    ) -> Result<PasskeyFlowResponse, SecurityError> {
        let user = req.user.ok_or_else(|| SecurityError::business("未登录"))?;
        let Some(mut record) = self.store.get_passkey_by_user(user.id).await? else {
            return Err(SecurityError::business(PASSKEY_NOT_BOUND_MESSAGE));
        };
        let webauthn = build_webauthn(options, &req.request)?;
        let credential = parse_passkey_payload::<PublicKeyCredential>(req.payload)?;
        let session = self
            .store
            .pop_passkey_session(&req.session_id, PASSKEY_VERIFY_SESSION)
            .await?
            .ok_or_else(|| SecurityError::business("Passkey 会话不存在或已过期"))?;
        if session.user_id != Some(user.id) {
            return Err(SecurityError::business("Passkey 会话用户不匹配"));
        }
        let state = decode_passkey_session::<PasskeyAuthentication>(session)?;
        let result = webauthn
            .finish_passkey_authentication(&credential, &state)
            .map_err(passkey_webauthn_error)?;
        record.passkey.update_credential(&result);
        record.last_used_at = Some(now_unix());
        record.updated_at = now_unix();
        self.store.upsert_passkey(record).await?;
        self.store
            .save_passkey_session(passkey_ready_session(req.session_id, user.id))
            .await?;
        Ok(PasskeyFlowResponse::Finished { user_id: None })
    }

    async fn verify_two_fa_or_backup(
        &self,
        two_fa: &mut TwoFaRecord,
        code: &str,
    ) -> Result<bool, SecurityError> {
        let clean_code = validate_numeric_code(code);
        if let Ok(clean_code) = clean_code {
            if self
                .validate_totp_and_update_usage(two_fa, &clean_code)
                .await?
            {
                return Ok(true);
            }
        }
        if self
            .validate_backup_code_and_update_usage(two_fa, code)
            .await?
        {
            return Ok(true);
        }
        Ok(false)
    }

    async fn validate_totp_and_update_usage(
        &self,
        two_fa: &mut TwoFaRecord,
        code: &str,
    ) -> Result<bool, SecurityError> {
        let now = now_unix();
        if two_fa.is_locked(now) {
            return Err(SecurityError::business(two_fa.locked_message()));
        }
        if !validate_totp_code(&two_fa.secret, code, now) {
            self.increment_failed_attempts(two_fa, now).await?;
            return Ok(false);
        }
        self.reset_usage(two_fa, now).await?;
        Ok(true)
    }

    async fn validate_backup_code_and_update_usage(
        &self,
        two_fa: &mut TwoFaRecord,
        code: &str,
    ) -> Result<bool, SecurityError> {
        let now = now_unix();
        if two_fa.is_locked(now) {
            return Err(SecurityError::business(two_fa.locked_message()));
        }
        let valid = self.store.consume_backup_code(two_fa.user_id, code).await?;
        if !valid {
            self.increment_failed_attempts(two_fa, now).await?;
            return Ok(false);
        }
        self.reset_usage(two_fa, now).await?;
        Ok(true)
    }

    async fn increment_failed_attempts(
        &self,
        two_fa: &mut TwoFaRecord,
        now: i64,
    ) -> Result<(), SecurityError> {
        two_fa.failed_attempts = two_fa.failed_attempts.saturating_add(1);
        if two_fa.failed_attempts >= MAX_FAIL_ATTEMPTS {
            two_fa.locked_until = Some(now.saturating_add(LOCKOUT_DURATION_SECONDS));
        }
        two_fa.updated_at = now;
        self.store.upsert_two_fa(two_fa.clone()).await?;
        Ok(())
    }

    async fn reset_usage(&self, two_fa: &mut TwoFaRecord, now: i64) -> Result<(), SecurityError> {
        two_fa.failed_attempts = 0;
        two_fa.locked_until = None;
        two_fa.last_used_at = Some(now);
        two_fa.updated_at = now;
        self.store.upsert_two_fa(two_fa.clone()).await?;
        Ok(())
    }
}

impl TwoFaRecord {
    fn is_locked(&self, now: i64) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }

    fn locked_message(&self) -> String {
        format!(
            "账户已被锁定，请在{}后重试",
            self.locked_until.unwrap_or_default()
        )
    }
}

pub(crate) fn passkey_login_enabled(options: &BTreeMap<String, String>) -> bool {
    option_bool(options, "passkey.enabled", false)
        || option_bool(options, "PasskeyLoginEnabled", false)
}

#[derive(Debug, Clone)]
struct PasskeySettings {
    enabled: bool,
    rp_display_name: String,
    rp_id: String,
    origins: String,
    allow_insecure_origin: bool,
}

impl PasskeySettings {
    fn from_options(options: &BTreeMap<String, String>) -> Self {
        Self {
            enabled: passkey_login_enabled(options),
            rp_display_name: option_string(options, "passkey.rp_display_name", "Halolake"),
            rp_id: option_string(options, "passkey.rp_id", ""),
            origins: option_string(options, "passkey.origins", ""),
            allow_insecure_origin: option_bool(options, "passkey.allow_insecure_origin", false),
        }
    }
}

fn build_webauthn(
    options: &BTreeMap<String, String>,
    request: &PasskeyRequestContext,
) -> Result<Webauthn, SecurityError> {
    let settings = PasskeySettings::from_options(options);
    if !settings.enabled {
        return Err(SecurityError::business(PASSKEY_DISABLED_MESSAGE));
    }
    let origins = resolve_passkey_origins(request, &settings)?;
    let first_origin = origins
        .first()
        .ok_or_else(|| SecurityError::business("无法确定 Passkey 的 Origin"))?;
    let rp_id = resolve_passkey_rp_id(request, &settings, first_origin)?;
    let first_url = Url::parse(first_origin).map_err(passkey_config_error)?;
    let mut builder = WebauthnBuilder::new(&rp_id, &first_url).map_err(passkey_webauthn_error)?;
    for origin in origins.iter().skip(1) {
        let origin = Url::parse(origin).map_err(passkey_config_error)?;
        builder = builder.append_allowed_origin(&origin);
    }
    builder = builder
        .rp_name(settings.rp_display_name.trim())
        .timeout(Duration::from_secs(PASSKEY_SESSION_TTL_SECONDS as u64));
    builder.build().map_err(passkey_webauthn_error)
}

fn resolve_passkey_origins(
    request: &PasskeyRequestContext,
    settings: &PasskeySettings,
) -> Result<Vec<String>, SecurityError> {
    let configured = settings.origins.trim();
    if !configured.is_empty() {
        let origins = configured
            .split(',')
            .filter_map(|origin| {
                let origin = origin.trim();
                (!origin.is_empty()).then_some(origin.to_string())
            })
            .collect::<Vec<_>>();
        if origins.is_empty() {
            return auto_passkey_origin(request, settings);
        }
        for origin in &origins {
            if !settings.allow_insecure_origin && origin.to_ascii_lowercase().starts_with("http://")
            {
                return Err(SecurityError::business(format!(
                    "Passkey 不允许使用不安全的 Origin: {origin}"
                )));
            }
        }
        return Ok(origins);
    }
    auto_passkey_origin(request, settings)
}

fn auto_passkey_origin(
    request: &PasskeyRequestContext,
    settings: &PasskeySettings,
) -> Result<Vec<String>, SecurityError> {
    let scheme = detect_passkey_scheme(request);
    let host = request
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| {
            SecurityError::business("无法确定 Passkey 的 Origin，请配置 passkey.origins")
        })?;
    let host_name = host_without_port(host);
    if scheme == "http" && !settings.allow_insecure_origin && !is_local_passkey_host(&host_name) {
        return Err(SecurityError::business(format!(
            "Passkey 仅支持 HTTPS，当前访问: {scheme}://{host}，请允许不安全 Origin 或配置 HTTPS"
        )));
    }
    Ok(vec![format!("{scheme}://{host}")])
}

fn resolve_passkey_rp_id(
    request: &PasskeyRequestContext,
    settings: &PasskeySettings,
    first_origin: &str,
) -> Result<String, SecurityError> {
    let configured = settings.rp_id.trim();
    if !configured.is_empty() {
        return Ok(host_without_port(configured));
    }
    if let Ok(url) = Url::parse(first_origin) {
        if let Some(host) = url.host_str() {
            return Ok(host_without_port(host));
        }
    }
    request
        .host
        .as_deref()
        .map(host_without_port)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| SecurityError::business("Passkey 未配置 Origin，无法推导 RPID"))
}

fn detect_passkey_scheme(request: &PasskeyRequestContext) -> String {
    if let Some(proto) = request
        .forwarded_proto
        .as_deref()
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return proto.to_ascii_lowercase();
    }
    if let Some(scheme) = request
        .uri_scheme
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return scheme.to_ascii_lowercase();
    }
    "http".to_string()
}

fn host_without_port(host: &str) -> String {
    let host = host.trim();
    if let Some(stripped) = host.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            return stripped[..end].to_string();
        }
    }
    if host.matches(':').count() == 1 {
        if let Some((name, port)) = host.rsplit_once(':') {
            if port.parse::<u16>().is_ok() {
                return name.to_string();
            }
        }
    }
    host.to_string()
}

fn is_local_passkey_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host.starts_with("127.")
}

fn passkey_config_error(err: impl std::fmt::Display) -> SecurityError {
    SecurityError::business(err.to_string())
}

fn passkey_webauthn_error(err: impl std::fmt::Debug) -> SecurityError {
    SecurityError::business(format!("Passkey 验证失败: {err:?}"))
}

fn passkey_user_uuid(user_id: u64) -> Uuid {
    Uuid::from_u128(user_id as u128)
}

fn passkey_credential_id(passkey: &Passkey) -> String {
    BASE64URL_NOPAD.encode(passkey.cred_id())
}

fn raw_credential_id(raw_id: &[u8]) -> String {
    BASE64URL_NOPAD.encode(raw_id)
}

fn parse_passkey_payload<T: DeserializeOwned>(
    payload: Option<serde_json::Value>,
) -> Result<T, SecurityError> {
    let Some(payload) = payload else {
        return Err(SecurityError::business("Passkey 请求数据不能为空"));
    };
    serde_json::from_value(payload).map_err(|err| SecurityError::business(err.to_string()))
}

fn encode_passkey_session<T: Serialize>(
    session_id: String,
    kind: &'static str,
    user_id: Option<u64>,
    state: &T,
) -> Result<PasskeySessionRecord, SecurityError> {
    let now = now_unix();
    Ok(PasskeySessionRecord {
        session_id,
        kind,
        payload: serde_json::to_string(state).map_err(serde_security_error)?,
        user_id,
        expires_at: now.saturating_add(PASSKEY_SESSION_TTL_SECONDS),
        created_at: now,
    })
}

fn decode_passkey_session<T: DeserializeOwned>(
    session: PasskeySessionRecord,
) -> Result<T, SecurityError> {
    serde_json::from_str(&session.payload).map_err(serde_security_error)
}

fn passkey_ready_session(session_id: String, user_id: u64) -> PasskeySessionRecord {
    let now = now_unix();
    PasskeySessionRecord {
        session_id,
        kind: PASSKEY_READY_SESSION,
        payload: "{}".to_string(),
        user_id: Some(user_id),
        expires_at: now.saturating_add(PASSKEY_READY_TTL_SECONDS),
        created_at: now,
    }
}

fn passkey_user_name(user: &PasskeyUser) -> String {
    let username = user.username.trim();
    if username.is_empty() {
        format!("user-{}", user.id)
    } else {
        username.to_string()
    }
}

fn passkey_user_display_name(user: &PasskeyUser) -> String {
    let display_name = user.display_name.trim();
    if display_name.is_empty() {
        passkey_user_name(user)
    } else {
        display_name.to_string()
    }
}

fn option_bool(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    options
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn option_string(options: &BTreeMap<String, String>, key: &str, default: &str) -> String {
    options
        .get(key)
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn can_manage_target_role(actor_role: i32, target_role: i32) -> bool {
    actor_role > target_role
}

fn two_fa_stats_from_records(
    total_users: usize,
    records: impl IntoIterator<Item = TwoFaRecord>,
) -> TwoFaStats {
    let now = now_unix();
    let mut enabled_users = 0usize;
    let mut locked_users = 0usize;
    for record in records {
        if record.is_enabled {
            enabled_users = enabled_users.saturating_add(1);
        }
        if record.is_locked(now) {
            locked_users = locked_users.saturating_add(1);
        }
    }
    let enabled_rate = if total_users == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", enabled_users as f64 / total_users as f64 * 100.0)
    };
    TwoFaStats {
        total_users,
        enabled_users,
        enabled_rate,
        locked_users,
    }
}

fn generate_totp_secret() -> String {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut bytes = [0u8; 20];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(&second.as_bytes()[..4]);
    BASE32_NOPAD.encode(&bytes)
}

fn validate_totp_code(secret: &str, code: &str, now: i64) -> bool {
    let clean_code = match validate_numeric_code(code) {
        Ok(code) => code,
        Err(_) => return false,
    };
    let secret = secret.trim().to_ascii_uppercase();
    let Ok(key) = BASE32_NOPAD.decode(secret.as_bytes()) else {
        return false;
    };
    let current_counter = now.div_euclid(TOTP_PERIOD_SECONDS).max(0) as u64;
    (-TOTP_WINDOW..=TOTP_WINDOW).any(|skew| {
        let counter = if skew.is_negative() {
            current_counter.saturating_sub(skew.unsigned_abs())
        } else {
            current_counter.saturating_add(skew as u64)
        };
        hotp(&key, counter).is_some_and(|otp| format!("{otp:06}") == clean_code)
    })
}

fn hotp(key: &[u8], counter: u64) -> Option<u32> {
    let mut mac = HmacSha1::new_from_slice(key).ok()?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let binary = (u32::from(digest[offset] & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    Some(binary % 10u32.pow(TOTP_DIGITS))
}

fn validate_numeric_code(code: &str) -> Result<String, SecurityError> {
    let code = code.replace(' ', "");
    if code.len() != 6 {
        return Err(SecurityError::business("验证码必须是6位数字"));
    }
    if !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SecurityError::business("验证码只能包含数字"));
    }
    Ok(code)
}

fn generate_backup_codes() -> Vec<String> {
    (0..BACKUP_CODE_COUNT)
        .map(|_| generate_backup_code())
        .collect()
}

fn generate_backup_code() -> String {
    const CHARSET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let first = Uuid::new_v4();
    let bytes = first.as_bytes();
    let mut code = [0u8; BACKUP_CODE_LENGTH];
    for (idx, slot) in code.iter_mut().enumerate() {
        *slot = CHARSET[usize::from(bytes[idx]) % CHARSET.len()];
    }
    format!(
        "{}-{}",
        std::str::from_utf8(&code[..4]).unwrap_or("AAAA"),
        std::str::from_utf8(&code[4..]).unwrap_or("AAAA")
    )
}

fn validate_backup_code_format(code: &str) -> bool {
    let clean = code.replace('-', "").to_ascii_uppercase();
    clean.len() == BACKUP_CODE_LENGTH
        && clean
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn normalize_backup_code(code: &str) -> String {
    let clean = code.replace('-', "").to_ascii_uppercase();
    if clean.len() == BACKUP_CODE_LENGTH {
        format!("{}-{}", &clean[..4], &clean[4..])
    } else {
        code.to_string()
    }
}

fn hash_backup_code(code: &str) -> Result<String, SecurityError> {
    hash(normalize_backup_code(code), DEFAULT_COST)
        .map_err(|err| SecurityError::Management(ManagementError::PasswordHash(err.to_string())))
}

fn generate_qr_code_data(secret: &str, username: &str, issuer: &str) -> String {
    let account_name = format!("{username} ({issuer})");
    format!(
        "otpauth://totp/{issuer}:{account_name}?secret={secret}&issuer={issuer}&digits=6&period=30"
    )
}

fn sqlx_security_error(err: sqlx::Error) -> SecurityError {
    SecurityError::Management(ManagementError::Storage(err.to_string()))
}

fn serde_security_error(err: serde_json::Error) -> SecurityError {
    SecurityError::Management(ManagementError::Storage(err.to_string()))
}

fn i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i64, SecurityError> {
    row.try_get::<i64, _>(name).map_err(sqlx_security_error)
}

fn u64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<u64, SecurityError> {
    Ok(i64_col(row, name)?.max(0) as u64)
}

fn i32_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i32, SecurityError> {
    Ok(i64_col(row, name)?.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

fn bool_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<bool, SecurityError> {
    Ok(i64_col(row, name)? != 0)
}

fn string_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, SecurityError> {
    row.try_get::<String, _>(name).map_err(sqlx_security_error)
}

fn optional_i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Option<i64> {
    row.try_get::<Option<i64>, _>(name).ok().flatten()
}


fn mysql_i64_col(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<i64, SecurityError> {
    row.try_get::<i64, _>(name).map_err(sqlx_security_error)
}

fn mysql_u64_col(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<u64, SecurityError> {
    Ok(mysql_i64_col(row, name)?.max(0) as u64)
}

fn mysql_i32_col(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<i32, SecurityError> {
    Ok(mysql_i64_col(row, name)?.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

fn mysql_bool_col(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<bool, SecurityError> {
    Ok(mysql_i64_col(row, name)? != 0)
}

fn mysql_string_col(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<String, SecurityError> {
    row.try_get::<String, _>(name).map_err(sqlx_security_error)
}

fn optional_mysql_i64_col(row: &sqlx::mysql::MySqlRow, name: &str) -> Option<i64> {
    row.try_get::<Option<i64>, _>(name).ok().flatten()
}

fn passkey_record_from_mysql_row(
    row: &sqlx::mysql::MySqlRow,
) -> Result<PasskeyRecord, SecurityError> {
    let user_uuid = mysql_string_col(row, "user_uuid")?
        .parse::<Uuid>()
        .map_err(|err| SecurityError::Management(ManagementError::Storage(err.to_string())))?;
    let passkey = serde_json::from_str::<Passkey>(&mysql_string_col(row, "passkey_json")?)
        .map_err(serde_security_error)?;
    Ok(PasskeyRecord {
        id: mysql_u64_col(row, "id")?,
        user_id: mysql_u64_col(row, "user_id")?,
        user_uuid,
        credential_id: mysql_string_col(row, "credential_id")?,
        passkey,
        last_used_at: optional_mysql_i64_col(row, "last_used_at"),
        created_at: mysql_i64_col(row, "created_at")?,
        updated_at: mysql_i64_col(row, "updated_at")?,
    })
}

fn passkey_session_from_mysql_row(
    row: &sqlx::mysql::MySqlRow,
) -> Result<PasskeySessionRecord, SecurityError> {
    let kind = passkey_session_kind_static(&mysql_string_col(row, "kind")?)?;
    Ok(PasskeySessionRecord {
        session_id: mysql_string_col(row, "session_id")?,
        kind,
        payload: mysql_string_col(row, "payload")?,
        user_id: optional_mysql_i64_col(row, "user_id").map(|user_id| user_id.max(0) as u64),
        expires_at: mysql_i64_col(row, "expires_at")?,
        created_at: mysql_i64_col(row, "created_at")?,
    })
}

fn pg_i64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<i64, SecurityError> {
    // Postgres INTEGER is INT4; BIGINT is INT8. Accept both.
    if let Ok(v) = row.try_get::<i64, _>(name) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<i32, _>(name) {
        return Ok(i64::from(v));
    }
    if let Ok(v) = row.try_get::<bool, _>(name) {
        return Ok(i64::from(v));
    }
    row.try_get::<i64, _>(name).map_err(sqlx_security_error)
}

fn pg_u64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<u64, SecurityError> {
    Ok(pg_i64_col(row, name)?.max(0) as u64)
}

fn pg_i32_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<i32, SecurityError> {
    if let Ok(v) = row.try_get::<i32, _>(name) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<i64, _>(name) {
        return Ok(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
    }
    if let Ok(v) = row.try_get::<bool, _>(name) {
        return Ok(i32::from(v));
    }
    row.try_get::<i32, _>(name).map_err(sqlx_security_error)
}

fn pg_bool_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<bool, SecurityError> {
    if let Ok(v) = row.try_get::<bool, _>(name) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<i32, _>(name) {
        return Ok(v != 0);
    }
    if let Ok(v) = row.try_get::<i64, _>(name) {
        return Ok(v != 0);
    }
    row.try_get::<bool, _>(name).map_err(sqlx_security_error)
}

fn pg_string_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<String, SecurityError> {
    row.try_get::<String, _>(name).map_err(sqlx_security_error)
}

fn pg_optional_i64_col(row: &sqlx::postgres::PgRow, name: &str) -> Option<i64> {
    if let Ok(v) = row.try_get::<Option<i64>, _>(name) {
        return v;
    }
    row.try_get::<Option<i32>, _>(name)
        .ok()
        .flatten()
        .map(i64::from)
}

fn passkey_record_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<PasskeyRecord, SecurityError> {
    let user_uuid = string_col(row, "user_uuid")?
        .parse::<Uuid>()
        .map_err(|err| SecurityError::Management(ManagementError::Storage(err.to_string())))?;
    let passkey = serde_json::from_str::<Passkey>(&string_col(row, "passkey_json")?)
        .map_err(serde_security_error)?;
    Ok(PasskeyRecord {
        id: u64_col(row, "id")?,
        user_id: u64_col(row, "user_id")?,
        user_uuid,
        credential_id: string_col(row, "credential_id")?,
        passkey,
        last_used_at: optional_i64_col(row, "last_used_at"),
        created_at: i64_col(row, "created_at")?,
        updated_at: i64_col(row, "updated_at")?,
    })
}

fn passkey_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<PasskeyRecord, SecurityError> {
    let user_uuid = pg_string_col(row, "user_uuid")?
        .parse::<Uuid>()
        .map_err(|err| SecurityError::Management(ManagementError::Storage(err.to_string())))?;
    let passkey = serde_json::from_str::<Passkey>(&pg_string_col(row, "passkey_json")?)
        .map_err(serde_security_error)?;
    Ok(PasskeyRecord {
        id: pg_u64_col(row, "id")?,
        user_id: pg_u64_col(row, "user_id")?,
        user_uuid,
        credential_id: pg_string_col(row, "credential_id")?,
        passkey,
        last_used_at: pg_optional_i64_col(row, "last_used_at"),
        created_at: pg_i64_col(row, "created_at")?,
        updated_at: pg_i64_col(row, "updated_at")?,
    })
}

fn passkey_session_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<PasskeySessionRecord, SecurityError> {
    let kind = passkey_session_kind_static(&string_col(row, "kind")?)?;
    Ok(PasskeySessionRecord {
        session_id: string_col(row, "session_id")?,
        kind,
        payload: string_col(row, "payload")?,
        user_id: optional_i64_col(row, "user_id").map(|user_id| user_id.max(0) as u64),
        expires_at: i64_col(row, "expires_at")?,
        created_at: i64_col(row, "created_at")?,
    })
}

fn passkey_session_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<PasskeySessionRecord, SecurityError> {
    let kind = passkey_session_kind_static(&pg_string_col(row, "kind")?)?;
    Ok(PasskeySessionRecord {
        session_id: pg_string_col(row, "session_id")?,
        kind,
        payload: pg_string_col(row, "payload")?,
        user_id: pg_optional_i64_col(row, "user_id").map(|user_id| user_id.max(0) as u64),
        expires_at: pg_i64_col(row, "expires_at")?,
        created_at: pg_i64_col(row, "created_at")?,
    })
}

fn passkey_session_kind_static(kind: &str) -> Result<&'static str, SecurityError> {
    match kind {
        PASSKEY_REGISTRATION_SESSION => Ok(PASSKEY_REGISTRATION_SESSION),
        PASSKEY_LOGIN_SESSION => Ok(PASSKEY_LOGIN_SESSION),
        PASSKEY_VERIFY_SESSION => Ok(PASSKEY_VERIFY_SESSION),
        PASSKEY_READY_SESSION => Ok(PASSKEY_READY_SESSION),
        _ => Err(SecurityError::Management(ManagementError::Storage(
            format!("unknown passkey session kind: {kind}"),
        ))),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::OptionStore;

    fn service() -> SecurityService {
        SecurityService::new(
            OptionStore::memory(BTreeMap::new()),
            SecurityStore::memory(),
        )
    }

    fn passkey_request(flow: PasskeyFlow) -> PasskeyFlowRequest {
        PasskeyFlowRequest {
            user: None,
            flow,
            session_id: "test-session".to_string(),
            request: PasskeyRequestContext {
                host: Some("localhost:3000".to_string()),
                forwarded_proto: Some("http".to_string()),
                uri_scheme: None,
            },
            payload: None,
        }
    }

    #[tokio::test]
    async fn reports_disabled_security_methods_like_new_api() {
        let service = service();

        let two_fa = service
            .call(GetTwoFaStatusRequest { user_id: 1 })
            .await
            .expect("two fa status");
        assert_eq!(
            two_fa,
            TwoFaStatus {
                enabled: false,
                locked: false,
                backup_codes_remaining: None,
            }
        );

        let passkey = service
            .call(GetPasskeyStatusRequest { user_id: 1 })
            .await
            .expect("passkey status");
        assert_eq!(
            passkey,
            PasskeyStatus {
                enabled: false,
                last_used_at: None,
            }
        );
    }

    #[tokio::test]
    async fn setup_and_enable_two_fa() {
        let service = service();
        let setup = service
            .call(StartTwoFaSetupRequest {
                user_id: 1,
                username: "alice".to_string(),
                issuer: "Halolake".to_string(),
            })
            .await
            .expect("setup");
        assert_eq!(setup.backup_codes.len(), BACKUP_CODE_COUNT);
        assert!(setup.qr_code_data.starts_with("otpauth://totp/"));

        let code = format!(
            "{:06}",
            hotp(
                &BASE32_NOPAD
                    .decode(setup.secret.as_bytes())
                    .expect("secret"),
                now_unix().div_euclid(TOTP_PERIOD_SECONDS) as u64
            )
            .expect("hotp")
        );
        service
            .call(EnableTwoFaRequest { user_id: 1, code })
            .await
            .expect("enable");

        let status = service
            .call(GetTwoFaStatusRequest { user_id: 1 })
            .await
            .expect("status");
        assert!(status.enabled);
        assert_eq!(status.backup_codes_remaining, Some(BACKUP_CODE_COUNT));
    }

    #[tokio::test]
    async fn backup_code_is_single_use_for_disable() {
        let service = service();
        let setup = service
            .call(StartTwoFaSetupRequest {
                user_id: 1,
                username: "alice".to_string(),
                issuer: "Halolake".to_string(),
            })
            .await
            .expect("setup");
        let code = format!(
            "{:06}",
            hotp(
                &BASE32_NOPAD
                    .decode(setup.secret.as_bytes())
                    .expect("secret"),
                now_unix().div_euclid(TOTP_PERIOD_SECONDS) as u64
            )
            .expect("hotp")
        );
        service
            .call(EnableTwoFaRequest { user_id: 1, code })
            .await
            .expect("enable");
        service
            .call(DisableTwoFaRequest {
                user_id: 1,
                code: setup.backup_codes[0].clone(),
            })
            .await
            .expect("disable");
        let status = service
            .call(GetTwoFaStatusRequest { user_id: 1 })
            .await
            .expect("status");
        assert!(!status.enabled);
    }

    #[tokio::test]
    async fn passkey_begin_keeps_new_api_disabled_message() {
        let service = service();

        let err = service
            .call(passkey_request(PasskeyFlow::LoginBegin))
            .await
            .expect_err("disabled");

        assert_eq!(err.message(), PASSKEY_DISABLED_MESSAGE);
    }

    #[test]
    fn passkey_enabled_accepts_new_and_legacy_option_keys() {
        assert!(!passkey_login_enabled(&BTreeMap::new()));
        assert!(passkey_login_enabled(&BTreeMap::from([(
            "PasskeyLoginEnabled".to_string(),
            "true".to_string(),
        )])));
        assert!(passkey_login_enabled(&BTreeMap::from([(
            "passkey.enabled".to_string(),
            "true".to_string(),
        )])));
        assert!(passkey_login_enabled(&BTreeMap::from([
            ("PasskeyLoginEnabled".to_string(), "true".to_string()),
            ("passkey.enabled".to_string(), "false".to_string()),
        ])));
        assert!(!passkey_login_enabled(&BTreeMap::from([
            ("PasskeyLoginEnabled".to_string(), "false".to_string()),
            ("passkey.enabled".to_string(), "false".to_string()),
        ])));
    }
}
