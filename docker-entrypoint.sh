#!/bin/sh
set -e

# Writable optional state belongs outside the application directory. Keep
# operator-provided paths intact while supplying Docker-safe defaults.
export TLS_CERT_PATH="${TLS_CERT_PATH:-/app/state/cert.pem}"
export TLS_KEY_PATH="${TLS_KEY_PATH:-/app/state/key.pem}"
export CASHU_WALLET_PATH="${CASHU_WALLET_PATH:-/app/state/cashu_wallet.db}"

echo "========================================"
echo "Starting Almond Blossom Server"
echo "========================================"
echo "Binary: /app/almond"
echo "Working directory: $(pwd)"
echo "Binary exists: $(test -f /app/almond && echo 'YES' || echo 'NO')"
echo "Binary executable: $(test -x /app/almond && echo 'YES' || echo 'NO')"
echo ""
echo "Environment variables:"
echo "  BIND_ADDR=${BIND_ADDR}"
echo "  PUBLIC_URL=${PUBLIC_URL}"
echo "  STORAGE_PATH=${STORAGE_PATH:-./files}"
echo "  MAX_TOTAL_SIZE=${MAX_TOTAL_SIZE}"
echo "  MAX_TOTAL_FILES=${MAX_TOTAL_FILES}"
echo "  RUST_LOG=${RUST_LOG}"
echo "========================================"
echo ""

# Execute the binary
echo "Executing /app/almond..."
exec /app/almond
