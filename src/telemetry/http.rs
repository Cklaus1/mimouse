//! The collector endpoint and the one request shape telemetry needs: an
//! HTTP/1.1 request over TLS (`tokio-rustls`, ring) to a base URL, answered
//! by a status code. No proxy support.

use std::{fmt, sync::Arc, time::Duration};

use futures::future::LocalBoxFuture;
use rustls::{ClientConfig, RootCertStore, crypto::ring};
use rustls_pki_types::ServerName;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

/// connect, handshake, request and response together
pub(super) const TIMEOUT: Duration = Duration::from_secs(10);
/// longest status line read
const MAX_STATUS_LINE: usize = 1024;

/// an `https://host[:port][/path]` base URL; requests append their route
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    url: String,
    /// `host[:port]` as given, the `Host` header
    authority: String,
    /// without IPv6 brackets
    host: String,
    port: u16,
    /// without a trailing `/`
    path: String,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}

impl Endpoint {
    /// a base URL; anything but `https://` is refused, so nothing is ever
    /// sent in plain text
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let invalid = |why: &str| format!("telemetry endpoint {url:?}: {why}");
        let rest = url
            .get(..8)
            .filter(|scheme| scheme.eq_ignore_ascii_case("https://"))
            .map(|_| &url[8..])
            .ok_or_else(|| invalid("only https:// is allowed"))?;
        if rest.contains(['?', '#', ' ']) {
            return Err(invalid("no query, fragment or spaces allowed"));
        }
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        if authority.contains('@') {
            return Err(invalid("no user name allowed"));
        }
        let (host, port) = match authority.strip_prefix('[') {
            Some(v6) => {
                let (host, port) = v6.split_once(']').ok_or_else(|| invalid("unclosed ["))?;
                (host, port.strip_prefix(':'))
            }
            None => match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            },
        };
        let port = match port {
            Some(port) => port.parse().map_err(|_| invalid("invalid port"))?,
            None => 443,
        };
        if host.is_empty() || ServerName::try_from(host).is_err() {
            return Err(invalid("invalid host"));
        }
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            authority: authority.to_owned(),
            host: host.to_owned(),
            port,
            path: path.trim_end_matches('/').to_owned(),
        })
    }
}

/// a request to `route` below the endpoint
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub method: &'static str,
    /// `/v1/batch`
    pub route: String,
    /// sent as `application/json`
    pub body: Option<Vec<u8>>,
}

impl Request {
    /// the bytes on the wire
    fn encode(&self, endpoint: &Endpoint) -> Vec<u8> {
        let mut head = format!(
            "{} {}{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.method, endpoint.path, self.route, endpoint.authority
        );
        let body = self.body.as_deref().unwrap_or_default();
        if self.body.is_some() {
            head += "Content-Type: application/json\r\n";
        }
        head += &format!("Content-Length: {}\r\n\r\n", body.len());
        [head.as_bytes(), body].concat()
    }
}

/// sends a request, returning the response status
pub(super) trait Transport {
    fn send(
        &self,
        endpoint: Endpoint,
        request: Request,
    ) -> LocalBoxFuture<'static, Result<u16, String>>;
}

/// HTTPS verified against the system's root certificates
#[derive(Clone, Default)]
pub(super) struct Https {
    /// trusted instead of the system roots
    #[cfg(test)]
    pub roots: Option<Arc<RootCertStore>>,
}

impl Transport for Https {
    fn send(
        &self,
        endpoint: Endpoint,
        request: Request,
    ) -> LocalBoxFuture<'static, Result<u16, String>> {
        let https = self.clone();
        Box::pin(async move {
            match timeout(TIMEOUT, https.exchange(&endpoint, &request)).await {
                Ok(result) => result,
                Err(_) => Err(format!("no answer within {} s", TIMEOUT.as_secs())),
            }
        })
    }
}

