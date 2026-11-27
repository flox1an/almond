use base64::{engine::general_purpose::STANDARD, Engine};
use nostr_sdk::prelude::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    process::{Child, Command},
    time::Duration,
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

async fn start(root: &Path, origin: &str, keys: &Keys) -> Server {
    let address = origin.strip_prefix("http://").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_almond"))
        .env_clear()
        .env("ALMOND_STORAGE_MAX_FILES", "22")
        .arg("--config")
        .arg(root.join("almond.env"))
        .arg("--managed-config")
        .arg(root.join("managed.json"))
        .arg("--storage-path")
        .arg(root.join("storage"))
        .arg("--admin-npubs")
        .arg(keys.public_key().to_bech32().unwrap())
        .arg("--bind-addr")
        .arg(address)
        .arg("--public-url")
        .arg(origin)
        .arg("--blob-max-size")
        .arg("400MiB")
        .spawn()
        .unwrap();
    let mut server = Server(child);
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("{origin}/admin/api/challenge"))
            .send()
            .await
            .is_ok()
        {
            return server;
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited during startup"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server did not start");
}

async fn request(
    client: &reqwest::Client,
    origin: &str,
    keys: &Keys,
    method: &str,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let path = "/admin/api/settings";
    let bytes = body
        .map(|v| serde_json::to_vec(&v).unwrap())
        .unwrap_or_default();
    let url = format!("{origin}{path}");
    let mut tags = vec![
        Tag::custom("u", [url.as_str()]),
        Tag::custom("method", [method]),
        Tag::custom("nonce", [uuid::Uuid::new_v4().to_string()]),
    ];
    if !bytes.is_empty() {
        let challenge: Value = client
            .get(format!("{origin}/admin/api/challenge"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        tags.push(Tag::custom(
            "challenge",
            [challenge["challenge"].as_str().unwrap()],
        ));
        tags.push(Tag::custom(
            "payload",
            [hex::encode(Sha256::digest(&bytes))],
        ));
    }
    let event = EventBuilder::new(Kind::Custom(27235), "")
        .tags(tags)
        .finalize(keys)
        .unwrap();
    let response = client
        .request(method.parse().unwrap(), url)
        .header(
            "Authorization",
            format!(
                "Nostr {}",
                STANDARD.encode(serde_json::to_vec(&event).unwrap())
            ),
        )
        .header("Content-Type", "application/json")
        .body(bytes)
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

#[tokio::test]
async fn managed_precedence_revision_writes_and_restart() {
    let root = std::env::temp_dir().join(format!("almond-managed-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("almond.env"),
        "ALMOND_STORAGE_MAX_FILES=33\nALMOND_UPLOAD_MAX_AGE=3d\n",
    )
    .unwrap();
    let managed = root.join("managed.json");
    std::fs::write(&managed, serde_json::to_vec(&json!({"version":1,"settings":{
        "blob_max_size":"300MiB", "storage_max_files":11, "upload_max_age":"1d", "upstream_cache_ttl":"2d"
    },"collaborators":[]})).unwrap()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let keys = Keys::generate();
    let client = reqwest::Client::new();
    let server = start(&root, &origin, &keys).await;
    let (status, initial) = request(&client, &origin, &keys, "GET", None).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let fields = &initial["settings"];
    for (name, source, value) in [
        ("blob_max_size", "cli", "400MiB"),
        ("storage_max_files", "environment", "22"),
        ("upload_max_age", "dotenv", "3d"),
        ("upstream_cache_ttl", "managed", "2d"),
    ] {
        assert_eq!(fields[name]["running_source"], source, "{name}");
        assert_eq!(fields[name]["running"], value, "{name}");
    }
    assert_eq!(fields["storage_max_files"]["managed"], 11);
    assert_eq!(fields["storage_max_files"]["editable"], false);
    let revision = initial["revision"].as_str().unwrap();
    let original = std::fs::read(&managed).unwrap();
    let (status, _) = request(
        &client,
        &origin,
        &keys,
        "PATCH",
        Some(json!({"base_revision":"stale","set":{"upstream_cache_ttl":"4d"}})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::PRECONDITION_FAILED);
    assert_eq!(std::fs::read(&managed).unwrap(), original);
    let (status, _) = request(
        &client,
        &origin,
        &keys,
        "PATCH",
        Some(json!({"base_revision":revision,"set":{"storage_max_files":99}})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT);
    assert_eq!(std::fs::read(&managed).unwrap(), original);
    let (status, _) = request(
        &client,
        &origin,
        &keys,
        "PATCH",
        Some(json!({"base_revision":revision,"set":{"upstream_cache_ttl":"not-a-duration"}})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(std::fs::read(&managed).unwrap(), original);
    let (status, saved) = request(&client, &origin, &keys, "PATCH", Some(json!({"base_revision":revision,"set":{"upstream_cache_ttl":"4d"},"reset":["storage_max_files"]}))).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(saved["applied"], false);
    assert_eq!(saved["restart_required"], true);
    let (status, pending) = request(&client, &origin, &keys, "GET", None).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(pending["revision"], saved["revision"]);
    assert_eq!(pending["settings"]["upstream_cache_ttl"]["running"], "2d");
    assert_eq!(pending["settings"]["upstream_cache_ttl"]["saved"], "4d");
    assert_eq!(
        pending["settings"]["upstream_cache_ttl"]["restart_required"],
        true
    );
    assert!(pending["settings"]["storage_max_files"]["managed"].is_null());
    let persisted: Value = serde_json::from_slice(&std::fs::read(&managed).unwrap()).unwrap();
    assert_eq!(persisted["settings"]["upstream_cache_ttl"], "4d");
    assert!(persisted["settings"].get("storage_max_files").is_none());
    let before_stale = std::fs::read(&managed).unwrap();
    let (status, _) = request(
        &client,
        &origin,
        &keys,
        "PATCH",
        Some(json!({"base_revision":revision,"reset":["upstream_cache_ttl"]})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::PRECONDITION_FAILED);
    assert_eq!(std::fs::read(&managed).unwrap(), before_stale);
    drop(server);
    let server = start(&root, &origin, &keys).await;
    let (status, restarted) = request(&client, &origin, &keys, "GET", None).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(restarted["settings"]["upstream_cache_ttl"]["running"], "4d");
    assert_eq!(
        restarted["settings"]["upstream_cache_ttl"]["running_source"],
        "managed"
    );
    assert_eq!(
        restarted["settings"]["upstream_cache_ttl"]["restart_required"],
        false
    );
    assert_eq!(restarted["settings"]["storage_max_files"]["running"], "22");
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
