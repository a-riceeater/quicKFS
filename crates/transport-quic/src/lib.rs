// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use quickfs_protocol::{CodecError, MAX_FRAME_SIZE, decode, encode};
use quinn::{Connection, Endpoint};
pub use quinn::{RecvStream, SendStream};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::BufReader,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};
use zeroize::Zeroize;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("QUIC connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("QUIC write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("QUIC read: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("stream closed: {0}")]
    Closed(#[from] quinn::ClosedStream),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("TLS: {0}")]
    Tls(String),
    #[error("timeout")]
    Timeout,
}
pub struct QuicClient {
    endpoint: Endpoint,
    connection: Connection,
    timeout: Duration,
}
impl QuicClient {
    pub async fn connect(
        server: SocketAddr,
        server_name: &str,
        certificate: CertificateDer<'static>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        install_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(certificate)
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        let crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        ));
        let mut endpoint = Endpoint::client(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))?;
        endpoint.set_default_client_config(config);
        let connection = tokio::time::timeout(
            timeout,
            endpoint
                .connect(server, server_name)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        )
        .await
        .map_err(|_| TransportError::Timeout)??;
        Ok(Self {
            endpoint,
            connection,
            timeout,
        })
    }

    pub async fn connect_pinned(
        server: SocketAddr,
        server_name: &str,
        fingerprint: [u8; 32],
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::connect_with_verifier(server, server_name, Some(fingerprint), timeout).await
    }

    /// Opens a connection that accepts the presented certificate only so an
    /// authenticated pairing proof can bind it to an out-of-band secret.
    /// Callers must not send credentials or filesystem requests on this mode.
    pub async fn connect_for_pairing(
        server: SocketAddr,
        server_name: &str,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::connect_with_verifier(server, server_name, None, timeout).await
    }

    async fn connect_with_verifier(
        server: SocketAddr,
        server_name: &str,
        fingerprint: Option<[u8; 32]>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        install_crypto_provider();
        let verifier = FingerprintVerifier::new(fingerprint);
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        let config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                .map_err(|error| TransportError::Tls(error.to_string()))?,
        ));
        let mut endpoint = Endpoint::client(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))?;
        endpoint.set_default_client_config(config);
        let connecting = endpoint
            .connect(server, server_name)
            .map_err(|error| TransportError::Tls(error.to_string()))?;
        let connection = tokio::time::timeout(timeout, connecting)
            .await
            .map_err(|_| TransportError::Timeout)??;
        Ok(Self {
            endpoint,
            connection,
            timeout,
        })
    }

    pub fn peer_certificate_fingerprint(&self) -> Result<[u8; 32], TransportError> {
        let identity = self
            .connection
            .peer_identity()
            .ok_or_else(|| TransportError::Tls("server did not present a certificate".into()))?;
        let certificates = identity
            .downcast::<Vec<CertificateDer<'static>>>()
            .map_err(|_| TransportError::Tls("unexpected server identity type".into()))?;
        let certificate = certificates
            .first()
            .ok_or_else(|| TransportError::Tls("server certificate chain is empty".into()))?;
        Ok(Sha256::digest(certificate.as_ref()).into())
    }
    pub async fn stream(&self) -> Result<(SendStream, RecvStream), TransportError> {
        Ok(
            tokio::time::timeout(self.timeout, self.connection.open_bi())
                .await
                .map_err(|_| TransportError::Timeout)??,
        )
    }
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn close(&self) {
        self.connection.close(0u32.into(), b"client shutdown");
        let _ = &self.endpoint;
    }
}

#[derive(Debug)]
struct FingerprintVerifier {
    expected: Option<[u8; 32]>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl FingerprintVerifier {
    fn new(expected: Option<[u8; 32]>) -> Arc<Self> {
        Arc::new(Self {
            expected,
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        })
    }
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(expected) = self.expected {
            let actual: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
            if actual != expected {
                return Err(rustls::Error::General(
                    "server certificate does not match the pinned identity".into(),
                ));
            }
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
pub fn server_endpoint(
    bind: SocketAddr,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<Endpoint, TransportError> {
    install_crypto_provider();
    let config = quinn::ServerConfig::with_single_cert(vec![cert], key)
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    Ok(Endpoint::server(config, bind)?)
}

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
pub fn load_certificate(path: &Path) -> Result<CertificateDer<'static>, TransportError> {
    let mut r = BufReader::new(File::open(path)?);
    rustls_pemfile::certs(&mut r)
        .next()
        .ok_or_else(|| TransportError::Tls("certificate missing".into()))?
        .map_err(TransportError::Io)
}
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TransportError> {
    let mut r = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut r)?
        .ok_or_else(|| TransportError::Tls("private key missing".into()))
}
pub async fn write_frame<T: Serialize>(
    send: &mut SendStream,
    value: &T,
) -> Result<(), TransportError> {
    let mut data = encode(value)?;
    send.write_all(&(data.len() as u32).to_be_bytes()).await?;
    send.write_all(&data).await?;
    data.zeroize();
    Ok(())
}
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T, TransportError> {
    let mut size = [0; 4];
    recv.read_exact(&mut size).await?;
    let size = u32::from_be_bytes(size) as usize;
    if size > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(size).into());
    }
    let mut data = vec![0; size];
    recv.read_exact(&mut data).await?;
    Ok(decode(&data)?)
}
