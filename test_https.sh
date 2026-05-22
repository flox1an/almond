#!/bin/bash

# Start the server in the background
ENABLE_HTTPS=true cargo run &
SERVER_PID=$!

# Wait for server to start
sleep 3

# Test HTTPS connection (with self-signed cert)
echo "Testing HTTPS connection..."
curl -k https://127.0.0.1:3000/ -v 2>&1 | grep -E "(HTTP|SSL|TLS|Server)"

# Cleanup
kill $SERVER_PID 2>/dev/null

echo "Test complete!"
