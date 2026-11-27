//! Administrative authentication and managed configuration.
use crate::{models::AppState, Config};
use axum::{
    body::{to_bytes, Body},
    extract::{Extension, State},
    http::{header, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use clap::{CommandFactory, FromArgMatches};
use nostr_sdk::prelude::{Kind, PublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, RwLock};

type Env = BTreeMap<String, String>;
static PROCESS_ENV: OnceLock<Env> = OnceLock::new();
static DOTENV_ENV: OnceLock<Env> = OnceLock::new();
static EXTERNAL_ENV: OnceLock<Env> = OnceLock::new();
fn env_snapshot() -> Env {
    std::env::vars().filter(|(_, v)| !v.is_empty()).collect()
}
fn translated_snapshot() -> Env {
    let mut env = env_snapshot();
    let (legacy, _) = crate::config::translate_legacy(|key| env.get(key).cloned());
    for (key, value) in legacy {
        env.entry(key).or_insert(value);
    }
    env
}
pub(crate) fn capture_environment() {
    let _ = PROCESS_ENV.set(translated_snapshot());
}
pub(crate) fn capture_dotenv() {
    let _ = DOTENV_ENV.set(translated_snapshot());
}
pub(crate) fn capture_external() {
    let _ = EXTERNAL_ENV.set(env_snapshot());
}
fn cli_values() -> Env {
    let mut out = Env::new();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        if let Some(flag) = arg.strip_prefix("--") {
            let (key, value) = if let Some((k, v)) = flag.split_once('=') {
                (k.to_string(), v.to_string())
            } else {
                (
                    flag.to_string(),
                    if args.peek().is_some_and(|s| !s.starts_with("--")) {
                        args.next().unwrap_or_default()
                    } else {
                        "true".into()
                    },
                )
            };
            let key = key.replace('-', "_");
            if matches!(
                key.as_str(),
                "admin_npubs"
                    | "allowed_npubs"
                    | "cors_origins"
                    | "upstream_servers"
                    | "cashu_paid"
                    | "dvm_kinds"
                    | "dvm_relays"
            ) {
                out.entry(key)
                    .and_modify(|v: &mut String| {
                        v.push(',');
                        v.push_str(&value);
                    })
                    .or_insert(value);
            } else {
                out.insert(key, value);
            }
        }
    }
    out
}
fn field_name(name: &str) -> String {
    format!("ALMOND_{}", name.to_ascii_uppercase())
}
fn source(name: &str) -> &'static str {
    let key = field_name(name);
    if cli_values().contains_key(name) {
        "cli"
    } else if PROCESS_ENV.get().is_some_and(|m| m.contains_key(&key)) {
        "environment"
    } else if DOTENV_ENV.get().is_some_and(|m| m.contains_key(&key)) {
        "dotenv"
    } else if EXTERNAL_ENV.get().is_some_and(|m| m.contains_key(&key)) {
        "environment"
    } else {
        "default"
    }
}
fn infrastructure(name: &str) -> bool {
    matches!(
        name,
        "config"
            | "managed_config"
            | "admin_npubs"
            | "bind_addr"
            | "public_url"
            | "tls_enabled"
            | "tls_cert"
            | "tls_key"
            | "tls_self_signed"
            | "storage_path"
            | "s3_endpoint"
            | "s3_bucket"
            | "s3_access_key_id"
            | "s3_secret_access_key"
            | "s3_region"
            | "s3_force_path_style"
            | "cashu_wallet_path"
            | "metrics_token"
            | "serve_files_path"
            | "serve_files_manifest_dir"
    )
}
fn secret(name: &str) -> bool {
    matches!(
        name,
        "s3_access_key_id" | "s3_secret_access_key" | "metrics_token"
    )
}
fn schema() -> Vec<(String, Option<String>)> {
    Config::command()
        .get_arguments()
        .filter_map(|arg| {
            arg.get_long().map(|long| {
                (
                    long.replace('-', "_"),
                    arg.get_default_values()
                        .first()
                        .map(|s| s.to_string_lossy().into_owned()),
                )
            })
        })
        .collect()
}
fn scalar(value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Array(a) => a
            .iter()
            .map(scalar)
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.join(",")),
        _ => Err("Expected a string, boolean, number or list".into()),
    }
}
fn resolved_values(managed: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let cli = cli_values();
    let external = EXTERNAL_ENV.get();
    schema()
        .into_iter()
        .map(|(name, default)| {
            let value = cli
                .get(&name)
                .cloned()
                .or_else(|| external.and_then(|e| e.get(&field_name(&name)).cloned()))
                .map(Value::String)
                .or_else(|| managed.get(&name).cloned())
                .or_else(|| default.map(Value::String))
                .unwrap_or(Value::Null);
            (name, value)
        })
        .collect()
}
fn validate_settings(settings: &BTreeMap<String, Value>) -> Result<(), String> {
    let fields = schema();
    for (name, value) in settings {
        if !fields.iter().any(|(n, _)| n == name) || infrastructure(name) {
            return Err(format!("Setting {name} is not manageable"));
        }
        scalar(value)?;
    }
    let resolved = resolved_values(settings);
    let mut args = vec!["almond".to_owned()];
    for (name, value) in resolved {
        if value.is_null() {
            continue;
        }
        let value = scalar(&value)?;
        if value.is_empty() {
            continue;
        }
        args.push(format!("--{}={value}", name.replace('_', "-")));
    }
    let matches = Config::command()
        .try_get_matches_from(args)
        .map_err(|e| e.to_string())?;
    Config::from_arg_matches(&matches)
        .map_err(|e| e.to_string())?
        .validate()
        .map_err(|e| e.to_string())?;
    Ok(())
}
pub(crate) fn install_managed() -> Result<(), crate::ConfigError> {
    let mut args = std::env::args().skip(1);
    let mut path = std::env::var("ALMOND_MANAGED_CONFIG").ok();
    while let Some(arg) = args.next() {
        if arg == "--managed-config" {
            path = args.next();
        } else if let Some(v) = arg.strip_prefix("--managed-config=") {
            path = Some(v.to_owned());
        }
    }
    let (doc, _) = read_managed(path.as_deref().map(Path::new))
        .map_err(|message| crate::ConfigError { message })?;
    validate_settings(&doc.settings).map_err(|message| crate::ConfigError { message })?;
    for (name, value) in doc.settings {
        if source(&name) == "default" {
            let value = scalar(&value).map_err(|message| crate::ConfigError { message })?;
            if !value.is_empty() {
                std::env::set_var(field_name(&name), value);
            }
        }
    }
    Ok(())
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Managed {
    version: u8,
    #[serde(default)]
    settings: BTreeMap<String, Value>,
    #[serde(default)]
    collaborators: Vec<String>,
}

pub struct AdminState {
    roots: Vec<PublicKey>,
    collaborators: RwLock<Vec<PublicKey>>,
    challenge: String,
    replay: Mutex<HashMap<String, u64>>,
    path: Option<PathBuf>,
    writes: Mutex<()>,
    running: BTreeMap<String, Value>,
    running_managed: BTreeMap<String, Value>,
}
impl AdminState {
    pub fn new(cfg: &Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let managed = read_managed(cfg.managed_config.as_deref())?.0;
        let collaborators = parse_keys(&managed.collaborators)?;
        let running = if PROCESS_ENV.get().is_some() {
            resolved_values(&managed.settings)
        } else {
            serde_json::to_value(cfg)?
                .as_object()
                .expect("Config serializes as object")
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        Ok(Self {
            roots: cfg.admin_npubs.clone(),
            collaborators: RwLock::new(collaborators),
            challenge: uuid::Uuid::new_v4().to_string(),
            replay: Mutex::new(HashMap::new()),
            path: cfg.managed_config.clone(),
            writes: Mutex::new(()),
            running,
            running_managed: managed.settings,
        })
    }
}
fn parse_keys(keys: &[String]) -> Result<Vec<PublicKey>, String> {
    keys.iter()
        .map(|k| PublicKey::parse(k).map_err(|e| e.to_string()))
        .collect()
}
fn read_managed(path: Option<&Path>) -> Result<(Managed, String), String> {
    let bytes = match path {
        None => None,
        Some(path) => match std::fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(format!(
                    "Cannot read managed settings {}: {e}",
                    path.display()
                ))
            }
        },
    };
    let doc = if let Some(bytes) = &bytes {
        let doc: Managed =
            serde_json::from_slice(bytes).map_err(|e| format!("Invalid managed settings: {e}"))?;
        if doc.version != 1 {
            return Err("Unsupported managed settings version".into());
        }
        doc
    } else {
        Managed {
            version: 1,
            ..Default::default()
        }
    };
    Ok((doc, hex::encode(Sha256::digest(bytes.unwrap_or_default()))))
}
fn save(path: &Path, doc: &Managed) -> Result<String, String> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(doc).map_err(|e| e.to_string())?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = parent.join(format!(".almond-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok::<_, std::io::Error>(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("Cannot atomically save managed settings: {e}"));
    }
    Ok(hex::encode(Sha256::digest(&bytes)))
}
fn error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error": message.to_string()})),
    )
        .into_response()
}
fn tag<'a>(event: &'a nostr_sdk::prelude::Event, name: &str) -> Option<&'a str> {
    let mut tags = event.tags.iter().filter(|t| t.kind() == name);
    let value = tags.next()?.content()?;
    if tags.next().is_some() {
        None
    } else {
        Some(value)
    }
}
pub async fn admin_auth(State(state): State<AppState>, req: Request<Body>, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let Some(event) = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| crate::services::auth::parse_auth_header(v).ok())
    else {
        return error(StatusCode::UNAUTHORIZED, "NIP-98 authorization required");
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let created = event.created_at.as_secs();
    let url = format!(
        "{}{}",
        state.public_url.trim_end_matches('/'),
        parts.uri.path_and_query().map_or("/", |p| p.as_str())
    );
    if event.verify().is_err()
        || event.kind != Kind::from(27235)
        || created.abs_diff(now) > 60
        || tag(&event, "u") != Some(url.as_str())
        || tag(&event, "method") != Some(parts.method.as_str())
    {
        return error(StatusCode::UNAUTHORIZED, "Invalid NIP-98 event");
    }
    if state.admin.roots.is_empty()
        || (!state.admin.roots.contains(&event.pubkey)
            && !state
                .admin
                .collaborators
                .read()
                .await
                .contains(&event.pubkey))
    {
        return error(StatusCode::FORBIDDEN, "Administrator key not authorized");
    }
    let bytes = match to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Admin request body too large",
            )
        }
    };
    if !bytes.is_empty()
        && tag(&event, "payload") != Some(hex::encode(Sha256::digest(&bytes)).as_str())
    {
        return error(StatusCode::UNAUTHORIZED, "Payload hash mismatch");
    }
    if parts.method != Method::GET && parts.method != Method::HEAD {
        if tag(&event, "challenge") != Some(state.admin.challenge.as_str()) {
            return error(StatusCode::UNAUTHORIZED, "Invalid restart challenge");
        }
        let mut replay = state.admin.replay.lock().await;
        replay.retain(|_, expiry| *expiry >= now);
        if replay.len() >= 100_000 {
            return error(StatusCode::SERVICE_UNAVAILABLE, "Replay cache full");
        }
        if replay.contains_key(&event.id.to_string()) {
            return error(StatusCode::UNAUTHORIZED, "Replayed event");
        }
        replay.insert(event.id.to_string(), created.saturating_add(60));
    }
    parts.extensions.insert(event.pubkey);
    let mut response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
