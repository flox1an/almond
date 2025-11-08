use std::time::Duration;

use nostr_relay_pool::{
    prelude::*,
    relay::limits::{RelayEventLimits, RelayMessageLimits},
};
use tracing::{info, warn};

use crate::models::AppState;

// Hardcoded seed relays for fetching user server lists
static SEED_RELAYS: &[&str] = &[
    "wss://nos.lol",
    "wss://nostr.mom",
    "wss://purplepag.es",
    "wss://purplerelay.com",
    "wss://relay.damus.io",
    "wss://relay.nostr.band",
    "wss://relay.snort.social",
    "wss://relay.primal.net",
    "wss://no.str.cr",
    "wss://nostr21.com",
    "wss://nostrue.com",
];

/// Parse pubkey from string (supports both npub and hex formats)
pub fn parse_pubkey(pubkey_str: &str) -> Result<PublicKey, String> {
    let pubkey_str = pubkey_str.trim();

    // Try parsing as npub first
    if let Ok(pubkey) = PublicKey::from_bech32(pubkey_str) {
        return Ok(pubkey);
    }

    // Try parsing as hex
    if let Ok(pubkey) = PublicKey::from_hex(pubkey_str) {
        return Ok(pubkey);
    }

    Err(format!("Invalid pubkey format: {}", pubkey_str))
}

/// Create and connect to a relay pool for fetching server lists
async fn create_pool() -> Result<RelayPool, Box<dyn std::error::Error + Send + Sync>> {
    let pool = RelayPool::new();

    let relay_options = RelayOptions::default().limits(RelayLimits {
        messages: RelayMessageLimits::default(),
        events: RelayEventLimits {
            max_size: Some(500 * 1024), // 500KB event size
            ..Default::default()
        },
    });

    for seed_relay in SEED_RELAYS.iter().copied() {
        if let Err(e) = pool.add_relay(seed_relay, relay_options.clone()).await {
            warn!("Failed to add relay {}: {}", seed_relay, e);
        }
    }

    pool.connect().await;
    Ok(pool)
}

/// Fetch user's blossom server list from Nostr (kind 10063) with caching
/// Returns a list of server URLs in order of preference (most reliable first)
/// Uses cache if available and not expired, otherwise fetches from Nostr
pub async fn fetch_user_server_list(
    state: &AppState,
    pubkey: &PublicKey,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let cache_ttl_hours = state.blossom_server_list_cache_ttl_hours;
    let cache_ttl_duration = Duration::from_secs(cache_ttl_hours * 3600);

    // Check cache first
    {
        let cache = state.blossom_server_lists.read().await;
        if let Some((servers, cached_at)) = cache.get(pubkey) {
            let age = std::time::Instant::now().duration_since(*cached_at);
            if age < cache_ttl_duration {
                info!(
                    "Using cached server list for pubkey: {} (age: {:?}, TTL: {}h)",
                    pubkey.to_hex(),
                    age,
                    cache_ttl_hours
                );
                return Ok(servers.clone());
            } else {
                info!(
                    "Cache expired for pubkey: {} (age: {:?}, TTL: {}h), fetching from Nostr",
                    pubkey.to_hex(),
                    age,
                    cache_ttl_hours
                );
            }
        }
    }

    // Cache miss or expired - fetch from Nostr
    info!("Fetching server list from Nostr for pubkey: {}", pubkey.to_hex());

    let pool = create_pool().await?;

    // Query for kind 10063 events (User Server List according to BUD-03)
    let filter = Filter::new()
        .kinds([Kind::Custom(10063)])
        .authors([*pubkey])
        .limit(1); // Only need the most recent event

    let timeout = Duration::from_secs(10);
    let events = pool
        .fetch_events(filter, timeout, ReqExitPolicy::default())
        .await?;

    pool.disconnect().await;

    // Get the most recent event (should only be one due to limit=1)
    let servers = match events.iter().next() {
        Some(event) => {
            info!("Found server list event for pubkey: {} (id: {})", pubkey.to_hex(), event.id);

            // Extract server tags from the event
            // Server tags have the format: ["server", "https://cdn.example.com"]
            let server_tags: Vec<_> = event
                .tags
                .iter()
                .filter(|tag| tag.kind() == TagKind::Custom("server".into()))
                .collect();

            let mut servers = Vec::new();

            for tag in server_tags {
                if let Some(server_url) = tag.content() {
                    // Validate that it's a proper URL (starts with http:// or https://)
                    if server_url.starts_with("http://") || server_url.starts_with("https://") {
                        let server_url_str = server_url.to_string();
                        info!("Found server in user list: {}", server_url_str);
                        servers.push(server_url_str);
                    } else {
                        warn!("Invalid server URL (missing http:// or https://): {}", server_url);
                    }
                }
            }

            servers
        }
        None => {
            info!("No server list found for pubkey: {}", pubkey.to_hex());
            Vec::new()
        }
    };

    info!("Fetched {} servers for pubkey: {}", servers.len(), pubkey.to_hex());

    // Update cache (even if empty, to avoid repeated queries)
    {
        let mut cache = state.blossom_server_lists.write().await;
        cache.insert(*pubkey, (servers.clone(), std::time::Instant::now()));
        info!(
            "Cached server list for pubkey: {} ({} servers, TTL: {}h)",
            pubkey.to_hex(),
            servers.len(),
            cache_ttl_hours
        );
    }

    Ok(servers)
}

