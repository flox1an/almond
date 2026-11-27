use crate::{
    error::AppError,
    models::{AppState, FileLocation},
    services::file_storage::{self, Removal},
};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::Response,
    Extension, Json,
};
use nostr_sdk::prelude::PublicKey;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

static ACTIVITY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const ACTIVITY_LIMIT: usize = 10_000;

#[derive(Deserialize, Default)]
pub struct Page {
    cursor: Option<String>,
    limit: Option<usize>,
    search: Option<String>,
    mime_type: Option<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    created_after: Option<u64>,
    created_before: Option<u64>,
    origin: Option<String>,
    backend: Option<String>,
    expiration: Option<String>,
    moderation: Option<String>,
}
#[derive(Deserialize)]
pub struct Action {
    action: String,
    hashes: Vec<String>,
    reason: String,
    #[serde(default)]
    report_ids: Vec<String>,
}

async fn write_record(path: &std::path::Path, value: &Value) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(value).map_err(|e| AppError::InternalError(e.to_string()))?;
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .await
        .map_err(|e| AppError::IoError(e.to_string()))?;
    let result = async {
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        tokio::fs::hard_link(&temp, path).await?;
        tokio::fs::remove_file(&temp).await?;
        tokio::fs::File::open(path.parent().unwrap())
            .await?
            .sync_all()
            .await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result.map_err(|e: std::io::Error| AppError::IoError(e.to_string()))
}

