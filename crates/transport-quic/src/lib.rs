// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use quickfs_protocol::{
    ALPN_PROTOCOL, CodecError, Envelope, PROTOCOL_MAJOR, Request, Response, decode_frame,
    encode_frame, parse_frame_header, version_major,
};
use quinn::{Connection, Endpoint, TransportConfig, VarInt};
pub use quinn::{RecvStream, SendStream};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject};
use rustls_platform_verifier::BuilderVerifierExt;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const MAX_CERTIFICATE_BUNDLE_SIZE: u64 = 4 * 1024 * 1024;
pub const MAX_PRIVATE_KEY_SIZE: u64 = 64 * 1024;
const MAX_CERTIFICATES_PER_BUNDLE: usize = 4096;
const FILESYSTEM_IDLE_TIMEOUT_MILLIS: u32 = 5 * 60 * 1_000;
const CLIENT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const STREAM_RECEIVE_WINDOW: u32 = 32 * 1024 * 1024;
const CONNECTION_RECEIVE_WINDOW: u32 = 128 * 1024 * 1024;
const CONNECTION_SEND_WINDOW: u64 = 128 * 1024 * 1024;
const SERVER_CONCURRENT_BIDI_STREAMS: u32 = 256;

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
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("timeout")]
    Timeout,
}
pub struct QuicClient {
    endpoint: Endpoint,
    connection: Connection,
    timeout: Duration,
    /// Whether outbound frames may be compressed. Off until the `Hello`/`HelloAck`
    /// handshake learns the peer advertises at least `MINOR_FRAME_COMPRESSION`, so
    /// the handshake itself and any peer too old to decompress are never sent a
    /// compressed frame. Decoding inbound compressed frames is always supported
    /// and does not consult this.
    compression: AtomicBool,
    /// The peer's exact wire version, learned from `HelloAck` during negotiation
    /// (0 until then). Lets the client gate optional minor capabilities beyond
    /// compression — e.g. [`quickfs_protocol::peer_supports_metadata_batch`] —
    /// without threading the version through every call site.
    server_version: AtomicU16,
}
impl QuicClient {
    pub async fn connect(
        server: SocketAddr,
        server_name: &str,
        certificate: CertificateDer<'static>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::connect_with_ca(server, server_name, vec![certificate], timeout).await
    }

    /// Connect using a deployed private-CA bundle. Standard X.509 chain,
    /// validity, key-usage, and server-name validation all remain enabled.
    pub async fn connect_with_ca(
        server: SocketAddr,
        server_name: &str,
        authorities: Vec<CertificateDer<'static>>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        install_crypto_provider();
        if authorities.is_empty() {
            return Err(TransportError::Tls(
                "certificate-authority bundle is empty".into(),
            ));
        }
        if authorities.len() > MAX_CERTIFICATES_PER_BUNDLE {
            return Err(TransportError::Tls(
                "certificate-authority bundle contains too many certificates".into(),
            ));
        }
        let mut roots = rustls::RootCertStore::empty();
        for authority in authorities {
            roots
                .add(authority)
                .map_err(|error| TransportError::Tls(error.to_string()))?;
        }
        let crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Self::connect_with_crypto(server, server_name, crypto, timeout).await
    }

    /// Connect using the operating system's native trust policy. This is the
    /// public-PKI path and also supports enterprise roots installed through
    /// MDM, Group Policy, or another platform-management mechanism.
    pub async fn connect_with_system_roots(
        server: SocketAddr,
        server_name: &str,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        install_crypto_provider();
        let crypto = rustls::ClientConfig::builder()
            .with_platform_verifier()
            .map_err(|error| TransportError::Tls(error.to_string()))?
            .with_no_client_auth();
        Self::connect_with_crypto(server, server_name, crypto, timeout).await
    }

    pub async fn connect_pinned(
        server: SocketAddr,
        server_name: &str,
        fingerprint: [u8; 32],
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::connect_with_verifier(server, server_name, Some(fingerprint), timeout).await
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
        Self::connect_with_crypto(server, server_name, crypto, timeout).await
    }

