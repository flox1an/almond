#!/bin/sh

# Local Blossom Cache configuration
# See README.md "Local Blossom Cache" section

export ALMOND_BIND_ADDR=127.0.0.1:24242
export ALMOND_PUBLIC_URL=http://127.0.0.1:24242
export ALMOND_UPLOAD_ACCESS=public
export ALMOND_MIRROR_ACCESS=off
export ALMOND_LIST_ENABLED=true
export ALMOND_HOMEPAGE_ENABLED=true
export ALMOND_CUSTOM_ORIGIN_ACCESS=public
export ALMOND_UPSTREAM_MODE=redirect-and-cache
export ALMOND_STORAGE_MAX_SIZE=5000MiB
export ALMOND_UPLOAD_MAX_AGE=300d
export ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE=500MiB

export ALMOND_STORAGE_PATH=./storage5
export ALMOND_METRICS_TOKEN=test

./target/release/almond
