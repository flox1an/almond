#!/bin/sh
set -e

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
