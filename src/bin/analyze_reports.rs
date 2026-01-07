use std::collections::HashMap;
use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
struct NostrBandResponse {
    events: Vec<NostrEvent>,
}

#[derive(Debug, Deserialize, Serialize)]
struct NostrEvent {
    id: String,
    pubkey: String,
    created_at: i64,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: String,
}

#[derive(Debug, Default)]
struct ReportStats {
    total_events: usize,
    unique_reporters: usize,
    unique_reported_pubkeys: usize,
    unique_reported_events: usize,
    label_counts: HashMap<String, usize>,
    reporter_counts: HashMap<String, usize>,
    reported_pubkey_counts: HashMap<String, usize>,
    reported_event_counts: HashMap<String, usize>,
    kind_counts: HashMap<u32, usize>,
}

fn analyze_events(events: &[NostrEvent]) -> ReportStats {
    let mut stats = ReportStats::default();
    stats.total_events = events.len();

    for event in events.iter() {
        // Count by kind
        *stats.kind_counts.entry(event.kind).or_insert(0) += 1;

        let reporter = &event.pubkey;
        *stats.reporter_counts.entry(reporter.clone()).or_insert(0) += 1;

        for tag in &event.tags {
            if tag.is_empty() {
                continue;
            }

            match tag[0].as_str() {
                "L" => {
                    // Label namespace
                    if tag.len() > 1 {
                        *stats.label_counts.entry(format!("L:{}", tag[1])).or_insert(0) += 1;
                    }
                }
                "l" => {
                    // Label value
                    if tag.len() > 1 {
                        let namespace = tag.get(2).map(|s| s.as_str()).unwrap_or("unknown");
                        *stats.label_counts.entry(format!("l:{}:{}", namespace, tag[1])).or_insert(0) += 1;
                    }
                }
                "p" => {
                    // Reported pubkey
                    if tag.len() > 1 {
                        *stats.reported_pubkey_counts.entry(tag[1].clone()).or_insert(0) += 1;
                    }
                }
                "e" => {
                    // Reported event
                    if tag.len() > 1 {
                        *stats.reported_event_counts.entry(tag[1].clone()).or_insert(0) += 1;
                    }
                }
                _ => {}
            }
        }
    }

    stats.unique_reporters = stats.reporter_counts.len();
    stats.unique_reported_pubkeys = stats.reported_pubkey_counts.len();
    stats.unique_reported_events = stats.reported_event_counts.len();

    stats
}

fn print_top_n(title: &str, counts: &HashMap<String, usize>, n: usize) {
    let mut sorted: Vec<_> = counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));

    println!("\n{}:", title);
    for (key, count) in sorted.iter().take(n) {
        // Truncate long keys for display
        let display_key = if key.len() > 64 {
            format!("{}...", &key[..61])
        } else {
            key.to_string()
        };
        println!("  {} - {} reports", display_key, count);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("🔍 Analyzing NIP-56 Report Events (kind 1985)...\n");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .danger_accept_invalid_certs(true)
        .build()?;

    let mut all_events: Vec<NostrEvent> = Vec::new();

    // Query nostr.band API for kind 1985 events (labels/reports)
    println!("📡 Fetching kind 1985 events via HTTP API...");
    let url = "https://api.nostr.band/v0/events?kind=1985&limit=5000";

    match client.get(url).send().await {
        Ok(response) => {
            if response.status().is_success() {
                let data: NostrBandResponse = response.json().await?;
                println!("✅ Received {} kind 1985 events", data.events.len());
                all_events.extend(data.events);
            } else {
                println!("⚠️ HTTP {} for kind 1985", response.status());
            }
        }
        Err(e) => {
            println!("❌ Failed to fetch kind 1985: {}", e);
        }
    }

    // Also try kind 1984 (original report kind)
    println!("\n📡 Fetching kind 1984 events via HTTP API...");
    let url = "https://api.nostr.band/v0/events?kind=1984&limit=5000";

    match client.get(url).send().await {
        Ok(response) => {
            if response.status().is_success() {
                let data: NostrBandResponse = response.json().await?;
                println!("✅ Received {} kind 1984 events", data.events.len());
                all_events.extend(data.events);
            } else {
                println!("⚠️ HTTP {} for kind 1984", response.status());
            }
        }
        Err(e) => {
            println!("❌ Failed to fetch kind 1984: {}", e);
        }
    }

    println!("\n✅ Total events collected: {}", all_events.len());

    if all_events.is_empty() {
        println!("No report events found.");
        return Ok(());
    }

    let stats = analyze_events(&all_events);

    println!("\n═══════════════════════════════════════════════════════");
    println!("                   REPORT ANALYSIS");
    println!("═══════════════════════════════════════════════════════");
    println!("\n📊 Summary:");
    println!("  Total report events: {}", stats.total_events);
    println!("  Unique reporters: {}", stats.unique_reporters);
    println!("  Unique reported pubkeys: {}", stats.unique_reported_pubkeys);
    println!("  Unique reported events: {}", stats.unique_reported_events);

    println!("\n📌 Events by kind:");
    for (kind, count) in &stats.kind_counts {
        println!("  Kind {}: {} events", kind, count);
    }

    print_top_n("🏷️  Label Types (top 20)", &stats.label_counts, 20);
    print_top_n("👤 Most Active Reporters (top 10)", &stats.reporter_counts, 10);
    print_top_n("🎯 Most Reported Pubkeys (top 10)", &stats.reported_pubkey_counts, 10);
    print_top_n("📄 Most Reported Events (top 10)", &stats.reported_event_counts, 10);

    // Print some sample events for context
    println!("\n═══════════════════════════════════════════════════════");
    println!("                   SAMPLE EVENTS");
    println!("═══════════════════════════════════════════════════════");

    for (i, event) in all_events.iter().take(5).enumerate() {
        println!("\n--- Event {} ---", i + 1);
        println!("ID: {}", event.id);
        println!("Kind: {}", event.kind);
        println!("Reporter: {}", event.pubkey);
        println!("Created: {}", chrono::DateTime::from_timestamp(event.created_at, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| event.created_at.to_string()));
        println!("Tags:");
        for tag in &event.tags {
            println!("  {:?}", tag);
        }
        if !event.content.is_empty() {
            let content_preview = if event.content.len() > 200 {
                format!("{}...", &event.content[..200])
            } else {
                event.content.clone()
            };
            println!("Content: {}", content_preview);
        }
    }

    println!("\n✅ Analysis complete!");
    Ok(())
}