pub async fn actions(
    State(state): State<AppState>,
    Extension(actor): Extension<PublicKey>,
    Json(input): Json<Action>,
) -> Result<Json<Value>, AppError> {
    if input.hashes.is_empty()
        || input.hashes.len() > 100
        || input.reason.trim().is_empty()
        || input.reason.len() > 2048
        || input.report_ids.len() > 100
    {
        return Err(AppError::BadRequest(
            "Supply 1–100 hashes and a reason of at most 2048 bytes".into(),
        ));
    }
    if ![
        "delete",
        "quarantine",
        "restore",
        "block",
        "unblock",
        "dismiss",
    ]
    .contains(&input.action.as_str())
    {
        return Err(AppError::BadRequest("Unknown action".into()));
    }
    for hash in input.hashes.iter().chain(input.report_ids.iter()) {
        file_storage::validate_sha256_format(hash)?;
    }
    for report in &input.report_ids {
        let bytes = tokio::fs::read(state.storage.reports.join(format!("{report}.json")))
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    AppError::NotFound("Report not found".into())
                } else {
                    AppError::IoError(e.to_string())
                }
            })?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|e| AppError::IoError(e.to_string()))?;
        if !value["requested"].as_array().is_some_and(|hashes| {
            hashes.iter().any(|hash| {
                input
                    .hashes
                    .iter()
                    .any(|h| hash.as_str() == Some(h.as_str()))
            })
        }) {
            return Err(AppError::BadRequest(
                "Report does not reference a selected hash".into(),
            ));
        }
    }
    if input.action == "dismiss" && input.report_ids.is_empty() {
        return Err(AppError::BadRequest("Dismiss requires report IDs".into()));
    }
    let activity = state.storage.root.join("admin-activity");
    tokio::fs::create_dir_all(&activity)
        .await
        .map_err(|e| AppError::IoError(e.to_string()))?;
    let _activity_guard = ACTIVITY_LOCK.lock().await;
    let entries = records(&activity).await?;
    let mut completed: Vec<_> = entries
        .iter()
        .filter(|v| {
            v["status"] == "completed" || v["status"] == "failed" || v["status"] == "reconciled"
        })
        .filter_map(|v| v["id"].as_str())
        .filter_map(|id| {
            entries
                .iter()
                .find(|v| v["id"] == id && v["status"] == "pending")
                .map(|intent| (intent["time"].as_i64().unwrap_or(0), id.to_owned()))
        })
        .collect();
    completed.sort();
    let intents = entries.iter().filter(|v| v["status"] == "pending").count();
    let remove = intents
        .saturating_add(input.hashes.len())
        .saturating_sub(ACTIVITY_LIMIT);
    if remove > completed.len() {
        return Err(AppError::ServiceUnavailable(
            "Activity retention full of unresolved actions; reconcile before continuing".into(),
        ));
    }
    for (_, id) in completed.into_iter().take(remove) {
        tokio::fs::remove_file(activity.join(format!("{id}.outcome.json")))
            .await
            .map_err(|e| AppError::IoError(e.to_string()))?;
        tokio::fs::remove_file(activity.join(format!("{id}.intent.json")))
            .await
            .map_err(|e| AppError::IoError(e.to_string()))?;
    }
    let mut results = Vec::new();
    for hash in &input.hashes {
        let id = uuid::Uuid::new_v4().to_string();
        let intent = json!({"id":id,"actor":actor.to_hex(),"action":input.action,"hash":hash,"reason":input.reason,"status":"pending","time":chrono::Utc::now().timestamp()});
        if let Err(error) = write_record(&activity.join(format!("{id}.intent.json")), &intent).await
        {
            results.push(json!({"id":id,"hash":hash,"ok":false,"status":"not_started","message":format!("Intent could not be recorded: {error}")}));
            break;
        }
        let result: Result<String, AppError> = match input.action.as_str() {
            "delete" => file_storage::remove_indexed_blob(&state, hash, Removal::Reported, None)
                .await
                .map(|removed| if removed { "Deleted" } else { "Already absent" }.to_string()),
            "quarantine" => {
                file_storage::remove_indexed_blob(&state, hash, Removal::Quarantined, None)
                    .await
                    .and_then(|removed| {
                        if removed {
                            Ok("Quarantined".into())
                        } else {
                            Err(AppError::NotFound("Blob not indexed".into()))
                        }
                    })
            }
            "restore" => file_storage::restore_quarantined(&state, hash)
                .await
                .map(|()| "Restored".into()),
            "block" => {
                let _guard = state.blob_mutation_locks.lock(hash).await;
                let dir = state.storage.root.join("admin-blocks");
                match tokio::fs::create_dir_all(&dir).await {
                    Err(e) => Err(AppError::IoError(e.to_string())),
                    Ok(()) => match write_record(&dir.join(hash), &intent).await {
                        Ok(()) => Ok("Blocked".into()),
                        Err(AppError::IoError(e)) if e.contains("exists") => {
                            Ok("Already blocked".into())
                        }
                        Err(e) => Err(e),
                    },
                }
            }
            "unblock" => {
                let _guard = state.blob_mutation_locks.lock(hash).await;
                match tokio::fs::remove_file(state.storage.root.join("admin-blocks").join(hash))
                    .await
                {
                    Ok(()) => Ok("Unblocked".into()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Ok("Already unblocked".into())
                    }
                    Err(e) => Err(AppError::IoError(e.to_string())),
                }
            }
            _ => Ok("Dismissed".into()),
        };
        let (ok, message) = match result {
            Ok(message) => (true, message),
            Err(e) => (false, e.to_string()),
        };
        let review_result: Result<(), AppError> = async {
        if ok && !input.report_ids.is_empty() {
            let dir = state.storage.root.join("admin-reviews");
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| AppError::IoError(e.to_string()))?;
            for report in &input.report_ids {
                let original =
                    tokio::fs::read(state.storage.reports.join(format!("{report}.json")))
                        .await
                        .map_err(|e| AppError::IoError(e.to_string()))?;
                let value: Value = serde_json::from_slice(&original)
                    .map_err(|e| AppError::IoError(e.to_string()))?;
                if !value["requested"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|v| v.as_str() == Some(hash)))
                {
                    continue;
                }
                write_record(&dir.join(format!("{report}-{id}.json")), &json!({"report_id":report,"hash":hash,"actor":actor.to_hex(),"action":input.action,"reason":input.reason,"time":chrono::Utc::now().timestamp()})).await?;
            }
        }
        Ok(())
        }.await;
        if let Err(error) = review_result {
            results.push(json!({"id":id,"hash":hash,"ok":false,"status":"uncertain","message":format!("Action ran but review recording failed: {error}")}));
            break;
        }
        let outcome =
            json!({"id":id,"status":if ok {"completed"} else {"failed"},"message":message});
        if let Err(error) =
            write_record(&activity.join(format!("{id}.outcome.json")), &outcome).await
        {
            results.push(json!({"id":id,"hash":hash,"ok":false,"status":"uncertain","message":format!("Action outcome could not be recorded: {error}")}));
            break;
        }
        results.push(json!({"id":id,"hash":hash,"ok":ok,"status":if ok {"completed"} else {"failed"},"message":message}));
    }
    for hash in input.hashes.iter().skip(results.len()) {
        results.push(json!({"hash":hash,"ok":false,"status":"not_started","message":"Earlier target could not be safely recorded"}));
    }
    Ok(Json(json!({"results":results})))
}