    async fn connect_with_crypto(
        server: SocketAddr,
        server_name: &str,
        mut crypto: rustls::ClientConfig,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        crypto.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
        let mut config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                .map_err(|error| TransportError::Tls(error.to_string()))?,
        ));
        config.transport_config(filesystem_transport_config(true, false));
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
            compression: AtomicBool::new(false),
            server_version: AtomicU16::new(0),
        })
    }

    /// Enable or disable outbound frame compression. The client sets this once,
    /// after `HelloAck` reveals the server's version, via
    /// [`quickfs_protocol::peer_supports_frame_compression`].
    pub fn set_compression(&self, enabled: bool) {
        self.compression.store(enabled, Ordering::Relaxed);
    }

    /// Record the peer's exact wire version, learned from `HelloAck`. The client
    /// sets this once during negotiation alongside [`Self::set_compression`].
    pub fn set_server_version(&self, version: u16) {
        self.server_version.store(version, Ordering::Relaxed);
    }

    /// The peer's wire version as learned from `HelloAck`, or 0 before negotiation.
    /// Used to gate optional minor capabilities the client may emit.
    pub fn server_version(&self) -> u16 {
        self.server_version.load(Ordering::Relaxed)
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
        Ok(certificate_sha256(certificate))
    }
    pub async fn stream(&self) -> Result<(SendStream, RecvStream), TransportError> {
        Ok(
            tokio::time::timeout(self.timeout, self.connection.open_bi())
                .await
                .map_err(|_| TransportError::Timeout)??,
        )
    }
    pub async fn send_frame<T: Serialize>(
        &self,
        send: &mut SendStream,
        value: &T,
    ) -> Result<(), TransportError> {
        let compress = self.compression.load(Ordering::Relaxed);
        tokio::time::timeout(self.timeout, write_frame(send, value, compress))
            .await
            .map_err(|_| TransportError::Timeout)?
    }
    pub async fn send_all(&self, send: &mut SendStream, data: &[u8]) -> Result<(), TransportError> {
        tokio::time::timeout(self.timeout, send.write_all(data))
            .await
            .map_err(|_| TransportError::Timeout)??;
        Ok(())
    }
    pub async fn receive_frame<T: DeserializeOwned>(
        &self,
        recv: &mut RecvStream,
    ) -> Result<T, TransportError> {
        tokio::time::timeout(self.timeout, read_frame(recv))
            .await
            .map_err(|_| TransportError::Timeout)?
    }
    pub async fn receive_exact(
        &self,
        recv: &mut RecvStream,
        data: &mut [u8],
    ) -> Result<(), TransportError> {
        tokio::time::timeout(self.timeout, recv.read_exact(data))
            .await
            .map_err(|_| TransportError::Timeout)??;
        Ok(())
    }
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn close(&self) {
        self.connection.close(0u32.into(), b"client shutdown");
        let _ = &self.endpoint;
    }
}

/// A deliberately restricted connection that accepts an initially untrusted
/// certificate but can issue only the pairing protocol request. Keeping this
/// separate from `QuicClient` makes it impossible for callers to send login
/// credentials over the certificate-accepting transport through this API.
pub struct PairingClient {
    inner: QuicClient,
}

