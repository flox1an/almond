#!/bin/bash
set -euo pipefail

FIPS_CONFIG_PATH="${FIPS_CONFIG_PATH:-/etc/fips/fips.yaml}"
FIPS_UDP_BIND="${FIPS_UDP_BIND:-0.0.0.0:2121}"
FIPS_TCP_BIND="${FIPS_TCP_BIND:-}"
FIPS_TUN_NAME="${FIPS_TUN_NAME:-fips0}"
FIPS_TUN_MTU="${FIPS_TUN_MTU:-1280}"
FIPS_REWRITE_DNS="${FIPS_REWRITE_DNS:-true}"
FIPS_ISOLATE="${FIPS_ISOLATE:-false}"
FIPS_POD_IFACE="${FIPS_POD_IFACE:-eth0}"
FIPS_PEER_ALIAS="${FIPS_PEER_ALIAS:-peer}"
FIPS_PEER_TRANSPORT="${FIPS_PEER_TRANSPORT:-udp}"
FIPS_PEERS="${FIPS_PEERS:-}"
FIPS_UDP_PORT="${FIPS_UDP_PORT:-${FIPS_UDP_BIND##*:}}"

cleanup() {
    [ -n "${ALMOND_PID:-}" ] && kill "$ALMOND_PID" 2>/dev/null || true
    [ -n "${FIPS_PID:-}" ] && kill "$FIPS_PID" 2>/dev/null || true
}

trap cleanup INT TERM EXIT

build_peers_yaml() {
    # Multi-peer format (preferred):
    # one peer per line: npub,addr[,alias[,transport]]
    # example:
    # npub1...,203.0.113.10:2121,gateway-a,udp
    # npub1...,198.51.100.20:2121,gateway-b,udp
    if [ -n "${FIPS_PEERS}" ]; then
        local peers_output=""
        local line=""
        while IFS= read -r line || [ -n "$line" ]; do
            line="$(echo "$line" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
            [ -z "$line" ] && continue
            case "$line" in
                \#*) continue ;;
            esac

            IFS=',' read -r npub addr alias transport _ <<< "$line"
            npub="$(echo "${npub:-}" | xargs)"
            addr="$(echo "${addr:-}" | xargs)"
            alias="$(echo "${alias:-}" | xargs)"
            transport="$(echo "${transport:-}" | xargs)"

            if [ -z "$npub" ] || [ -z "$addr" ]; then
                echo "Invalid FIPS_PEERS entry (expected: npub,addr[,alias[,transport]]): $line" >&2
                exit 1
            fi

            [ -z "$alias" ] && alias="$FIPS_PEER_ALIAS"
            [ -z "$transport" ] && transport="$FIPS_PEER_TRANSPORT"

            peers_output="${peers_output}
  - npub: \"${npub}\"
    alias: \"${alias}\"
    addresses:
      - transport: ${transport}
        addr: \"${addr}\"
    connect_policy: auto_connect
    auto_reconnect: true"
        done <<< "$FIPS_PEERS"

        if [ -n "$peers_output" ]; then
            printf '%s\n' "$peers_output" | sed '/^[[:space:]]*$/d'
            return
        fi
    fi

    if [ -n "${FIPS_PEER_NPUB:-}" ] && [ -n "${FIPS_PEER_ADDR:-}" ]; then
        cat <<EOF
  - npub: "${FIPS_PEER_NPUB}"
    alias: "${FIPS_PEER_ALIAS}"
    addresses:
      - transport: ${FIPS_PEER_TRANSPORT}
        addr: "${FIPS_PEER_ADDR}"
    connect_policy: auto_connect
    auto_reconnect: true
EOF
        return
    fi

    echo "  []"
}

build_tcp_yaml() {
    if [ -n "$FIPS_TCP_BIND" ]; then
        cat <<EOF
  tcp:
    bind_addr: "${FIPS_TCP_BIND}"
EOF
    fi
}

generate_fips_config() {
    if [ -z "${FIPS_NSEC:-}" ]; then
        echo "FIPS_NSEC is required unless ${FIPS_CONFIG_PATH} is mounted and FIPS_GENERATE_CONFIG=false" >&2
        exit 1
    fi

    mkdir -p "$(dirname "$FIPS_CONFIG_PATH")" /run/fips

    local peers_yaml
    local tcp_yaml
    peers_yaml="$(build_peers_yaml)"
    tcp_yaml="$(build_tcp_yaml)"

    cat > "$FIPS_CONFIG_PATH" <<EOF
node:
  identity:
    nsec: "${FIPS_NSEC}"
  control:
    enabled: true
    socket_path: "/run/fips/control.sock"

tun:
  enabled: true
  name: ${FIPS_TUN_NAME}
  mtu: ${FIPS_TUN_MTU}

dns:
  enabled: true
  bind_addr: "127.0.0.1"
  port: 5354

transports:
  udp:
    bind_addr: "${FIPS_UDP_BIND}"
${tcp_yaml}
peers:
${peers_yaml}
EOF
}

