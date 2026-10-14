//! Peer certificate trust. Input DTLS (listener and outgoing connections),
//! clipboard TLS, pairing and `doctor` probes all verify peers through a
//! [`Policy`]. MiMouse certificates are self-signed, so there is no chain to
//! verify: a peer is its certificate's SHA-256 fingerprint.
//!
//! Incoming connections need [`Policy::Authorized`] (may control this
//! machine). Outgoing connections verify the client they dial: its pinned
//! fingerprint, or any certificate until one is pinned on first use
//! (`ClientManager::server_policy` / `ClientManager::pin`).

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
};

use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{WebPkiSupportedAlgorithms, ring},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use webrtc_dtls::{
    config::{ClientAuthType, Config as DtlsConfig, ExtendedMasterSecretType},
    crypto::Certificate,
};

use super::generate_fingerprint;

/// which peer certificates a connection accepts
#[derive(Clone, Debug)]
pub(crate) enum Policy {
    /// fingerprints in `authorized_fingerprints`: peers that may control
    /// this machine
    Authorized(Arc<RwLock<HashMap<String, String>>>),
    /// exactly the certificate with this fingerprint
    Pinned(String),
    /// any certificate; the caller decides on its fingerprint (pairing
    /// compares codes, trust on first use pins it, `doctor` reports it)
    Any,
}

impl Policy {
    /// Err: why a peer presenting `fingerprint` is refused
    pub(crate) fn check(&self, fingerprint: &str) -> Result<(), String> {
        match self {
            Policy::Authorized(keys) => {
                if keys.read().expect("lock").contains_key(fingerprint) {
                    Ok(())
                } else {
                    Err(format!(
                        "unauthorized certificate fingerprint {fingerprint}"
                    ))
                }
            }
            Policy::Pinned(pin) if pin == fingerprint => Ok(()),
            Policy::Pinned(pin) => Err(format!(
                "certificate fingerprint {fingerprint} is not the pinned {pin}"
            )),
            Policy::Any => Ok(()),
        }
    }
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

/// accepts a chain of exactly one certificate trusted by `policy`; the
/// fingerprint of a refused one is stored in the returned cell
fn dtls_verifier(policy: Policy) -> (VerifyPeerCertificateFn, Arc<OnceLock<String>>) {
    let rejected = Arc::new(OnceLock::new());
    let verify = {
        let rejected = rejected.clone();
        Arc::new(
            move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                let [cert] = certs else {
                    return Err(webrtc_dtls::Error::Other(format!(
                        "expected one certificate, got {}",
                        certs.len()
                    )));
                };
                let fingerprint = generate_fingerprint(cert);
                policy.check(&fingerprint).map_err(|reason| {
                    let _ = rejected.set(fingerprint);
                    webrtc_dtls::Error::Other(reason)
                })
            },
        )
    };
    (verify, rejected)
}

/// DTLS config for accepting peers with `cert`: a peer must present one
/// certificate trusted by `policy`. The returned cell gets the fingerprint of
/// a refused peer.
pub(crate) fn dtls_server_config(
    cert: Certificate,
    policy: Policy,
) -> (DtlsConfig, Arc<OnceLock<String>>) {
    let (verify, rejected) = dtls_verifier(policy);
    let config = DtlsConfig {
        certificates: vec![cert],
        extended_master_secret: ExtendedMasterSecretType::Require,
        client_auth: ClientAuthType::RequireAnyClientCert,
        verify_peer_certificate: Some(verify),
        ..Default::default()
    };
    (config, rejected)
}

/// DTLS config for dialing a server that must present one certificate
/// trusted by `policy`. The returned cell gets the fingerprint of a refused
/// server.
pub(crate) fn dtls_client_config(
    cert: Certificate,
    policy: Policy,
) -> (DtlsConfig, Arc<OnceLock<String>>) {
    let (verify, rejected) = dtls_verifier(policy);
    let config = DtlsConfig {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        // skips only the CA chain check; `verify_peer_certificate` still runs
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        verify_peer_certificate: Some(verify),
        ..Default::default()
    };
    (config, rejected)
}