async fn records(dir: &std::path::Path) -> Result<Vec<Value>, AppError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(AppError::IoError(e.to_string())),
    };
    let mut values = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| AppError::IoError(e.to_string()))?
    {
        if entry.file_name().to_string_lossy().contains(".tmp-") {
            continue;
        }
        if !entry
            .file_type()
            .await
            .map_err(|e| AppError::IoError(e.to_string()))?
            .is_file()
        {
            continue;
        }
        let data = tokio::fs::read(entry.path())
            .await
            .map_err(|e| AppError::IoError(e.to_string()))?;
        let mut value: Value = serde_json::from_slice(&data).map_err(|e| {
            AppError::IoError(format!("Invalid record {}: {e}", entry.path().display()))
        })?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("_file".into(), json!(entry.file_name().to_string_lossy()));
        }
        values.push(value);
    }
    values.sort_by(|a, b| a["_file"].as_str().cmp(&b["_file"].as_str()));
    Ok(values)
}
pub async fn activity(
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, AppError> {
    let entries = records(&state.storage.root.join("admin-activity")).await?;
    let mut items: Vec<_> = entries
        .iter()
        .filter(|v| v["status"] == "pending")
        .map(|intent| {
            let mut item = intent.clone();
            let outcome = entries
                .iter()
                .find(|v| v["id"] == intent["id"] && v["status"] != "pending");
            item["outcome"] = outcome
                .cloned()
                .unwrap_or_else(|| json!({"status":"uncertain"}));
            item
        })
        .collect();
    items.sort_by(|a, b| a["_file"].as_str().cmp(&b["_file"].as_str()));
    let limit = page.limit.unwrap_or(50).clamp(1, 100);
    let selected: Vec<_> = items
        .into_iter()
        .filter(|v| {
            page.cursor
                .as_ref()
                .is_none_or(|c| v["_file"].as_str().is_some_and(|f| f > c.as_str()))
        })
        .take(limit + 1)
        .collect();
    let next = if selected.len() > limit {
        selected[limit - 1]["_file"].as_str().map(str::to_owned)
    } else {
        None
    };
    Ok(Json(
        json!({"items":selected.into_iter().take(limit).collect::<Vec<_>>(),"next_cursor":next}),
    ))
}
#[derive(Deserialize)]
pub struct Reconciliation {
    reason: String,
}

pub async fn reconcile_activity(
    State(state): State<AppState>,
    Extension(actor): Extension<PublicKey>,
    Path(id): Path<String>,
    Json(input): Json<Reconciliation>,
) -> Result<Json<Value>, AppError> {
    let id = uuid::Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid activity id".into()))?
        .to_string();
    if input.reason.trim().is_empty() || input.reason.len() > 2048 {
        return Err(AppError::BadRequest(
            "Supply observations of at most 2048 bytes".into(),
        ));
    }
    let _guard = ACTIVITY_LOCK.lock().await;
    let dir = state.storage.root.join("admin-activity");
    let path = dir.join(format!("{id}.intent.json"));
    let bytes = tokio::fs::read(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound("Activity not found".into())
        } else {
            AppError::IoError(e.to_string())
        }
    })?;
    let intent: Value =
        serde_json::from_slice(&bytes).map_err(|e| AppError::IoError(e.to_string()))?;
    if intent["id"] != id || intent["status"] != "pending" {
        return Err(AppError::Conflict("Invalid activity intent".into()));
    }
    let outcome_path = dir.join(format!("{id}.outcome.json"));
    if tokio::fs::try_exists(&outcome_path)
        .await
        .map_err(|e| AppError::IoError(e.to_string()))?
    {
        return Err(AppError::Conflict("Activity already has an outcome".into()));
    }
    let outcome = json!({"id":id,"status":"reconciled","actor":actor.to_hex(),"reason":input.reason,"time":chrono::Utc::now().timestamp()});
    write_record(&outcome_path, &outcome).await?;
    Ok(Json(outcome))
}

