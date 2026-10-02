#!/bin/sh
set -e

# Container defaults. They apply only when the operator set neither the new
# name nor its deprecated pre-0.5 alias, so `-e BIND_ADDR=...` keeps working.
# ponytail: fold back into Dockerfile ENV once the aliases are removed.
[ -n "${ALMOND_BIND_ADDR:-}${BIND_ADDR:-}" ] || export ALMOND_BIND_ADDR=0.0.0.0:3000
[ -n "${ALMOND_PUBLIC_URL:-}${PUBLIC_URL:-}" ] || export ALMOND_PUBLIC_URL=http://localhost:3000
[ -n "${ALMOND_STORAGE_PATH:-}${STORAGE_PATH:-}" ] || export ALMOND_STORAGE_PATH=/app/files
[ -n "${ALMOND_STORAGE_MAX_FILES:-}${MAX_TOTAL_FILES:-}" ] || export ALMOND_STORAGE_MAX_FILES=1000000
[ -n "${ALMOND_CLEANUP_INTERVAL:-}${CLEANUP_INTERVAL_SECS:-}" ] || export ALMOND_CLEANUP_INTERVAL=60s
# Writable optional state belongs outside the application directory.
[ -n "${ALMOND_TLS_CERT:-}${TLS_CERT_PATH:-}" ] || export ALMOND_TLS_CERT=/app/state/cert.pem
[ -n "${ALMOND_TLS_KEY:-}${TLS_KEY_PATH:-}" ] || export ALMOND_TLS_KEY=/app/state/key.pem
[ -n "${ALMOND_CASHU_WALLET_PATH:-}${CASHU_WALLET_PATH:-}" ] || export ALMOND_CASHU_WALLET_PATH=/app/state/cashu_wallet.db

echo "========================================"
echo "Starting Almond Blossom Server"
echo "========================================"
echo "Binary: /app/almond"
echo "Working directory: $(pwd)"
echo "Binary exists: $(test -f /app/almond && echo 'YES' || echo 'NO')"
echo "Binary executable: $(test -x /app/almond && echo 'YES' || echo 'NO')"
echo ""
echo "Environment variables:"
echo "  ALMOND_BIND_ADDR=${ALMOND_BIND_ADDR}"
echo "  ALMOND_PUBLIC_URL=${ALMOND_PUBLIC_URL}"
echo "  ALMOND_STORAGE_PATH=${ALMOND_STORAGE_PATH:-./files}"
echo "  ALMOND_STORAGE_MAX_SIZE=${ALMOND_STORAGE_MAX_SIZE}"
echo "  ALMOND_STORAGE_MAX_FILES=${ALMOND_STORAGE_MAX_FILES}"
echo "  RUST_LOG=${RUST_LOG}"
echo "========================================"
echo ""

# Execute the binary
echo "Executing /app/almond..."
exec /app/almond
