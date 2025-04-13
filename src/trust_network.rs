use std::collections::HashMap;
use std::time::Duration;

use nostr_relay_pool::{
    prelude::*,
    relay::limits::{RelayEventLimits, RelayMessageLimits},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// Hardcoded seed relays for trust network refresh.
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

/// Create and connect to a relay pool
pub async fn create_pool() -> Result<RelayPool> {
    let pool = RelayPool::new();

    let relay_options = RelayOptions::default().limits(RelayLimits {
        messages: RelayMessageLimits::default(),
        events: RelayEventLimits {
            max_size: Some(500 * 1024), // 500KB event size
            ..Default::default()
        },
    });

    for seed_relay in SEED_RELAYS.iter().copied() {
        pool.add_relay(seed_relay, relay_options.clone()).await?;
    }

    pool.connect().await;
    Ok(pool)
}

/// Fetch followers for multiple pubkeys and return results grouped by pubkey
async fn get_followers(
    pool: &RelayPool,
    pubkeys: &[PublicKey],
) -> Result<HashMap<PublicKey, Vec<String>>> {
    let mut followers_by_pubkey: HashMap<PublicKey, Vec<String>> = HashMap::new();

    let filter = Filter::new()
        .kinds([Kind::ContactList])
        .authors(pubkeys.iter().copied())
        .limit(300);
    let timeout = Duration::from_secs(10);
    let events = pool
        .fetch_events(filter, timeout, ReqExitPolicy::default())
        .await?;
    println!("💻 Received {} contact list events", events.len());

    for event in events.iter() {
        let author = event.pubkey;
        let mut followers = Vec::new();
        let tags = event.tags.filter(TagKind::p());
        for tag in tags {
            if let Some(content) = tag.content() {
                followers.push(content.to_string());
            }
        }
        // Remove duplicates for this event
        followers.sort();
        followers.dedup();

        // Aggregate with existing followers for this author
        let existing_followers = followers_by_pubkey.entry(author).or_insert_with(Vec::new);
        existing_followers.extend(followers);
    }

    // Final deduplication per author
    for followers in followers_by_pubkey.values_mut() {
        followers.sort();
        followers.dedup();
    }

    Ok(followers_by_pubkey)
}

/// Simple trust network refresh logic (a pared‑down version of refreshTrustNetwork).
pub async fn refresh_trust_network(
    owner_pubkeys: &[PublicKey],
) -> Result<HashMap<PublicKey, usize>> {
    // Create and connect to the relay pool
    let pool = create_pool().await?;

    // Use local mutable state.
    let mut pubkey_follower_count: HashMap<String, usize> = HashMap::new();

    // Phase 1: Fetch owner's follows
    println!("🔍 Fetching owner's follows");
    let one_hop_network = get_followers(&pool, owner_pubkeys).await?;
    let empty_vec = Vec::new();
    let followers = one_hop_network.get(&owner_pubkeys[0]).unwrap_or(&empty_vec);
    println!("✋ Found {} one-hop connections", followers.len());

    // Phase 2: Query follows from one-hop network in batches
    println!(
        "🌐 Building web of trust graph from {} one-hop keys",
        followers.len()
    );
    for chunk in followers.chunks(50) {
        let pubkeys: Vec<PublicKey> = chunk
            .iter()
            .filter_map(|pk_str| PublicKey::parse(pk_str).ok())
            .collect();

        if let Ok(followers_by_pubkey) = get_followers(&pool, &pubkeys).await {
            for (_, followers) in followers_by_pubkey {
                for follower in followers {
                    *pubkey_follower_count.entry(follower).or_insert(0) += 1;
                }
            }
        }
    }

    println!(
        "📡 Total network size (unique pubkeys): {}",
        pubkey_follower_count.len()
    );

    // Filter pubkeys with more than 3 followers and convert to PublicKey
    let trusted_pubkeys: HashMap<PublicKey, usize> = pubkey_follower_count
        .into_iter()
        .filter(|(_, count)| *count > 3)
        .filter_map(|(pk_str, count)| PublicKey::from_hex(&pk_str).ok().map(|pk| (pk, count)))
        .collect();

    println!(
        "🫂 Total number of trusted pubkeys: {}",
        trusted_pubkeys.len()
    );

    pool.disconnect().await;
    Ok(trusted_pubkeys)
}
