#!/bin/sh

# Local Blossom Cache configuration
# See README.md "Local Blossom Cache" section

export BIND_ADDR=127.0.0.1:24242
export PUBLIC_URL=http://127.0.0.1:24242
export FEATURE_UPLOAD_ENABLED=public
export FEATURE_MIRROR_ENABLED=off
export FEATURE_LIST_ENABLED=true
export FEATURE_HOMEPAGE_ENABLED=true
export FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED=public
export UPSTREAM_MODE=redirect_and_cache
export MAX_TOTAL_SIZE=5000
export MAX_FILE_AGE_DAYS=300
export MAX_UPSTREAM_DOWNLOAD_SIZE_MB=500

export STORAGE_PATH=./storage5
export METRICS_BEARER_TOKEN=test

./target/release/almond