impl PairingClient {
    pub async fn connect(
        server: SocketAddr,
        server_name: &str,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            inner: QuicClient::connect_with_verifier(server, server_name, None, timeout).await?,
        })
    }

    pub fn peer_certificate_fingerprint(&self) -> Result<[u8; 32], TransportError> {
        self.inner.peer_certificate_fingerprint()
    }

    pub async fn pair(
        &self,
        pairing_id: Uuid,
        client_nonce: [u8; 32],
        client_proof: [u8; 32],
    ) -> Result<Response, TransportError> {
        let mut request = Envelope::new(Request::Pair {
            pairing_id,
            client_nonce,
            client_proof: client_proof.into(),
        });
        let (mut send, mut recv) = self.inner.stream().await?;
        let write_result = self.inner.send_frame(&mut send, &request).await;
        request.message.clear_secrets();
        write_result?;
        send.finish()?;
        let response: Envelope<Response> = self.inner.receive_frame(&mut recv).await?;
        if version_major(response.version) != PROTOCOL_MAJOR {
            return Err(TransportError::Protocol(format!(
                "unsupported response protocol major version {}",
                version_major(response.version)
            )));
        }
        if response.request_id != request.request_id {
            return Err(TransportError::Protocol(
                "pairing response request identifier did not match".into(),
            ));
        }
        Ok(response.message)
    }

    pub fn close(&self) {
        self.inner.close();
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
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Endpoint, TransportError> {
    install_crypto_provider();
    let crypto = server_crypto_config(certificates, key)?;
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .map_err(|error| TransportError::Tls(error.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(filesystem_transport_config(false, true));
    Ok(Endpoint::server(config, bind)?)
}

fn filesystem_transport_config(
    client_keep_alive: bool,
    accept_filesystem_streams: bool,
) -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config
        .max_idle_timeout(Some(quinn::IdleTimeout::from(VarInt::from_u32(
            FILESYSTEM_IDLE_TIMEOUT_MILLIS,
        ))))
        .stream_receive_window(VarInt::from_u32(STREAM_RECEIVE_WINDOW))
        .receive_window(VarInt::from_u32(CONNECTION_RECEIVE_WINDOW))
        .send_window(CONNECTION_SEND_WINDOW);
    if client_keep_alive {
        config.keep_alive_interval(Some(CLIENT_KEEP_ALIVE_INTERVAL));
    }
    if accept_filesystem_streams {
        config.max_concurrent_bidi_streams(VarInt::from_u32(SERVER_CONCURRENT_BIDI_STREAMS));
    }
    Arc::new(config)
}

/// Validate that a PEM-derived certificate chain and private key form a
/// usable QUIC/TLS server identity without binding a network socket.
pub fn validate_server_identity(
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<(), TransportError> {
    install_crypto_provider();
    let crypto = server_crypto_config(certificates, key)?;
    quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .map_err(|error| TransportError::Tls(error.to_string()))?;
    Ok(())
}

fn server_crypto_config(
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<rustls::ServerConfig, TransportError> {
    if certificates.is_empty() {
        return Err(TransportError::Tls(
            "server certificate chain is empty".into(),
        ));
    }
    if certificates.len() > MAX_CERTIFICATES_PER_BUNDLE {
        return Err(TransportError::Tls(
            "server certificate chain contains too many certificates".into(),
        ));
    }
    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| TransportError::Tls(error.to_string()))?;
    crypto.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
    Ok(crypto)
}

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
pub fn certificate_sha256(certificate: &CertificateDer<'_>) -> [u8; 32] {
    Sha256::digest(certificate.as_ref()).into()
}
pub fn load_certificate(path: &Path) -> Result<CertificateDer<'static>, TransportError> {
    load_certificates(path)?
        .into_iter()
        .next()
        .ok_or_else(|| TransportError::Tls("certificate bundle is empty".into()))
}
pub fn load_certificate_pem(path: &Path) -> Result<Vec<u8>, TransportError> {
    read_bounded_file(path, MAX_CERTIFICATE_BUNDLE_SIZE, "certificate bundle")
}
pub fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TransportError> {
    let pem = load_certificate_pem(path)?;
    parse_certificates_pem(&pem)
}
pub fn parse_certificates_pem(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TransportError> {
    validate_buffer_size(pem.len(), MAX_CERTIFICATE_BUNDLE_SIZE, "certificate bundle")?;
    let certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TransportError::Tls(error.to_string()))?;
    if certificates.is_empty() {
        return Err(TransportError::Tls("certificate bundle is empty".into()));
    }
    if certificates.len() > MAX_CERTIFICATES_PER_BUNDLE {
        return Err(TransportError::Tls(
            "certificate bundle contains too many certificates".into(),
        ));
    }
    Ok(certificates)
}
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TransportError> {
    let pem = load_private_key_pem(path)?;
    parse_private_key_pem(&pem)
}
pub fn load_private_key_pem(path: &Path) -> Result<Zeroizing<Vec<u8>>, TransportError> {
    Ok(Zeroizing::new(read_bounded_file(
        path,
        MAX_PRIVATE_KEY_SIZE,
        "private key",
    )?))
}
pub fn parse_private_key_pem(pem: &[u8]) -> Result<PrivateKeyDer<'static>, TransportError> {
    validate_buffer_size(pem.len(), MAX_PRIVATE_KEY_SIZE, "private key")?;
    PrivateKeyDer::from_pem_slice(pem).map_err(|error| TransportError::Tls(error.to_string()))
}

fn read_bounded_file(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>, TransportError> {
    let initial = fs::metadata(path)?;
    if !initial.is_file() {
        return Err(TransportError::Tls(format!(
            "{label} is not a regular file"
        )));
    }
    validate_declared_size(initial.len(), maximum, label)?;

    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(TransportError::Tls(format!(
            "{label} is not a regular file"
        )));
    }
    validate_declared_size(opened.len(), maximum, label)?;

    let mut data = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut data)?;
    validate_buffer_size(data.len(), maximum, label)?;
    Ok(data)
}

