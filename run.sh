#!/bin/sh

export UPSTREAM_SERVERS=https://blossom.primal.net,https://blossom.yakihonne.com/,https://cdn.satellite.earth/,https://24242.io/,https://blossom.band/,https://nostr.download/
export MAX_UPSTREAM_DOWNLOAD_SIZE_MB=5000
export MAX_TOTAL_SIZE=50000
export MAX_TOTAL_FILES=999999999
export ALLOW_WOT=true
export ALLOWED_NPUBS=npub1klr0dy2ul2dx9llk58czvpx73rprcmrvd5dc7ck8esg8f8es06qs427gxc,npub106nla9les99krufcx2r2ylzycvqqhpj25mgpv0l9hf8ew99hwlpqlq7ze5
export MAX_CHUNK_SIZE_MB=200
./target/release/almond












# upload server configuration
#export MAX_TOTAL_SIZE=50000 # 50GB storage
#export MAX_FILE_AGE_DAYS=1 # store uploaded blobs for 1 day
#export STORAGE_PATH=./storage
#export ALLOW_WOT=true
#export ALLOWED_NPUBS=npub1klr0dy2ul2dx9llk58czvpx73rprcmrvd5dc7ck8esg8f8es06qs427gxc,npub106nla9les99krufcx2r2ylzycvqqhpj25mgpv0l9hf8ew99hwlpqlq7ze5
#./target/release/almond
