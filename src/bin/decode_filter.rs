/// Decode a MessagePack-encoded BinaryFuse16 filter to understand its structure

fn main() {
    // Base64 from localhost:3000/filter
    let base64_data = "lc+RCi3siQJcwQgHCNwAGM2sDc0qQc01Nc1Vfc3eOc3TL80tqs2Mk83S6M0GKs0Fgc3sZs1kuc3DLs3v0s0C+c1ADc1h3s04+s027c1np83H7M02683tyA==";

    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        base64_data
    ).expect("Failed to decode base64");

    println!("Raw bytes ({} bytes):", bytes.len());
    for (i, chunk) in bytes.chunks(16).enumerate() {
        print!("  {:04x}: ", i * 16);
        for byte in chunk {
            print!("{:02x} ", byte);
        }
        println!();
    }

    // Try to deserialize as generic JSON value
    let value: serde_json::Value = rmp_serde::from_slice(&bytes)
        .expect("Failed to deserialize to JSON value");

    println!("\nMessagePack structure as JSON:");
    println!("{}", serde_json::to_string_pretty(&value).unwrap());

    // Try to understand the structure
    match value {
        serde_json::Value::Array(arr) => {
            println!("\n✓ It's an array with {} elements:", arr.len());
            for (i, item) in arr.iter().enumerate() {
                println!("  [{}]: {}", i, describe_json_value(item));
            }
        }
        serde_json::Value::Object(obj) => {
            println!("\n✓ It's an object with {} fields:", obj.len());
            for (key, val) in obj.iter() {
                println!("  {}: {}", key, describe_json_value(val));
            }
        }
        _ => {
            println!("\n✗ Unexpected structure type");
        }
    }
}

fn describe_json_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Number(n) => format!("Number({})", n),
        serde_json::Value::String(s) => format!("String(\"{}\")", s),
        serde_json::Value::Array(arr) => format!("Array[{}]", arr.len()),
        serde_json::Value::Object(obj) => format!("Object{{{} fields}}", obj.len()),
        serde_json::Value::Bool(b) => format!("Bool({})", b),
        serde_json::Value::Null => "Null".to_string(),
    }
}
