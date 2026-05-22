// Debug script to understand MessagePack structure
const MessagePack = require('@msgpack/msgpack');
const fs = require('fs');

// Read the base64 filter from the server response
const base64Data = "lc+RCi3siQJcwQgHCNwAGM1Wks20uc1JrM1ef82zKs0uzM2wx83/qc3ea80DYs2a5M0gts0P6M0B0802782mFc21pM3zus2XFM2ggM1yF83Brs3OQM1SIg==";

// Decode base64 to bytes
const bytes = Buffer.from(base64Data, 'base64');

console.log('Total bytes:', bytes.length);
console.log('Hex dump:');
console.log(bytes.toString('hex'));
console.log('\nByte-by-byte:');

for (let i = 0; i < Math.min(20, bytes.length); i++) {
    const byte = bytes[i];
    console.log(`[${i}] 0x${byte.toString(16).padStart(2, '0')} (${byte})`);
}

console.log('\n--- Attempting to decode ---');

try {
    const data = MessagePack.decode(bytes, { useBigInt64: true });
    console.log('Success!');
    console.log('Decoded data:', JSON.stringify(data, (_, v) => typeof v === 'bigint' ? v.toString() : v, 2));
} catch (e) {
    console.error('Error:', e.message);
    console.error('Stack:', e.stack);
}