configure_dns() {
    if [ "$FIPS_REWRITE_DNS" != "true" ]; then
        return
    fi

    local upstream_nameservers
    upstream_nameservers="$(awk '/^nameserver[[:space:]]+/ && $2 !~ /^127[.]/ && $2 != "::1" { print $2 }' /etc/resolv.conf || true)"

    if [ -z "$upstream_nameservers" ]; then
        upstream_nameservers="127.0.0.11"
    fi

    while IFS= read -r nameserver; do
        [ -n "$nameserver" ] && echo "server=${nameserver}" >> /etc/dnsmasq.d/fips.conf
    done <<< "$upstream_nameservers"

    local search_line
    search_line="$(awk '/^(search|domain)[[:space:]]+/ { print; exit }' /etc/resolv.conf || true)"

    {
        echo "nameserver 127.0.0.1"
        [ -n "$search_line" ] && echo "$search_line"
    } > /etc/resolv.conf
}

configure_fips_hosts() {
    mkdir -p /etc/fips
    touch /etc/fips/hosts

    if [ -n "${FIPS_HOSTS:-}" ]; then
        printf '%s\n' "$FIPS_HOSTS" > /etc/fips/hosts
    fi
}

configure_isolation() {
    if [ "$FIPS_ISOLATE" != "true" ]; then
        return
    fi

    iptables -A OUTPUT -o lo -j ACCEPT
    iptables -A INPUT -i lo -j ACCEPT
    iptables -A OUTPUT -o "$FIPS_POD_IFACE" -p udp --dport "$FIPS_UDP_PORT" -j ACCEPT
    iptables -A OUTPUT -o "$FIPS_POD_IFACE" -p udp --sport "$FIPS_UDP_PORT" -j ACCEPT
    iptables -A INPUT -i "$FIPS_POD_IFACE" -p udp --dport "$FIPS_UDP_PORT" -j ACCEPT
    iptables -A INPUT -i "$FIPS_POD_IFACE" -p udp --sport "$FIPS_UDP_PORT" -j ACCEPT
    iptables -A INPUT -i "$FIPS_POD_IFACE" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
    iptables -A OUTPUT -o "$FIPS_POD_IFACE" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
    iptables -A OUTPUT -o "$FIPS_POD_IFACE" -j DROP
    iptables -A INPUT -i "$FIPS_POD_IFACE" -j DROP

    ip6tables -A OUTPUT -o lo -j ACCEPT
    ip6tables -A INPUT -i lo -j ACCEPT
    ip6tables -A OUTPUT -o "$FIPS_TUN_NAME" -j ACCEPT
    ip6tables -A INPUT -i "$FIPS_TUN_NAME" -j ACCEPT
    ip6tables -A OUTPUT -o "$FIPS_POD_IFACE" -j DROP
    ip6tables -A INPUT -i "$FIPS_POD_IFACE" -j DROP
}

if [ "${FIPS_GENERATE_CONFIG:-true}" = "true" ] || [ ! -f "$FIPS_CONFIG_PATH" ]; then
    generate_fips_config
fi

# Write TLS cert/key from environment variables if provided
if [ -n "${TLS_CERT:-}" ] && [ -n "${TLS_KEY:-}" ]; then
    TLS_CERT_PATH="${TLS_CERT_PATH:-/app/cert.pem}"
    TLS_KEY_PATH="${TLS_KEY_PATH:-/app/key.pem}"
    printf '%s' "$TLS_CERT" > "$TLS_CERT_PATH"
    printf '%s' "$TLS_KEY" > "$TLS_KEY_PATH"
    chmod 600 "$TLS_KEY_PATH"
fi

configure_fips_hosts
configure_dns
configure_isolation

ip6tables -t mangle -A OUTPUT -o "$FIPS_TUN_NAME" -p tcp --tcp-flags SYN,RST SYN -j TCPMSS --clamp-mss-to-pmtu 2>/dev/null || true

dnsmasq --conf-dir=/etc/dnsmasq.d
fips --config "$FIPS_CONFIG_PATH" &
FIPS_PID="$!"

echo "Starting Almond Blossom Server with FIPS support"
echo "  BIND_ADDR=${BIND_ADDR}"
echo "  PUBLIC_URL=${PUBLIC_URL}"
echo "  STORAGE_PATH=${STORAGE_PATH:-./files}"
echo "  UPSTREAM_SERVERS=${UPSTREAM_SERVERS:-}"
echo "  FIPS_CONFIG_PATH=${FIPS_CONFIG_PATH}"
echo "  FIPS_TUN_NAME=${FIPS_TUN_NAME}"

/app/almond &
ALMOND_PID="$!"

wait -n "$FIPS_PID" "$ALMOND_PID"
