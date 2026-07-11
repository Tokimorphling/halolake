//! First-boot secrets: strong admin password + session/internal secrets.
//!
//! When no root user exists, generates credentials and writes them to a file
//! (default `/data/halolake-credentials.txt` or `HALOLAKE_CREDENTIALS_FILE`).
//! Secrets already present in env/config are left unchanged.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use data_encoding::BASE64URL_NOPAD;
use halolake_control_plane::{BootstrapRootUserRequest, ManagementError};
use service_async::Service;
use tracing::info;
use uuid::Uuid;

use crate::storage::ManagementStore;
use halolake_domain::ROLE_ROOT_USER;

const DEFAULT_ADMIN_USERNAME: &str = "admin";

#[derive(Debug, Clone)]
pub(crate) struct BootstrapSecrets {
    pub(crate) session_secret: Option<String>,
    pub(crate) internal_secret: Option<String>,
}

/// Resolve credentials file path (env overrides default).
/// Docker image uses `/data/...`; local dev falls back to `./data/...`.
pub(crate) fn credentials_path() -> PathBuf {
    if let Some(path) = std::env::var("HALOLAKE_CREDENTIALS_FILE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(path);
    }
    let docker = PathBuf::from("/data/halolake-credentials.txt");
    if Path::new("/data").is_dir() {
        return docker;
    }
    PathBuf::from("./data/halolake-credentials.txt")
}

/// Ensure session / internal secrets exist (generate + persist if missing).
pub(crate) fn ensure_runtime_secrets(
    session_secret: Option<String>,
    internal_secret: Option<String>,
) -> Result<BootstrapSecrets> {
    let path = credentials_path();
    let existing = read_kv_file(&path).unwrap_or_default();

    let mut out = BootstrapSecrets {
        session_secret: None,
        internal_secret: None,
    };
    let mut dirty = false;
    let mut lines: Vec<(String, String)> = existing.clone();

    // Empty string from config (`secret = ""`) counts as unset.
    let session = first_nonempty([
        nonempty_owned(session_secret).as_deref(),
        nonempty_owned(std::env::var("SESSION_SECRET").ok()).as_deref(),
        kv_get(&existing, "session_secret"),
    ]);
    let session = match session {
        Some(s) => s,
        None => {
            let s = random_token(32);
            set_kv(&mut lines, "session_secret", &s);
            dirty = true;
            info!(
                path = %path.display(),
                "generated SESSION_SECRET and will write credentials file"
            );
            s
        }
    };
    out.session_secret = Some(session);

    let internal = first_nonempty([
        nonempty_owned(internal_secret).as_deref(),
        nonempty_owned(std::env::var("HALOLAKE_INTERNAL_SECRET").ok()).as_deref(),
        kv_get(&existing, "internal_secret"),
    ]);
    let internal = match internal {
        Some(s) => s,
        None => {
            let s = random_token(32);
            set_kv(&mut lines, "internal_secret", &s);
            dirty = true;
            info!(
                path = %path.display(),
                "generated internal.secret and will write credentials file"
            );
            s
        }
    };
    out.internal_secret = Some(internal);

    if dirty {
        write_kv_file(&path, &lines, None)?;
    }

    Ok(out)
}

/// If no root user exists, create one with a strong random password and append to credentials file.
pub(crate) async fn ensure_root_admin(management: &ManagementStore) -> Result<bool> {
    if root_exists(management)? {
        return Ok(false);
    }

    let path = credentials_path();
    let username = std::env::var("HALOLAKE_ADMIN_USERNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_ADMIN_USERNAME.to_string());
    // Keep username short enough for setup UI constraint (≤12) when using defaults.
    let username = if username.len() > 12 {
        username.chars().take(12).collect()
    } else {
        username
    };

    let password = random_password();
    management
        .call(BootstrapRootUserRequest {
            username: username.clone(),
            password: password.clone(),
        })
        .await
        .map_err(|err| match err {
            ManagementError::Duplicate => anyhow::anyhow!("root already exists"),
            other => anyhow::anyhow!(other),
        })?;

    let mut lines = read_kv_file(&path).unwrap_or_default();
    set_kv(&mut lines, "username", &username);
    set_kv(&mut lines, "password", &password);
    set_kv(&mut lines, "role", "root");
    set_kv(
        &mut lines,
        "generated_at",
        &now_rfc3339_approx(),
    );
    write_kv_file(
        &path,
        &lines,
        Some(&format!(
            "# Halolake bootstrap credentials (generated once)\n\
             # Login at the web UI with username/password below.\n\
             # Change the password after first login and restrict this file (chmod 600).\n\
             # Do not commit this file.\n"
        )),
    )?;

    // Loud but never print the password itself.
    eprintln!(
        "\n\
         ============================================================\n\
         Halolake first-boot admin credentials written to:\n\
           {}\n\
         username: {}\n\
         password: (see file — not printed to logs)\n\
         ============================================================\n",
        path.display(),
        username
    );
    info!(
        path = %path.display(),
        username = %username,
        "generated root admin credentials"
    );
    Ok(true)
}

fn root_exists(management: &ManagementStore) -> Result<bool> {
    let data = management
        .current_data()
        .map_err(|err| anyhow::anyhow!(err))?;
    Ok(data.users.iter().any(|u| u.role == ROLE_ROOT_USER))
}

fn random_token(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    // uuid v4 is good entropy; stitch for longer tokens
    let mut offset = 0;
    while offset < nbytes {
        let u = Uuid::new_v4();
        let bytes = u.as_bytes();
        let n = (nbytes - offset).min(bytes.len());
        buf[offset..offset + n].copy_from_slice(&bytes[..n]);
        offset += n;
    }
    BASE64URL_NOPAD.encode(&buf)
}

fn random_password() -> String {
    // ~43 chars of base64url from 32 bytes (~256 bits)
    random_token(32)
}

fn nonempty_owned(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn first_nonempty<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn kv_get<'a>(lines: &'a [(String, String)], key: &str) -> Option<&'a str> {
    lines
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn set_kv(lines: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(slot) = lines
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
    {
        slot.1 = value.to_string();
    } else {
        lines.push((key.to_string(), value.to_string()));
    }
}

fn read_kv_file(path: &Path) -> Result<Vec<(String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("read credentials file {}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(out)
}

fn write_kv_file(path: &Path, lines: &[(String, String)], header: Option<&str>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create credentials dir {}", parent.display()))?;
        }
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("open credentials file {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    if let Some(header) = header {
        file.write_all(header.as_bytes())?;
    } else {
        file.write_all(
            b"# Halolake runtime secrets / bootstrap credentials\n# chmod 600 recommended\n",
        )?;
    }
    for (k, v) in lines {
        writeln!(file, "{k}={v}")?;
    }
    file.flush()?;
    Ok(())
}

fn now_rfc3339_approx() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // compact unix timestamp is fine for operators; avoids chrono dep
    format!("unix:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn random_password_is_long_and_unique() {
        let a = random_password();
        let b = random_password();
        assert!(a.len() >= 40, "len={}", a.len());
        assert_ne!(a, b);
    }

    #[test]
    fn kv_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "halolake-cred-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("creds.txt");
        let mut lines = vec![("username".into(), "admin".into())];
        set_kv(&mut lines, "password", "secret");
        write_kv_file(&path, &lines, None).unwrap();
        let loaded = read_kv_file(&path).unwrap();
        assert_eq!(kv_get(&loaded, "username"), Some("admin"));
        assert_eq!(kv_get(&loaded, "password"), Some("secret"));
        let _ = fs::remove_dir_all(dir);
    }
}