fn validate_declared_size(size: u64, maximum: u64, label: &str) -> Result<(), TransportError> {
    if size == 0 {
        return Err(TransportError::Tls(format!("{label} is empty")));
    }
    if size > maximum {
        return Err(TransportError::Tls(format!(
            "{label} exceeds the {maximum}-byte safety limit"
        )));
    }
    Ok(())
}

fn validate_buffer_size(data_len: usize, maximum: u64, label: &str) -> Result<(), TransportError> {
    let size = u64::try_from(data_len)
        .map_err(|_| TransportError::Tls(format!("{label} size does not fit u64")))?;
    if size == 0 {
        return Err(TransportError::Tls(format!("{label} is empty")));
    }
    if size > maximum {
        return Err(TransportError::Tls(format!(
            "{label} exceeds the {maximum}-byte safety limit"
        )));
    }
    Ok(())
}
pub async fn write_frame<T: Serialize>(
    send: &mut SendStream,
    value: &T,
    compress: bool,
) -> Result<(), TransportError> {
    let (prefix, body) = encode_frame(value, compress)?;
    let body = Zeroizing::new(body);
    send.write_all(&prefix.to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T, TransportError> {
    let mut prefix = [0; 4];
    recv.read_exact(&mut prefix).await?;
    let (compressed, size) = parse_frame_header(u32::from_be_bytes(prefix))?;
    let mut body = Zeroizing::new(vec![0; size]);
    recv.read_exact(&mut body).await?;
    Ok(decode_frame(compressed, &body)?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn identity_inputs_are_regular_nonempty_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("empty.pem");
        fs::write(&empty, []).unwrap();
        assert!(load_certificate_pem(&empty).is_err());
        assert!(load_private_key_pem(&empty).is_err());
        assert!(load_certificate_pem(directory.path()).is_err());

        let oversized_certificate = directory.path().join("oversized-certificate.pem");
        fs::File::create(&oversized_certificate)
            .unwrap()
            .set_len(MAX_CERTIFICATE_BUNDLE_SIZE + 1)
            .unwrap();
        assert!(load_certificate_pem(&oversized_certificate).is_err());

        let oversized_key = directory.path().join("oversized-key.pem");
        fs::File::create(&oversized_key)
            .unwrap()
            .set_len(MAX_PRIVATE_KEY_SIZE + 1)
            .unwrap();
        assert!(load_private_key_pem(&oversized_key).is_err());
    }
}
