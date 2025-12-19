use xorf::{BinaryFuse16, Filter};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct FilterDebug {
    seed: u64,
    segment_length: u32,
    segment_length_mask: u32,
    segment_count_length: u32,
    fingerprints: Vec<u16>,
}

fn main() {
    let keys: Vec<u64> = vec![1, 2, 3, 4, 5];
    let filter = BinaryFuse16::try_from(&keys).expect("Failed to create filter");

    let serialized = rmp_serde::to_vec(&filter).expect("Failed to serialize");
    println!("Serialized length: {}", serialized.len());
    println!("All bytes (hex): ");
    for (i, byte) in serialized.iter().enumerate() {
        print!("{:02x} ", byte);
        if (i + 1) % 16 == 0 {
            println!();
        }
    }
    println!("\n");

    // Deserialize to verify structure
    let deserialized: BinaryFuse16 = rmp_serde::from_slice(&serialized)
        .expect("Failed to deserialize");

    println!("Filter works: {}", deserialized.contains(&1u64));

    // Deserialize as debug struct to see field names
    let debug: FilterDebug = rmp_serde::from_slice(&serialized)
        .expect("Failed to deserialize as debug");

    println!("\nFilter structure:");
    println!("  seed: 0x{:016x}", debug.seed);
    println!("  segment_length: {}", debug.segment_length);
    println!("  segment_length_mask: {}", debug.segment_length_mask);
    println!("  segment_count_length: {}", debug.segment_count_length);
    println!("  fingerprints.len(): {}", debug.fingerprints.len());
    println!("  fingerprints (first 10): {:?}", &debug.fingerprints[..10.min(debug.fingerprints.len())]);

    // Print as base64 for easier testing
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    println!("\nBase64: {}", BASE64.encode(&serialized));
}
