#!/bin/sh

export ALMOND_UPSTREAM_SERVERS=https://blossom.primal.net,https://blossom.yakihonne.com/,https://cdn.satellite.earth/,https://24242.io/,https://blossom.band/,https://nostr.download/
export ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE=5000MiB
export ALMOND_STORAGE_MAX_SIZE=50000MiB
export ALMOND_STORAGE_MAX_FILES=999999999
export ALMOND_ALLOWED_NPUBS=npub1klr0dy2ul2dx9llk58czvpx73rprcmrvd5dc7ck8esg8f8es06qs427gxc,npub106nla9les99krufcx2r2ylzycvqqhpj25mgpv0l9hf8ew99hwlpqlq7ze5
export ALMOND_CHUNK_MAX_SIZE=200MiB
./target/release/almond












# upload server configuration
#export ALMOND_STORAGE_MAX_SIZE=50000MiB # 50GB storage
#export ALMOND_UPLOAD_MAX_AGE=1d # store uploaded blobs for 1 day
#export ALMOND_STORAGE_PATH=./storage
#export ALMOND_ALLOWED_NPUBS=npub1klr0dy2ul2dx9llk58czvpx73rprcmrvd5dc7ck8esg8f8es06qs427gxc,npub106nla9les99krufcx2r2ylzycvqqhpj25mgpv0l9hf8ew99hwlpqlq7ze5
#./target/release/almond
