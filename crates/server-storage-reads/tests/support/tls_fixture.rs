//! Synthetic-only PKI generation with fixed commands and descriptor-capped files.
#[allow(dead_code)]
#[path = "../../../../tools/qualification/process.rs"]
mod process;
#[path = "../../src/bin/support/tls_file.rs"]
mod tls_file;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
pub use tls_file::read;
static COUNT: AtomicU32 = AtomicU32::new(0);
fn create(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "fixture create refused")?;
    file.write_all(bytes)
        .map_err(|_| "fixture config write refused".into())
}
pub struct Fixture {
    pub directory: PathBuf,
    pub ca: Vec<u8>,
    pub ip_cert: Vec<u8>,
    pub ip_key: Vec<u8>,
    pub dns_cert: Vec<u8>,
    pub dns_key: Vec<u8>,
}
impl Fixture {
    pub fn generate() -> Result<Self, String> {
        let directory = std::env::temp_dir().join(format!(
            "m4-synthetic-tls-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|_| "fixture directory refused")?;
        create(
            &directory.join("ca.req.cnf"),
            b"[req]\nprompt=no\ndistinguished_name=dn\n[dn]\nCN=M4SyntheticCA\n",
        )?;
        create(
            &directory.join("leaf.req.cnf"),
            b"[req]\nprompt=no\ndistinguished_name=dn\n[dn]\nCN=M4SyntheticLeaf\n",
        )?;
        for (name, san) in [("ip", "IP:127.0.0.1,IP:::1"), ("dns", "DNS:localhost")] {
            create(&directory.join(format!("{name}.cnf")),format!("[leaf]\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName={san}\n").as_bytes())?;
        }
        for name in [
            "ca.key.pem",
            "ca.pem",
            "ca.der",
            "ip.key.pem",
            "ip.csr.pem",
            "ip.der",
            "ip.key.der",
            "dns.key.pem",
            "dns.csr.pem",
            "dns.der",
            "dns.key.der",
        ] {
            create(&directory.join(name), &[])?;
        }
        let start = Instant::now();
        let commands: Vec<Vec<&str>> = vec![
            vec![
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-sha256",
                "-batch",
                "-config",
                "ca.req.cnf",
                "-x509",
                "-days",
                "2",
                "-subj",
                "/CN=M4SyntheticCA",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
                "-keyout",
                "ca.key.pem",
                "-out",
                "ca.pem",
            ],
            vec![
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-sha256",
                "-batch",
                "-config",
                "leaf.req.cnf",
                "-new",
                "-subj",
                "/CN=M4SyntheticLeaf",
                "-keyout",
                "ip.key.pem",
                "-out",
                "ip.csr.pem",
            ],
            vec![
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-sha256",
                "-batch",
                "-config",
                "leaf.req.cnf",
                "-new",
                "-subj",
                "/CN=M4SyntheticLeaf",
                "-keyout",
                "dns.key.pem",
                "-out",
                "dns.csr.pem",
            ],
            vec![
                "x509",
                "-req",
                "-sha256",
                "-in",
                "ip.csr.pem",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key.pem",
                "-set_serial",
                "1",
                "-days",
                "1",
                "-extfile",
                "ip.cnf",
                "-extensions",
                "leaf",
                "-outform",
                "DER",
                "-out",
                "ip.der",
            ],
            vec![
                "x509",
                "-req",
                "-sha256",
                "-in",
                "dns.csr.pem",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key.pem",
                "-set_serial",
                "2",
                "-days",
                "1",
                "-extfile",
                "dns.cnf",
                "-extensions",
                "leaf",
                "-outform",
                "DER",
                "-out",
                "dns.der",
            ],
            vec!["x509", "-in", "ca.pem", "-outform", "DER", "-out", "ca.der"],
            vec![
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                "ip.key.pem",
                "-outform",
                "DER",
                "-out",
                "ip.key.der",
            ],
            vec![
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                "dns.key.pem",
                "-outform",
                "DER",
                "-out",
                "dns.key.der",
            ],
        ];
        for args in commands {
            let remaining = Duration::from_secs(120).saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err("fixture overall deadline".into());
            }
            let mut command = Command::new("/usr/bin/openssl");
            command.env_clear().current_dir(&directory).args(args);
            let result = process::bounded(command, vec![], remaining.min(Duration::from_secs(30)))?;
            if !result.status.success() || !result.stdout.is_empty() {
                return Err("fixture generator refused".into());
            }
        }
        for name in [
            "ca.key.pem",
            "ca.pem",
            "ca.der",
            "ip.key.pem",
            "ip.csr.pem",
            "ip.der",
            "ip.key.der",
            "dns.key.pem",
            "dns.csr.pem",
            "dns.der",
            "dns.key.der",
        ] {
            let _ = read(&directory.join(name))?;
        }
        Ok(Self {
            ca: read(&directory.join("ca.der"))?,
            ip_cert: read(&directory.join("ip.der"))?,
            ip_key: read(&directory.join("ip.key.der"))?,
            dns_cert: read(&directory.join("dns.der"))?,
            dns_key: read(&directory.join("dns.key.der"))?,
            directory,
        })
    }
    pub fn server(&self, dns: bool) -> Result<Arc<rustls::ServerConfig>, String> {
        let (cert, key) = if dns {
            (&self.dns_cert, &self.dns_key)
        } else {
            (&self.ip_cert, &self.ip_key)
        };
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "fixture versions")?
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(cert.clone())],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                key.clone(),
            )),
        )
        .map_err(|_| "fixture key/cert")?;
        Ok(Arc::new(config))
    }
}
