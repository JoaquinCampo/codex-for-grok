use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{fs::OpenOptionsExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::warn;

const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_SKEW_SECS: u64 = 120;

#[derive(Clone, Debug)]
pub struct Session {
    pub access_token: String,
    pub account_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Codex auth is unavailable; run `codex login`")]
    Missing,
    #[error("Codex auth file permissions must be owner-only (0600)")]
    InsecurePermissions,
    #[error("Codex auth file is invalid")]
    Invalid,
    #[error("Codex session refresh failed")]
    Refresh,
    #[error("Codex auth file could not be updated securely")]
    Write,
}

#[derive(Clone)]
pub struct AuthManager {
    path: PathBuf,
    client: Client,
    refresh_lock: std::sync::Arc<Mutex<()>>,
    refresh_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl AuthManager {
    pub fn new(path: PathBuf, client: Client) -> Self {
        Self {
            path,
            client,
            refresh_lock: std::sync::Arc::new(Mutex::new(())),
            refresh_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn refresh_count(&self) -> u64 {
        self.refresh_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn session(&self) -> Result<Session, AuthError> {
        let path = self.path.clone();
        let (auth, session) = tokio::task::spawn_blocking(move || load_auth(&path))
            .await
            .map_err(|_| AuthError::Invalid)??;

        if token_expires_soon(&session.access_token) {
            return self.refresh_if_unchanged(Some(&session.access_token)).await;
        }

        let _ = auth;
        Ok(session)
    }

    pub async fn refresh_if_unchanged(
        &self,
        failed_token: Option<&str>,
    ) -> Result<Session, AuthError> {
        let _guard = self.refresh_lock.lock().await;
        let path = self.path.clone();
        let (auth, session) = tokio::task::spawn_blocking(move || load_auth(&path))
            .await
            .map_err(|_| AuthError::Invalid)??;

        if failed_token.is_some_and(|token| token != session.access_token) {
            return Ok(session);
        }

        let refresh_token = auth
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(AuthError::Missing)?;

        let response = self
            .client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await
            .map_err(|_| AuthError::Refresh)?;
        if !response.status().is_success() {
            return Err(AuthError::Refresh);
        }
        let refreshed: RefreshResponse = response.json().await.map_err(|_| AuthError::Refresh)?;
        if refreshed.access_token.is_empty() {
            return Err(AuthError::Refresh);
        }

        let updated = merge_refresh(auth, &refreshed)?;
        let updated_session = session_from_auth(&updated)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || secure_write_auth(&path, &updated))
            .await
            .map_err(|_| AuthError::Write)??;
        self.refresh_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(updated_session)
    }

    pub async fn check_ready(&self) -> Result<(), AuthError> {
        let _ = self.session().await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

fn load_auth(path: &Path) -> Result<(Value, Session), AuthError> {
    check_auth_permissions(path)?;
    let content = fs::read_to_string(path).map_err(|_| AuthError::Missing)?;
    let auth: Value = serde_json::from_str(&content).map_err(|_| AuthError::Invalid)?;
    let session = session_from_auth(&auth)?;
    Ok((auth, session))
}

fn session_from_auth(auth: &Value) -> Result<Session, AuthError> {
    let tokens = auth
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or(AuthError::Invalid)?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(AuthError::Missing)?;
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(AuthError::Missing)?;
    Ok(Session {
        access_token: access_token.to_owned(),
        account_id: account_id.to_owned(),
    })
}

fn merge_refresh(mut auth: Value, refreshed: &RefreshResponse) -> Result<Value, AuthError> {
    let tokens = auth
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or(AuthError::Invalid)?;
    tokens.insert("access_token".to_owned(), json!(refreshed.access_token));
    if let Some(refresh_token) = &refreshed.refresh_token {
        tokens.insert("refresh_token".to_owned(), json!(refresh_token));
    }
    if let Some(id_token) = &refreshed.id_token {
        tokens.insert("id_token".to_owned(), json!(id_token));
    }
    Ok(auth)
}

fn check_auth_permissions(path: &Path) -> Result<(), AuthError> {
    let metadata = fs::metadata(path).map_err(|_| AuthError::Missing)?;
    if metadata.permissions().mode() & 0o077 != 0 {
        warn!(auth_path = %path.display(), "Codex auth file has unsafe permissions");
        return Err(AuthError::InsecurePermissions);
    }
    Ok(())
}

fn secure_write_auth(path: &Path, auth: &Value) -> Result<(), AuthError> {
    check_auth_permissions(path)?;
    let parent = path.parent().ok_or(AuthError::Write)?;
    let lock_path = path.with_extension("json.bridge.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|_| AuthError::Write)?;
    lock.lock_exclusive().map_err(|_| AuthError::Write)?;

    let suffix = format!("{}.{}", std::process::id(), monotonic_nanos());
    let temporary_path = parent.join(format!(".auth.json.bridge-{suffix}"));
    let serialized = serde_json::to_vec_pretty(auth).map_err(|_| AuthError::Write)?;
    let result = (|| {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)
            .map_err(|_| AuthError::Write)?;
        temporary
            .write_all(&serialized)
            .map_err(|_| AuthError::Write)?;
        temporary.write_all(b"\n").map_err(|_| AuthError::Write)?;
        temporary.sync_all().map_err(|_| AuthError::Write)?;
        fs::rename(&temporary_path, path).map_err(|_| AuthError::Write)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| AuthError::Write)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| AuthError::Write)?;
        check_auth_permissions(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    let _ = FileExt::unlock(&lock);
    result
}

fn token_expires_soon(token: &str) -> bool {
    let Some(payload) = token.split('.').nth(1) else {
        return false;
    };
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&decoded) else {
        return false;
    };
    let Some(expiry) = value.get("exp").and_then(Value::as_u64) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    expiry <= now.saturating_add(REFRESH_SKEW_SECS)
}

fn monotonic_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_preserves_unrelated_auth_fields() {
        let auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": "old", "refresh_token": "keep", "account_id": "account"}
        });
        let refreshed = RefreshResponse {
            access_token: "new".to_owned(),
            refresh_token: None,
            id_token: Some("id".to_owned()),
        };
        let updated = merge_refresh(auth, &refreshed).expect("merge succeeds");
        assert_eq!(updated["auth_mode"], "chatgpt");
        assert_eq!(updated["tokens"]["access_token"], "new");
        assert_eq!(updated["tokens"]["refresh_token"], "keep");
        assert_eq!(updated["tokens"]["id_token"], "id");
    }
}
