# HTTPS Setup Guide

This guide explains how to configure Almond to run with HTTPS/TLS support.

## Quick Start

### Auto-Generated Self-Signed Certificate

For development, let Almond generate a self-signed certificate:

```bash
ALMOND_TLS_ENABLED=true ALMOND_TLS_SELF_SIGNED=true cargo run --bin almond
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
ALMOND_TLS_ENABLED=true \
ALMOND_TLS_CERT=/path/to/fullchain.pem \
ALMOND_TLS_KEY=/path/to/privkey.pem \
ALMOND_PUBLIC_URL=https://your-domain.com \
cargo run --bin almond
```

## Environment Variables

### HTTPS Configuration

- **`ALMOND_TLS_ENABLED`** (default: `false`)
  - Set to `true` to enable HTTPS/TLS
  - When enabled, server will only accept HTTPS connections

- **`ALMOND_TLS_CERT`** (default: `./cert.pem`)
  - Path to the TLS certificate file (PEM format)
  - Should contain the certificate chain

- **`ALMOND_TLS_KEY`** (default: `./key.pem`)
  - Path to the TLS private key file (PEM format)
  - Must be readable only by the server process

- **`ALMOND_TLS_SELF_SIGNED`** (default: `false`)
  - Automatically generate self-signed certificate if cert/key files not found
  - Leave `false` in production so missing certificates fail startup

- **`ALMOND_PUBLIC_URL`** (auto-detected)
  - Public URL for the service
  - Defaults to `https://127.0.0.1:3000` when HTTPS enabled
  - Defaults to `http://127.0.0.1:3000` when HTTPS disabled

## Docker Setup

### Self-Signed Certificate (Development)

```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e ALMOND_TLS_ENABLED=true \
  -e ALMOND_TLS_SELF_SIGNED=true \
  -e ALMOND_PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

### Custom Certificates (Production)

```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -v /path/to/certs:/app/certs \
  -e ALMOND_TLS_ENABLED=true \
  -e ALMOND_TLS_CERT=/app/certs/fullchain.pem \
  -e ALMOND_TLS_KEY=/app/certs/privkey.pem \
  -e ALMOND_PUBLIC_URL=https://your-domain.com \
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
ALMOND_TLS_ENABLED=true \
ALMOND_TLS_SELF_SIGNED=true \
ALMOND_UPSTREAM_SERVERS=https://cdn.satellite.earth,https://blossom.primal.net \
ALMOND_UPSTREAM_MAX_DOWNLOAD_SIZE=1000MiB \
ALMOND_UPLOAD_ACCESS=off \
ALMOND_MIRROR_ACCESS=off \
cargo run --bin almond
```

This configuration:
- Enables HTTPS with auto-generated self-signed cert
- Proxies content from upstream servers
- Disables uploads and mirrors (read-only cache)
- Allows up to 1GB downloads from upstream

### 2. Personal Server with HTTPS

```bash
ALMOND_TLS_ENABLED=true \
ALMOND_TLS_SELF_SIGNED=true \
ALMOND_ALLOWED_NPUBS=npub1... \
ALMOND_UPLOAD_ACCESS=wot \
ALMOND_MIRROR_ACCESS=wot \
cargo run --bin almond
```

### 3. Production Server with Let's Encrypt

Assuming you have certbot configured:

```bash
ALMOND_TLS_ENABLED=true \
ALMOND_TLS_CERT=/etc/letsencrypt/live/your-domain.com/fullchain.pem \
ALMOND_TLS_KEY=/etc/letsencrypt/live/your-domain.com/privkey.pem \
ALMOND_PUBLIC_URL=https://your-domain.com \
ALMOND_BIND_ADDR=0.0.0.0:443 \
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
3. **Certificate files not found**: Check paths in `ALMOND_TLS_CERT` and `ALMOND_TLS_KEY`
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
4. **Update ALMOND_PUBLIC_URL** to use `https://`
5. **Restart the server**
6. **Update client configurations** to use HTTPS URLs
7. **Optional**: Set up HTTP → HTTPS redirect via reverse proxy

## Additional Resources

- [Let's Encrypt](https://letsencrypt.org/) - Free SSL/TLS certificates
- [Certbot](https://certbot.eff.org/) - Automatic Let's Encrypt certificate management
- [rustls documentation](https://docs.rs/rustls/) - TLS library used by Almond
