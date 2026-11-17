//! Encrypted connections to a server.
//!
//! `kito-server` speaks TLS. A server that people set up for themselves
//! has a certificate it made for itself, which no authority vouches for:
//! a client knows it by its fingerprint (the SHA-256 of the certificate),
//! given to it with its token, as with the key of an SSH host. A server
//! behind a certificate from a public authority is known by its name.
//!
//! A server on this very machine may be reached without a fingerprint:
//! the connection is encrypted, and `kito connect` keeps the fingerprint
//! it saw for the next time.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

/// How a client knows the server it reaches is the one it means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// By the fingerprint of its certificate: `sha256:ab12…`.
    Fingerprint(String),
    /// By its name, vouched for by a public certificate authority.
    Authority,
    /// Not at all: for a server on this machine, the first time.
    Anything,
}

/// The fingerprint of a certificate: `sha256:` and 64 hexadecimal digits.
pub fn fingerprint(certificate: &[u8]) -> String {
    let digest = Sha256::digest(certificate);
    format!("sha256:{}", digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

/// A fingerprint as people type it: with or without `sha256:`, colons
/// between the bytes or not, in either case.
pub fn same_fingerprint(one: &str, other: &str) -> bool {
    let plain = |text: &str| -> String {
        let text = text.trim().to_ascii_lowercase();
        text.strip_prefix("sha256:").unwrap_or(&text).chars().filter(char::is_ascii_hexdigit).collect()
    };
    let (one, other) = (plain(one), plain(other));
    one.len() == 64 && one == other
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Checks the certificate a server presents against a fingerprint, and
/// remembers the fingerprint it saw.
#[derive(Debug)]
struct Pinned {
    /// `None`: any certificate will do.
    pin: Option<String>,
    provider: Arc<CryptoProvider>,
    seen: Seen,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = fingerprint(end_entity.as_ref());
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(presented.clone());
        match &self.pin {
            Some(pin) if !same_fingerprint(pin, &presented) => Err(rustls::Error::General(format!(
                "the server presents a certificate with fingerprint {presented}, not the one this \
                 directory was connected with: either the server has a new certificate (ask whoever \
                 runs it, then connect again with --fingerprint) or this is not the server"
            ))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// One encrypted connection, read and written through the same lock: a
/// client asks and then listens, never both at once.
#[derive(Clone)]
struct Shared(Arc<Mutex<StreamOwned<ClientConnection, TcpStream>>>);

impl Read for Shared {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).read(buffer)
    }
}

impl Write for Shared {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// The two ends of a connection, as a client uses them.
pub type Ends = (Box<dyn BufRead + Send>, Box<dyn Write + Send>);

/// The fingerprint a handshake saw, once it has seen one.
type Seen = Arc<Mutex<Option<String>>>;

fn session(host: &str, trust: &Trust) -> Result<(ClientConnection, Seen), String> {
    let seen: Seen = Arc::new(Mutex::new(None));
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = match trust {
        Trust::Authority => {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        pinned => {
            let pin = match pinned {
                Trust::Fingerprint(pin) => Some(pin.clone()),
                _ => None,
            };
            let verifier = Pinned { pin, provider: provider(), seen: seen.clone() };
            builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth()
        }
    };
    // A server known by its fingerprint is not known by a name: any will do.
    let name = ServerName::try_from(host.to_owned())
        .or_else(|_| ServerName::try_from("localhost".to_owned()))
        .map_err(|e| e.to_string())?;
    let connection = ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    Ok((connection, seen))
}

/// What went wrong with a handshake, with what usually explains it: a
/// server that does not encrypt answers in words, or hangs up.
fn explain(error: std::io::Error) -> String {
    // What this module refuses a certificate with is said as it is.
    let text = error.to_string().replace("unexpected error: ", "");
    let not_tls = error.kind() == std::io::ErrorKind::UnexpectedEof
        || text.contains("InvalidContentType")
        || text.contains("corrupt message");
    match not_tls {
        true => format!(
            "{text}: the server does not seem to encrypt its connections (was it started with --plain? \
             then connect with --plain)"
        ),
        false => text,
    }
}

/// Encrypts a connection to the server at `host`, checked as `trust`
/// says. The handshake is done before this returns.
pub fn connect(stream: TcpStream, host: &str, trust: &Trust) -> Result<Ends, String> {
    let (mut connection, _) = session(host, trust)?;
    let mut stream = stream;
    while connection.is_handshaking() {
        connection.complete_io(&mut stream).map_err(explain)?;
    }
    let shared = Shared(Arc::new(Mutex::new(StreamOwned::new(connection, stream))));
    Ok((Box::new(BufReader::new(shared.clone())), Box::new(shared)))
}

/// The fingerprint of the certificate the server at `host` presents,
/// whatever it is: to show it, or to keep it for a server on this machine.
pub fn presented(stream: TcpStream, host: &str) -> Result<String, String> {
    let (mut connection, seen) = session(host, &Trust::Anything)?;
    let mut stream = stream;
    while connection.is_handshaking() {
        connection.complete_io(&mut stream).map_err(explain)?;
    }
    connection.send_close_notify();
    let _ = connection.complete_io(&mut stream);
    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    seen.ok_or_else(|| "the server presented no certificate".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_is_the_same_however_it_is_typed() {
        let print = fingerprint(b"a certificate");
        assert!(print.starts_with("sha256:") && print.len() == 7 + 64, "{print}");
        let hex = print.strip_prefix("sha256:").unwrap();
        assert!(same_fingerprint(&print, hex));
        assert!(same_fingerprint(&print, &hex.to_uppercase()));
        let colons: Vec<String> =
            hex.as_bytes().chunks(2).map(|pair| String::from_utf8_lossy(pair).into_owned()).collect();
        assert!(same_fingerprint(&print, &format!("SHA256:{}", colons.join(":"))));
        assert!(!same_fingerprint(&print, &fingerprint(b"another")));
        assert!(!same_fingerprint("", ""), "nothing is not a fingerprint");
        assert!(!same_fingerprint(&hex[..10], &hex[..10]));
    }
}
