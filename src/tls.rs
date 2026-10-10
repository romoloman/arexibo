//! Shared Mozilla and system CA trust for Rust HTTPS and XMR clients.

use once_cell::sync::Lazy;
use rustls_pki_types::CertificateDer;
use std::path::Path;

fn system_certificates() -> rustls_native_certs::CertificateResult {
    // Use Linux system locations directly: load_native_certs() also honors
    // environment overrides, which must not affect the player's trust store.
    const SYSTEM_BUNDLES: &[&str] = &[
        "/etc/ssl/certs/ca-certificates.crt", // Debian / Ubuntu
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem", // RHEL
        "/etc/pki/tls/certs/ca-bundle.crt", // Fedora / older RHEL
        "/etc/ssl/ca-bundle.pem", // SUSE
        "/etc/pki/tls/cacert.pem", // OpenELEC
        "/etc/ssl/cert.pem", // Alpine
        "/opt/etc/ssl/certs/ca-certificates.crt", // Entware
        "/etc/ssl/certs/cacert.pem", // OpenHarmony
    ];
    let bundle = SYSTEM_BUNDLES.iter().map(Path::new).find(|path| path.exists());
    let mut result = rustls_native_certs::load_certs_from_paths(bundle, None);
    for directory in ["/etc/ssl/certs", "/etc/pki/tls/certs", "/etc/security/certificates"] {
        let directory = Path::new(directory);
        if directory.is_dir() {
            let loaded = rustls_native_certs::load_certs_from_paths(None, Some(directory));
            result.certs.extend(loaded.certs);
            result.errors.extend(loaded.errors);
        }
    }
    if bundle.is_none() && result.certs.is_empty() {
        log::warn!("no system CA certificates found in Linux trust store locations");
    }
    result.certs.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
    result.certs.dedup();
    result
}

// Load once per process, just as the HTTP agents retain their TLS configuration.
static ROOT_CERTIFICATES: Lazy<Vec<CertificateDer<'static>>> = Lazy::new(|| {
    let native = system_certificates();
    for error in native.errors {
        log::warn!("could not load a system CA certificate: {error}");
    }
    let mut certificates = webpki_root_certs::TLS_SERVER_ROOT_CERTS.to_vec();
    let mut store = rustls::RootCertStore::empty();
    for certificate in native.certs {
        match store.add(certificate.clone()) {
            Ok(()) => certificates.push(certificate),
            Err(error) => log::warn!("ignoring invalid system CA certificate: {error}"),
        }
    }
    certificates
});

pub(crate) fn root_store() -> rustls::RootCertStore {
    let mut store = rustls::RootCertStore::empty();
    store.add_parsable_certificates(ROOT_CERTIFICATES.iter().cloned());
    store
}

pub(crate) fn http_config(no_verify: bool) -> ureq::tls::TlsConfig {
    let mut builder = ureq::tls::TlsConfig::builder().disable_verification(no_verify);
    if !no_verify {
        let certificates = ROOT_CERTIFICATES.iter()
            .map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned());
        builder = builder.root_certs(ureq::tls::RootCerts::from(certificates));
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::{Read, Write}, net::TcpListener, process::Command, sync::Arc};

    fn request(host: &str, trusted: bool, directory: &std::path::Path) {
        let certificate = CertificateDer::from(fs::read(directory.join("cert.der")).unwrap());
        let key = rustls_pki_types::PrivateKeyDer::Pkcs8(
            fs::read(directory.join("key.der")).unwrap().into());
        let server = Arc::new(rustls::ServerConfig::builder()
            .with_no_client_auth().with_single_cert(vec![certificate], key).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(server).unwrap(), socket);
            let mut buffer = [0; 4096];
            if stream.read(&mut buffer).is_ok() {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(http_config(false)).proxy(None)
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build().into();
        let response = agent.get(format!("https://{host}:{port}/")).call();
        assert_eq!(response.is_ok(), trusted, "{host}: {response:?}");
        thread.join().unwrap();
    }

    #[test]
    fn system_ca_ignores_environment_overrides() {
        // Isolate CA environment variables and the Lazy cache in child processes
        // so this test cannot change the trust policy of other concurrent tests.
        if let Some(directory) = std::env::var_os("AREXIBO_TLS_TEST_DIR") {
            let directory = std::path::PathBuf::from(directory);
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let certificate = CertificateDer::from(fs::read(directory.join("cert.der")).unwrap());
            let store = root_store();
            assert!(store.len() >= webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
            let mut custom_store = rustls::RootCertStore::empty();
            custom_store.add(certificate).unwrap();
            assert!(!store.roots.contains(&custom_store.roots[0]));
            request("localhost", false, &directory);
            request("127.0.0.1", false, &directory); // certificate has only a DNS SAN
            return;
        }
        let directory = std::env::temp_dir().join(format!("arexibo-tls-{}", std::process::id()));
        fs::create_dir_all(directory.join("override-certs")).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        fs::write(directory.join("cert.der"), cert.cert.der()).unwrap();
        fs::write(directory.join("key.der"), cert.key_pair.serialize_der()).unwrap();
        fs::write(directory.join("ca.pem"), cert.cert.pem()).unwrap();
        fs::write(directory.join("override-certs/ca.pem"), cert.cert.pem()).unwrap();
        for (bundle, cert_dir) in [("ca.pem", "override-certs"), ("missing.pem", "override-certs"), ("ca.pem", "missing")] {
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tls::tests::system_ca_ignores_environment_overrides", "--nocapture"])
                .env("AREXIBO_TLS_TEST_DIR", &directory)
                .env("SSL_CERT_FILE", directory.join(bundle))
                .env("SSL_CERT_DIR", directory.join(cert_dir))
                .status().unwrap();
            assert!(status.success(), "CA bundle {bundle}");
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