impl Https {
    async fn exchange(&self, endpoint: &Endpoint, request: &Request) -> Result<u16, String> {
        let config = self.config().await?;
        let name = ServerName::try_from(endpoint.host.clone()).map_err(|e| e.to_string())?;
        let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let mut tls = TlsConnector::from(config)
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        tls.write_all(&request.encode(endpoint))
            .await
            .map_err(|e| format!("send: {e}"))?;
        tls.flush().await.map_err(|e| format!("send: {e}"))?;
        let mut head = Vec::new();
        let mut buf = [0u8; 256];
        while !head.windows(2).any(|w| w == b"\r\n") {
            let n = tls
                .read(&mut buf)
                .await
                .map_err(|e| format!("receive: {e}"))?;
            if n == 0 || head.len() > MAX_STATUS_LINE {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        status(&head).ok_or_else(|| "invalid HTTP response".to_owned())
    }

    async fn config(&self) -> Result<Arc<ClientConfig>, String> {
        #[cfg(test)]
        let roots = match &self.roots {
            Some(roots) => roots.clone(),
            None => native_roots().await?,
        };
        #[cfg(not(test))]
        let roots = native_roots().await?;
        let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Arc::new(config))
    }
}

/// the system's root certificates (blocking: reads files or the keychain)
async fn native_roots() -> Result<Arc<RootCertStore>, String> {
    tokio::task::spawn_blocking(|| {
        let loaded = rustls_native_certs::load_native_certs();
        for e in &loaded.errors {
            log::debug!("telemetry: root certificates: {e}");
        }
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(loaded.certs);
        match roots.is_empty() {
            true => Err("no system root certificates found".to_owned()),
            false => Ok(Arc::new(roots)),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// the status code of `HTTP/1.x NNN ...`
fn status(head: &[u8]) -> Option<u16> {
    let line = head.split(|&b| b == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split(' ');
    parts.next()?.strip_prefix("HTTP/1.")?;
    let code = parts.next()?;
    (code.len() == 3).then_some(())?;
    code.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_base_urls_are_accepted() {
        let e = Endpoint::parse("https://t.example.org/base/").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            ("t.example.org", 443, "/base")
        );
        assert_eq!(e.to_string(), "https://t.example.org/base");
        let e = Endpoint::parse("HTTPS://[::1]:8443").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            ("::1", 8443, "")
        );
        assert_eq!(e.authority, "[::1]:8443");
        for bad in [
            "http://t.example.org",
            "t.example.org",
            "https://",
            "https://user@t.example.org",
            "https://t.example.org:99999",
            "https://t.example.org/?x=1",
            "ftp://t.example.org",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn request_appends_the_route_to_the_base_path() {
        let endpoint = Endpoint::parse("https://t.example.org:8443/mimouse").unwrap();
        let request = Request {
            method: "POST",
            route: "/v1/batch".into(),
            body: Some(b"{}".to_vec()),
        };
        assert_eq!(
            String::from_utf8(request.encode(&endpoint)).unwrap(),
            "POST /mimouse/v1/batch HTTP/1.1\r\nHost: t.example.org:8443\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
        );
        let delete = Request {
            method: "DELETE",
            route: "/v1/installs/x".into(),
            body: None,
        };
        let encoded = String::from_utf8(delete.encode(&endpoint)).unwrap();
        assert!(encoded.starts_with("DELETE /mimouse/v1/installs/x HTTP/1.1\r\n"));
        assert!(encoded.ends_with("Content-Length: 0\r\n\r\n"));
        assert!(!encoded.contains("Content-Type"));
    }

    #[test]
    fn status_line_is_parsed() {
        assert_eq!(status(b"HTTP/1.1 204 No Content\r\n"), Some(204));
        assert_eq!(status(b"HTTP/1.0 500 x\r\nmore"), Some(500));
        assert_eq!(status(b"HTTP/2 204\r\n"), None);
        assert_eq!(status(b"garbage"), None);
        assert_eq!(status(b"HTTP/1.1 20\r\n"), None);
    }
}
