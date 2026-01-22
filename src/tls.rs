use std::fs;
use std::path::PathBuf;
use tracing::{info, warn};

/// Generate a self-signed certificate and private key
pub fn generate_self_signed_cert(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    use rcgen::{generate_simple_self_signed, CertifiedKey};

    info!("🔐 Generating self-signed certificate...");

    // Generate certificate with localhost and common IP addresses
    let subject_alt_names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];

    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(subject_alt_names)?;

    // Write certificate
    fs::write(cert_path, cert.pem())?;
    info!("✅ Certificate written to: {}", cert_path.display());

    // Write private key
    fs::write(key_path, key_pair.serialize_pem())?;
    info!("✅ Private key written to: {}", key_path.display());

    warn!("⚠️  Self-signed certificate generated - clients will need to trust it");
    warn!("⚠️  For production, use a certificate from a trusted CA (e.g., Let's Encrypt)");

    Ok(())
}

/// Load TLS configuration from certificate and key files
pub async fn load_tls_config(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<axum_server::tls_rustls::RustlsConfig, Box<dyn std::error::Error>> {
    info!("🔐 Loading TLS configuration...");
    info!("📄 Certificate: {}", cert_path.display());
    info!("🔑 Private key: {}", key_path.display());

    // Check if files exist
    if !cert_path.exists() {
        return Err(format!("Certificate file not found: {}", cert_path.display()).into());
    }
    if !key_path.exists() {
        return Err(format!("Private key file not found: {}", key_path.display()).into());
    }

    // Build TLS config
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(
        fs::read(cert_path)?,
        fs::read(key_path)?,
    )
    .await?;

    info!("✅ TLS configuration loaded successfully");

    Ok(config)
}

/// Ensure TLS certificates exist, generating self-signed if needed
pub fn ensure_tls_certificates(
    cert_path: &PathBuf,
    key_path: &PathBuf,
    auto_generate: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cert_exists = cert_path.exists();
    let key_exists = key_path.exists();

    if cert_exists && key_exists {
        info!("✅ TLS certificates found");
        return Ok(());
    }

    if !auto_generate {
        if !cert_exists {
            return Err(format!("Certificate file not found: {}", cert_path.display()).into());
        }
        if !key_exists {
            return Err(format!("Private key file not found: {}", key_path.display()).into());
        }
    }

    // Auto-generate self-signed certificate
    warn!("⚠️  TLS certificates not found - generating self-signed certificate");

    // Ensure parent directories exist
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)?;
    }

    generate_self_signed_cert(cert_path, key_path)?;

    Ok(())
}