pub async fn blocks(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    Ok(Json(json!(
        records(&state.storage.root.join("admin-blocks")).await?
    )))
}

pub async fn blobs(
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, AppError> {
    let blocks = records(&state.storage.root.join("admin-blocks")).await?;
    let reports = records(&state.storage.reports).await?;
    let mut entries = state.file_index.snapshot().await;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let limit = page.limit.unwrap_or(50).clamp(1, 100);
    let filtered: Vec<_> = entries
        .into_iter()
        .filter(|(hash, meta)| {
            page.cursor.as_ref().is_none_or(|c| hash > c)
                && page.search.as_ref().is_none_or(|s| {
                    hash.contains(s) || meta.pubkey.as_ref().is_some_and(|p| p.to_hex().contains(s))
                })
                && page
                    .mime_type
                    .as_ref()
                    .is_none_or(|s| meta.mime_type.as_ref().is_some_and(|m| m.starts_with(s)))
                && page.min_size.is_none_or(|n| meta.size >= n)
                && page.max_size.is_none_or(|n| meta.size <= n)
                && page.created_after.is_none_or(|n| meta.created_at >= n)
                && page.created_before.is_none_or(|n| meta.created_at <= n)
                && page.origin.as_ref().is_none_or(|s| match s.as_str() {
                    "upload" => meta.origin == crate::models::BlobOrigin::Upload,
                    "upstream" => meta.origin == crate::models::BlobOrigin::UpstreamCache,
                    _ => false,
                })
                && page.backend.as_ref().is_none_or(|s| {
                    matches!(
                        (s.as_str(), &meta.location),
                        ("local", FileLocation::Local(_)) | ("s3", FileLocation::S3 { .. })
                    )
                })
                && page.expiration.as_ref().is_none_or(|s| match s.as_str() {
                    "set" => meta.expiration.is_some(),
                    "none" => meta.expiration.is_none(),
                    "expired" => meta
                        .expiration
                        .is_some_and(|t| t <= chrono::Utc::now().timestamp().max(0) as u64),
                    _ => false,
                })
                && page.moderation.as_ref().is_none_or(|s| {
                    let blocked = blocks
                        .iter()
                        .any(|b| b["_file"].as_str() == Some(hash.as_str()));
                    let reported = reports.iter().any(|r| {
                        r["requested"]
                            .as_array()
                            .is_some_and(|v| v.iter().any(|h| h.as_str() == Some(hash.as_str())))
                    });
                    match s.as_str() {
                        "blocked" => blocked,
                        "reported" => reported,
                        "clear" => !blocked && !reported,
                        _ => false,
                    }
                })
        })
        .take(limit + 1)
        .collect();
    let next = if filtered.len() > limit {
        Some(filtered[limit - 1].0.clone())
    } else {
        None
    };
    let items: Vec<_> = filtered
        .into_iter()
        .take(limit)
        .map(|(hash, metadata)| json!({"hash":hash,"metadata":&*metadata}))
        .collect();
    Ok(Json(json!({"items":items,"next_cursor":next})))
}
pub async fn blob(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    file_storage::validate_sha256_format(&hash)?;
    let metadata = state
        .file_index
        .get(&hash)
        .await
        .ok_or_else(|| AppError::NotFound("Blob not found".into()))?;
    let reports = records(&state.storage.reports)
        .await?
        .into_iter()
        .filter(|r| {
            r["requested"]
                .as_array()
                .is_some_and(|v| v.iter().any(|h| h.as_str() == Some(hash.as_str())))
        })
        .collect::<Vec<_>>();
    let blocked = tokio::fs::try_exists(state.storage.root.join("admin-blocks").join(&hash))
        .await
        .map_err(|e| AppError::IoError(e.to_string()))?;
    Ok(Json(
        json!({"hash":hash,"metadata":&*metadata,"reports":reports,"blocked":blocked}),
    ))
}
pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let stats = state.file_index.stats().await;
    let reports = records(&state.storage.reports).await?;
    let reviews = records(&state.storage.root.join("admin-reviews")).await?;
    let outstanding = reports
        .iter()
        .flat_map(|r| {
            r["requested"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(move |hash| (r, hash))
        })
        .filter(|(r, hash)| {
            !reviews
                .iter()
                .any(|v| v["report_id"] == r["event"]["id"] && v["hash"].as_str() == Some(*hash))
        })
        .count();
    Ok(Json(
        json!({"blob_count":stats.count,"total_bytes":stats.total_bytes,"max_total_size":state.max_total_size,"max_total_files":state.max_total_files,"outstanding_reports":outstanding,"s3":state.native_s3.as_ref().map(|s| format!("{:?}",s.status()))}),
    ))
}
pub async fn reports(
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, AppError> {
    let mut items = records(&state.storage.reports).await?;
    let reviews = records(&state.storage.root.join("admin-reviews")).await?;
    for item in &mut items {
        let id = item["event"]["id"].as_str().unwrap_or("").to_owned();
        item["reviews"] = json!(reviews
            .iter()
            .filter(|v| v["report_id"].as_str() == Some(id.as_str()))
            .collect::<Vec<_>>());
    }
    let mut groups = std::collections::BTreeMap::<String, Vec<Value>>::new();
    for item in items {
        if let Some(hashes) = item["requested"].as_array() {
            for hash in hashes.iter().filter_map(Value::as_str) {
                groups
                    .entry(hash.to_owned())
                    .or_default()
                    .push(item.clone());
            }
        }
    }
    let limit = page.limit.unwrap_or(50).clamp(1, 100);
    let filtered: Vec<_> = groups
        .into_iter()
        .filter(|(hash, _)| {
            page.cursor.as_ref().is_none_or(|c| hash > c)
                && page.search.as_ref().is_none_or(|s| hash.contains(s))
        })
        .take(limit + 1)
        .collect();
    let next = if filtered.len() > limit {
        Some(filtered[limit - 1].0.clone())
    } else {
        None
    };
    let items: Vec<_> = filtered
        .into_iter()
        .take(limit)
        .map(|(hash, reports)| json!({"hash":hash,"reports":reports}))
        .collect();
    Ok(Json(json!({"items":items,"next_cursor":next})))
}
pub async fn preview(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response<Body>, AppError> {
    file_storage::validate_sha256_format(&hash)?;
    let metadata = state
        .file_index
        .get(&hash)
        .await
        .ok_or_else(|| AppError::NotFound("Blob not found".into()))?;
    let body = match &metadata.location {
        FileLocation::Local(path) => {
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|e| AppError::IoError(e.to_string()))?;
            Body::from_stream(tokio_util::io::ReaderStream::new(file))
        }
        FileLocation::S3 { key } => {
            let s3 = state
                .native_s3
                .as_ref()
                .ok_or_else(|| AppError::ServiceUnavailable("S3 unavailable".into()))?;
            Body::from_stream(tokio_util::io::ReaderStream::new(
                s3.get(key, None).await?.body.into_async_read(),
            ))
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, "attachment")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(body)
        .map_err(|e| AppError::InternalError(e.to_string()))
}