pub async fn challenge(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"challenge":state.admin.challenge})),
    )
}
pub async fn me(Extension(pubkey): Extension<PublicKey>) -> Json<Value> {
    Json(json!({"pubkey":pubkey.to_string()}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollaboratorChange {
    base_revision: String,
    collaborators: Vec<String>,
}
pub async fn collaborators(State(state): State<AppState>) -> Response {
    match read_managed(state.admin.path.as_deref()) { Ok((doc, revision)) => Json(json!({"revision":revision,"collaborators":doc.collaborators,"writable":state.admin.path.is_some()})).into_response(), Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR,e) }
}
pub async fn update_collaborators(
    State(state): State<AppState>,
    Json(change): Json<CollaboratorChange>,
) -> Response {
    let Some(path) = state.admin.path.as_deref() else {
        return error(StatusCode::CONFLICT, "Managed configuration is disabled");
    };
    if change.collaborators.len() > 1000 {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "Too many collaborators");
    }
    let keys = match parse_keys(&change.collaborators) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    let _guard = state.admin.writes.lock().await;
    let (mut doc, revision) = match read_managed(Some(path)) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if revision != change.base_revision {
        return error(StatusCode::PRECONDITION_FAILED, "Stale revision");
    }
    doc.collaborators = change.collaborators;
    match save(path, &doc) {
        Ok(revision) => {
            *state.admin.collaborators.write().await = keys;
            Json(json!({"revision":revision,"applied":true})).into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsChange {
    base_revision: String,
    #[serde(default)]
    set: BTreeMap<String, Value>,
    #[serde(default)]
    reset: Vec<String>,
}
pub async fn settings(State(state): State<AppState>) -> Response {
    let (doc, revision) = match read_managed(state.admin.path.as_deref()) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let embedded = PROCESS_ENV.get().is_none();
    let saved = if embedded {
        state.admin.running.clone()
    } else {
        resolved_values(&doc.settings)
    };
    let mut fields = serde_json::Map::new();
    for (name, _) in schema() {
        let src = if embedded { "embedded" } else { source(&name) };
        let running = state
            .admin
            .running
            .get(&name)
            .cloned()
            .unwrap_or(Value::Null);
        let next = saved.get(&name).cloned().unwrap_or(Value::Null);
        let managed = doc.settings.get(&name).cloned();
        let running_source = if src == "default" && state.admin.running_managed.contains_key(&name)
        {
            "managed"
        } else {
            src
        };
        let saved_source = if src == "default" && managed.is_some() {
            "managed"
        } else {
            src
        };
        let redact = |v: Value| {
            if secret(&name) {
                json!({"configured":!v.is_null()})
            } else {
                v
            }
        };
        fields.insert(name.clone(), json!({"running":redact(running.clone()),"running_source":running_source,"saved":redact(next.clone()),"saved_source":saved_source,"managed":managed.map(redact),"editable":state.admin.path.is_some() && !infrastructure(&name) && src == "default","reason":if infrastructure(&name) { Some("deployment-owned") } else if src != "default" { Some("set by deployment") } else if state.admin.path.is_none() { Some("managed configuration disabled") } else { None },"restart_required":running != next}));
    }
    Json(json!({"revision":revision,"writable":state.admin.path.is_some() && !embedded,"settings":fields})).into_response()
}
pub async fn update_settings(
    State(state): State<AppState>,
    Json(change): Json<SettingsChange>,
) -> Response {
    let Some(path) = state.admin.path.as_deref() else {
        return error(StatusCode::CONFLICT, "Managed configuration is disabled");
    };
    if PROCESS_ENV.get().is_none() {
        return error(
            StatusCode::CONFLICT,
            "Embedded configuration is deployment-owned",
        );
    }
    let _guard = state.admin.writes.lock().await;
    let (mut doc, revision) = match read_managed(Some(path)) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if revision != change.base_revision {
        return error(StatusCode::PRECONDITION_FAILED, "Stale revision");
    }
    let fields = schema();
    for (name, value) in change.set {
        if !fields.iter().any(|(n, _)| n == &name)
            || infrastructure(&name)
            || source(&name) != "default"
        {
            return error(
                StatusCode::CONFLICT,
                format!("Setting {name} is deployment-owned or unknown"),
            );
        }
        doc.settings.insert(name, value);
    }
    for name in change.reset {
        if !fields.iter().any(|(n, _)| n == &name) || infrastructure(&name) {
            return error(
                StatusCode::CONFLICT,
                format!("Setting {name} is deployment-owned or unknown"),
            );
        }
        doc.settings.remove(&name);
    }
    if let Err(e) = validate_settings(&doc.settings) {
        return error(StatusCode::UNPROCESSABLE_ENTITY, e);
    }
    match save(path, &doc) {
        Ok(revision) => Json(json!({"revision":revision,"applied":false,"restart_required":true}))
            .into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_file_revisions_and_corruption() {
        let dir = std::env::temp_dir().join(format!("almond-admin-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("settings.json");
        let (mut doc, empty) = read_managed(Some(&path)).unwrap();
        doc.settings.insert("upload_max_age".into(), json!("14d"));
        let revision = save(&path, &doc).unwrap();
        assert_ne!(revision, empty);
        assert_eq!(
            read_managed(Some(&path)).unwrap().0.settings["upload_max_age"],
            "14d"
        );
        std::fs::write(&path, b"").unwrap();
        assert!(read_managed(Some(&path)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