/// the certificate chain and key of `cert` for rustls; the PEM file uses a
/// nonstandard `PRIVATE_KEY` tag, so take the already parsed certificate
/// (mimouse generates PKCS#8 keys)
fn identity(cert: &Certificate) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let key = PrivatePkcs8KeyDer::from(cert.private_key.serialized_der.clone()).into();
    (cert.certificate.clone(), key)
}

/// TLS 1.3 server config presenting `cert`, requiring a client certificate
/// trusted by `policy`
pub(crate) fn tls_server_config(
    cert: &Certificate,
    policy: Policy,
) -> Result<ServerConfig, rustls::Error> {
    let provider = Arc::new(ring::default_provider());
    let verifier = Arc::new(Verifier {
        policy,
        algorithms: provider.signature_verification_algorithms,
    });
    let (certs, key) = identity(cert);
    ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
}

/// TLS 1.3 client config presenting `cert`, requiring a server certificate
/// trusted by `policy`
pub(crate) fn tls_client_config(
    cert: &Certificate,
    policy: Policy,
) -> Result<ClientConfig, rustls::Error> {
    let provider = Arc::new(ring::default_provider());
    let verifier = Arc::new(Verifier {
        policy,
        algorithms: provider.signature_verification_algorithms,
    });
    let (certs, key) = identity(cert);
    ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
}

/// checks the peer's certificate against a [`Policy`], and the handshake
/// signatures as usual
#[derive(Debug)]
struct Verifier {
    policy: Policy,
    algorithms: WebPkiSupportedAlgorithms,
}

impl Verifier {
    fn check(&self, cert: &CertificateDer) -> Result<(), rustls::Error> {
        self.policy
            .check(&generate_fingerprint(cert))
            .map_err(rustls::Error::General)
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Verifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientManager;
    use futures::future::try_join;
    use std::{
        net::{Ipv4Addr, SocketAddr},
        time::Duration,
    };
    use tokio::net::{TcpListener, TcpStream, UdpSocket};
    use tokio_rustls::{TlsAcceptor, TlsConnector};
    use webrtc_dtls::conn::DTLSConn;

    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    #[derive(Clone, Copy, Debug)]
    enum Transport {
        /// input
        Dtls,
        /// clipboard
        Tls,
    }

    fn certificate() -> Certificate {
        Certificate::generate_self_signed(["ignored".to_owned()]).expect("cert")
    }

    /// a loopback handshake between a server presenting `server` that
    /// accepts `client`'s certificate if authorized in `keys`, and a client
    /// presenting `client` that accepts the server by `policy`: the server's
    /// fingerprint as the client saw it, `None` if either side refused
    async fn handshake(
        transport: Transport,
        server: &Certificate,
        keys: &Arc<RwLock<HashMap<String, String>>>,
        client: &Certificate,
        policy: Policy,
    ) -> Option<String> {
        let authorized = Policy::Authorized(keys.clone());
        let handshake = async {
            match transport {
                Transport::Dtls => dtls(server, authorized, client, policy).await,
                Transport::Tls => tls(server, authorized, client, policy).await,
            }
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .expect("handshake neither completed nor failed")
            .ok()
    }

    async fn dtls(
        server: &Certificate,
        server_policy: Policy,
        client: &Certificate,
        client_policy: Policy,
    ) -> Result<String, String> {
        let server_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        server_socket
            .connect(client_socket.local_addr().unwrap())
            .await
            .unwrap();
        client_socket
            .connect(server_socket.local_addr().unwrap())
            .await
            .unwrap();
        let (server_config, _) = dtls_server_config(server.clone(), server_policy);
        let (client_config, _) = dtls_client_config(client.clone(), client_policy);
        let accept = async {
            DTLSConn::new(Arc::new(server_socket), server_config, false, None)
                .await
                .map_err(|e| format!("server: {e}"))
        };
        let connect = async {
            DTLSConn::new(Arc::new(client_socket), client_config, true, None)
                .await
                .map_err(|e| format!("client: {e}"))
        };
        let (server, client) = try_join(accept, connect).await?;
        let state = client.connection_state().await;
        let _ = client.close().await;
        let _ = server.close().await;
        let [cert] = &state.peer_certificates[..] else {
            return Err("not one server certificate".into());
        };
        Ok(generate_fingerprint(cert))
    }

    async fn tls(
        server: &Certificate,
        server_policy: Policy,
        client: &Certificate,
        client_policy: Policy,
    ) -> Result<String, String> {
        let server_config = tls_server_config(server, server_policy).unwrap();
        let client_config = tls_client_config(client, client_policy).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = async {
            let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            TlsAcceptor::from(Arc::new(server_config))
                .accept(stream)
                .await
                .map_err(|e| format!("server: {e}"))
        };
        let connect = async {
            let stream = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
            let name = ServerName::try_from("mimouse").unwrap();
            TlsConnector::from(Arc::new(client_config))
                .connect(name, stream)
                .await
                .map_err(|e| format!("client: {e}"))
        };
        let (_server, client) = try_join(accept, connect).await?;
        client
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| generate_fingerprint(c))
            .ok_or("no server certificate".into())
    }

    /// N3 critical-path gate: every policy over input DTLS and clipboard TLS
    /// with real loopback handshakes, accepting or refusing as expected
    #[tokio::test]
    async fn trust_matrix() {
        let (client, server, other) = (certificate(), certificate(), certificate());
        let fingerprint = super::super::certificate_fingerprint;
        let authorized = Arc::new(RwLock::new(HashMap::from([(
            fingerprint(&client),
            "client".to_owned(),
        )])));
        let unknown = Arc::new(RwLock::new(HashMap::new()));
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 4242));

