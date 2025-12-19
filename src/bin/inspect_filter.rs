/// Binary to inspect the serialization format of BinaryFuse16
/// Usage: cargo run --bin inspect_filter

use base64::Engine;
use xorf::{BinaryFuse16, Filter};

fn main() {
    // Create a simple test filter with known values
    let test_hashes = vec![
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03",
    ];

    // Convert to u64 fingerprints (using same logic as server)
    let fingerprints: Vec<u64> = test_hashes
        .iter()
        .map(|hex| sha256_to_u64(hex))
        .collect();

    println!("Test fingerprints:");
    for (i, fp) in fingerprints.iter().enumerate() {
        println!("  [{}] 0x{:016x} ({})", i, fp, fp);
    }

    // Build filter
    let filter = BinaryFuse16::try_from(&fingerprints).expect("Failed to build filter");

    // Test contains
    println!("\nFilter contains test:");
    for (i, fp) in fingerprints.iter().enumerate() {
        println!("  [{}] contains: {}", i, filter.contains(fp));
    }

    // Serialize with MessagePack
    let serialized = rmp_serde::to_vec(&filter).expect("Failed to serialize");

    println!("\nSerialized filter:");
    println!("  Length: {} bytes", serialized.len());
    println!("  Hex dump (first 100 bytes):");
    for (i, chunk) in serialized.chunks(16).take(7).enumerate() {
        print!("    {:04x}: ", i * 16);
        for byte in chunk {
            print!("{:02x} ", byte);
        }
        // ASCII representation
        print!(" | ");
        for byte in chunk {
            if *byte >= 32 && *byte < 127 {
                print!("{}", *byte as char);
            } else {
                print!(".");
            }
        }
        println!();
    }

    // Try to inspect internal structure
    println!("\nInternal structure analysis:");
    println!("  Filter len: {}", filter.len());

    // Base64 encode (like the server does)
    let b64 = base64::engine::general_purpose::STANDARD.encode(&serialized);
    println!("\nBase64 encoded:");
    println!("  {}", b64);
}

fn sha256_to_u64(hex: &str) -> u64 {
    let bytes: Vec<u8> = (0..16)
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();

    if bytes.len() >= 8 {
        u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    } else {
        0
    }
}
