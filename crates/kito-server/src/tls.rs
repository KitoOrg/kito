//! The certificate of a server, and the connections it encrypts with it.
//!
//! A server makes a key and a certificate for itself the first time it
//! needs them (`tls/key.pem`, `tls/cert.pem` in its directory). Nobody
//! vouches for such a certificate: clients know it by its fingerprint,
//! which `kito-server fingerprint` prints and `user add` gives with the
//! token. Whoever has a certificate from an authority puts it, and its
//! key, in the same two files.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

fn files(dir: &Path) -> (PathBuf, PathBuf) {
    let tls = dir.join("tls");
    (tls.join("cert.pem"), tls.join("key.pem"))
}

/// Makes the server's key and certificate if it has none yet.
pub fn ensure(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let (cert, key) = files(dir);
    if cert.is_file() && key.is_file() {
        return Ok(());
    }
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .map_err(|e| format!("making the server's certificate: {e}"))?;
    let tls = dir.join("tls");
    std::fs::create_dir_all(&tls).map_err(|e| format!("{}: {e}", tls.display()))?;
    // The key first, readable by its owner only.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&key)
        .and_then(|mut file| file.write_all(made.signing_key.serialize_pem().as_bytes()))
        .map_err(|e| format!("{}: {e}", key.display()))?;
    std::fs::write(&cert, made.cert.pem()).map_err(|e| format!("{}: {e}", cert.display()))
}

fn certificates(dir: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let (cert, _) = files(dir);
    let found: Result<Vec<_>, _> =
        CertificateDer::pem_file_iter(&cert).map_err(|e| format!("{}: {e}", cert.display()))?.collect();
    let found = found.map_err(|e| format!("{}: {e}", cert.display()))?;
    match found.is_empty() {
        true => Err(format!("{} holds no certificate", cert.display())),
        false => Ok(found),
    }
}

/// The fingerprint clients know the server by: of its own certificate,
/// the first in the file.
pub fn fingerprint(dir: &Path) -> Result<String, String> {
    ensure(dir)?;
    Ok(kitod::tls::fingerprint(certificates(dir)?[0].as_ref()))
}

/// What the server encrypts its connections with.
pub fn config(dir: &Path) -> Result<Arc<ServerConfig>, String> {
    ensure(dir)?;
    let (_, key) = files(dir);
    let key = PrivateKeyDer::from_pem_file(&key).map_err(|e| format!("{}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certificates(dir)?, key)
        .map_err(|e| format!("the server's certificate and key: {e}"))?;
    Ok(Arc::new(config))
}

/// One encrypted connection, read and written by the thread that serves it.
#[derive(Clone)]
pub struct Encrypted(Rc<RefCell<StreamOwned<ServerConnection, TcpStream>>>);

impl Encrypted {
    pub fn new(config: Arc<ServerConfig>, stream: TcpStream) -> Result<Encrypted, String> {
        let connection = ServerConnection::new(config).map_err(|e| e.to_string())?;
        Ok(Encrypted(Rc::new(RefCell::new(StreamOwned::new(connection, stream)))))
    }
}

impl Read for Encrypted {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().read(buffer)
    }
}

impl Write for Encrypted {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}