        for transport in [Transport::Dtls, Transport::Tls] {
            let accepts = |keys, policy| handshake(transport, &server, keys, &client, policy);

            // Authorized: the server accepts the client by its key
            let seen = accepts(&authorized, Policy::Any).await;
            assert_eq!(seen, Some(fingerprint(&server)), "{transport:?} authorized");
            let seen = accepts(&unknown, Policy::Any).await;
            assert_eq!(seen, None, "{transport:?} unauthorized");

            // Pinned: the client accepts only the pinned server
            let seen = accepts(&authorized, Policy::Pinned(fingerprint(&server))).await;
            assert!(seen.is_some(), "{transport:?} pinned match");
            let seen = accepts(&authorized, Policy::Pinned(fingerprint(&other))).await;
            assert_eq!(seen, None, "{transport:?} pinned mismatch");

            // unpinned: the first server is pinned, another one then refused
            let clients = ClientManager::default();
            let handle = clients.add_client();
            let first = handle_policy(&clients, handle);
            let seen = accepts(&authorized, first).await;
            let seen = seen.expect("unpinned server refused");
            assert!(clients.pin(handle, addr, &seen), "{transport:?} TOFU first");
            assert_eq!(
                clients.get_state(handle).unwrap().0.fingerprint,
                Some(fingerprint(&server))
            );
            let second = handle_policy(&clients, handle);
            let seen = handshake(transport, &other, &authorized, &client, second).await;
            assert_eq!(seen, None, "{transport:?} TOFU second certificate");
            assert!(!clients.pin(handle, addr, &fingerprint(&other)));
        }
    }

    fn handle_policy(clients: &ClientManager, handle: u64) -> Policy {
        clients.server_policy(handle).expect("client exists")
    }

    #[test]
    fn policies() {
        let (known, unknown) = ("aa:01", "bb:02");
        let keys = HashMap::from([(known.to_owned(), "k".to_owned())]);
        let authorized = Policy::Authorized(Arc::new(RwLock::new(keys)));
        assert!(authorized.check(known).is_ok());
        assert!(authorized.check(unknown).is_err());
        let pinned = Policy::Pinned(known.into());
        assert!(pinned.check(known).is_ok());
        assert!(pinned.check(unknown).is_err());
        assert!(Policy::Any.check(unknown).is_ok());
    }

    #[test]
    fn dtls_verifier_rejects_chains_without_exactly_one_cert() {
        let (known, unknown) = (vec![1u8; 32], vec![2u8; 32]);
        let (verify, rejected) = dtls_verifier(Policy::Pinned(generate_fingerprint(&known)));

        assert!(verify(&[], &[]).is_err());
        assert!(verify(&[known.clone(), unknown.clone()], &[]).is_err());
        assert_eq!(rejected.get(), None);

        // the verifier stays usable for well-formed chains
        assert!(verify(&[known], &[]).is_ok());
        assert!(verify(std::slice::from_ref(&unknown), &[]).is_err());
        assert_eq!(rejected.get(), Some(&generate_fingerprint(&unknown)));
    }
}
