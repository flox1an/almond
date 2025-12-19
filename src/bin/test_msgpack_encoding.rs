use xorf::BinaryFuse16;

fn main() {
    // Create a simple BinaryFuse16 filter
    let keys: Vec<u64> = vec![1, 2, 3, 4, 5];
    let filter = BinaryFuse16::try_from(&keys).expect("Failed to create filter");

    println!("Filter created with {} fingerprints", filter.fingerprints.len());

    // Serialize using MessagePack
    let serialized = rmp_serde::to_vec(&filter).expect("Failed to serialize");

    println!("\nSerialized bytes ({} total):", serialized.len());
    println!("\nByte-by-byte (first 40 bytes):");
    for (i, byte) in serialized.iter().enumerate().take(40) {
        println!("[{:3}] 0x{:02x} ({:3})", i, byte, byte);
    }

    // Try to deserialize it back
    let deserialized: BinaryFuse16 = rmp_serde::from_slice(&serialized)
        .expect("Failed to deserialize");

    println!("\nDeserialized successfully!");
    println!("Fingerprints length: {}", deserialized.fingerprints.len());
}
