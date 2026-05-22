# HTTPS Setup Guide

This guide explains how to configure Almond to run with HTTPS/TLS support.

## Quick Start

### Auto-Generated Self-Signed Certificate

The easiest way to enable HTTPS is to let Almond automatically generate a self-signed certificate:

```bash
ENABLE_HTTPS=true cargo run --bin almond
```

On first run with HTTPS enabled, Almond will:
1. Check for existing certificates at `./cert.pem` and `./key.pem`
2. If not found, automatically generate a self-signed certificate
3. Start the HTTPS server on the configured address

The self-signed certificate includes Subject Alternative Names (SANs) for:
- `localhost`
- `127.0.0.1` (IPv4 loopback)
- `::1` (IPv6 loopback)

**Note:** Browsers will show a security warning for self-signed certificates. This is normal and expected for development/testing.

### Using Custom Certificates (Production)

For production use with trusted certificates (e.g., from Let's Encrypt):

```bash
ENABLE_HTTPS=true \
TLS_CERT_PATH=/path/to/fullchain.pem \
TLS_KEY_PATH=/path/to/privkey.pem \
TLS_AUTO_GENERATE=false \
PUBLIC_URL=https://your-domain.com \
cargo run --bin almond
```

## Environment Variables

### HTTPS Configuration

- **`ENABLE_HTTPS`** (default: `false`)
  - Set to `true` to enable HTTPS/TLS
  - When enabled, server will only accept HTTPS connections

- **`TLS_CERT_PATH`** (default: `./cert.pem`)
  - Path to the TLS certificate file (PEM format)
  - Should contain the certificate chain

- **`TLS_KEY_PATH`** (default: `./key.pem`)
  - Path to the TLS private key file (PEM format)
  - Must be readable only by the server process

- **`TLS_AUTO_GENERATE`** (default: `true`)
  - Automatically generate self-signed certificate if cert/key files not found
  - Set to `false` to require existing certificates (fail if missing)

- **`PUBLIC_URL`** (auto-detected)
  - Public URL for the service
  - Defaults to `https://127.0.0.1:3000` when HTTPS enabled
  - Defaults to `http://127.0.0.1:3000` when HTTPS disabled

## Docker Setup

### Self-Signed Certificate (Development)

```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e ENABLE_HTTPS=true \
  -e PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

### Custom Certificates (Production)

```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -v /path/to/certs:/app/certs \
  -e ENABLE_HTTPS=true \
  -e TLS_CERT_PATH=/app/certs/fullchain.pem \
  -e TLS_KEY_PATH=/app/certs/privkey.pem \
  -e TLS_AUTO_GENERATE=false \
  -e PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

## Testing HTTPS

### Test with curl

Accept self-signed certificate with `-k` flag:

```bash
curl -k https://localhost:3000/
```

### Test with browser

1. Navigate to `https://localhost:3000/`
2. Browser will show a security warning about the self-signed certificate
3. Click "Advanced" → "Proceed to localhost (unsafe)" (Chrome) or equivalent
4. The homepage should load

### Verify certificate details

```bash
openssl s_client -connect localhost:3000 -servername localhost < /dev/null 2>/dev/null | openssl x509 -text -noout
```

## Common Use Cases

### 1. Caching Proxy with HTTPS

Run as a caching edge server with HTTPS enabled:

```bash
ENABLE_HTTPS=true \
UPSTREAM_SERVERS=https://cdn.satellite.earth,https://blossom.primal.net \
MAX_UPSTREAM_DOWNLOAD_SIZE_MB=1000 \
FEATURE_UPLOAD_ENABLED=off \
FEATURE_MIRROR_ENABLED=off \
cargo run --bin almond
```

This configuration:
- Enables HTTPS with auto-generated self-signed cert
- Proxies content from upstream servers
- Disables uploads and mirrors (read-only cache)
- Allows up to 1GB downloads from upstream

### 2. Personal Server with HTTPS

```bash
ENABLE_HTTPS=true \
ALLOWED_NPUBS=npub1... \
FEATURE_UPLOAD_ENABLED=wot \
FEATURE_MIRROR_ENABLED=wot \
cargo run --bin almond
```

### 3. Production Server with Let's Encrypt

Assuming you have certbot configured:

```bash
ENABLE_HTTPS=true \
TLS_CERT_PATH=/etc/letsencrypt/live/your-domain.com/fullchain.pem \
TLS_KEY_PATH=/etc/letsencrypt/live/your-domain.com/privkey.pem \
TLS_AUTO_GENERATE=false \
PUBLIC_URL=https://your-domain.com \
BIND_ADDR=0.0.0.0:443 \
cargo run --bin almond
```

## Security Considerations

### Self-Signed Certificates

- **Development/Testing Only**: Self-signed certificates should only be used for development, testing, or private networks
- **Browser Warnings**: Users will see security warnings and must manually accept the certificate
- **No Chain of Trust**: Self-signed certificates are not trusted by browsers or operating systems by default

### Production Certificates

- **Use Let's Encrypt**: Free, automated, and trusted certificates
- **Certificate Renewal**: Automate certificate renewal (certbot can do this)
- **File Permissions**: Ensure private key is only readable by the server process (`chmod 600`)
- **Regular Updates**: Keep certificates up to date before expiration

### HTTPS Best Practices

1. **Always use HTTPS in production** when serving content over the internet
2. **Redirect HTTP to HTTPS** using a reverse proxy (nginx, caddy, etc.)
3. **Use HSTS headers** to enforce HTTPS (can be added via reverse proxy)
4. **Monitor certificate expiration** and automate renewal
5. **Secure private keys** with proper file permissions

## Troubleshooting

### Certificate Generation Failed

If certificate generation fails, check:
- Write permissions in the current directory
- Disk space availability
- SELinux/AppArmor policies (on Linux)

### Server Won't Start with HTTPS

Common issues:
1. **Port already in use**: Check if another service is using the port
2. **Permission denied**: Ports < 1024 require root/sudo on Linux
3. **Certificate files not found**: Check paths in `TLS_CERT_PATH` and `TLS_KEY_PATH`
4. **Invalid certificate format**: Ensure PEM format is used

### Browser Certificate Errors

For self-signed certificates:
1. Browser warnings are expected
2. You can add the certificate to your system's trust store
3. For testing, use curl with `-k` flag or browser "proceed anyway" option

## Migration from HTTP to HTTPS

If you're running HTTP and want to switch to HTTPS:

1. **Backup your data** (files directory)
2. **Set environment variables** for HTTPS
3. **Generate or install certificates**
4. **Update PUBLIC_URL** to use `https://`
5. **Restart the server**
6. **Update client configurations** to use HTTPS URLs
7. **Optional**: Set up HTTP → HTTPS redirect via reverse proxy

## Additional Resources

- [Let's Encrypt](https://letsencrypt.org/) - Free SSL/TLS certificates
- [Certbot](https://certbot.eff.org/) - Automatic Let's Encrypt certificate management
- [rustls documentation](https://docs.rs/rustls/) - TLS library used by Almond
