//! A minimal HTTP readiness probe.
//!
//! Deliberately hand-rolled rather than pulling in a full HTTP client: the job
//! is to learn whether a service answers, which needs the status line and
//! nothing else. A client crate would add dozens of dependencies and, in the
//! common default configuration, link against OpenSSL — which would cost the
//! single self-contained binary that the packaging story depends on.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("{0} is not a valid URL")]
    InvalidUrl(String),
    #[error("only http:// is supported, not {scheme}://")]
    UnsupportedScheme { scheme: String },
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("no HTTP status line in the response")]
    Malformed,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The parts of a URL this probe needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Path and query, ready to place in the request line.
    pub path: String,
}

/// Split an `http://` URL into host, port and path.
pub fn parse_url(url: &str) -> Result<Target, ProbeError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| ProbeError::InvalidUrl(url.to_string()))?;
    if !scheme.eq_ignore_ascii_case("http") {
        return Err(ProbeError::UnsupportedScheme {
            scheme: scheme.to_string(),
        });
    }

    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(ProbeError::InvalidUrl(url.to_string()));
    }
    // Credentials are not supported; a readiness probe should not carry any.
    let authority = authority.rsplit('@').next().unwrap_or(authority);

    let invalid = || ProbeError::InvalidUrl(url.to_string());

    // An IPv6 literal is bracketed, and is full of colons, so the port cannot
    // be found by splitting on the last one.
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(invalid)?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse::<u16>().map_err(|_| invalid())?,
            None if tail.is_empty() => 80,
            None => return Err(invalid()),
        };
        (host, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().map_err(|_| invalid())?),
            None => (authority, 80),
        }
    };

    if host.is_empty() {
        return Err(invalid());
    }

    Ok(Target {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// Issue one `GET` and return the HTTP status code.
pub async fn status_of(url: &str, timeout: Duration) -> Result<u16, ProbeError> {
    let target = parse_url(url)?;
    tokio::time::timeout(timeout, request(&target))
        .await
        .map_err(|_| ProbeError::Timeout(timeout))?
}

async fn request(target: &Target) -> Result<u16, ProbeError> {
    let address = format!("{}:{}", target.host, target.port);
    let mut stream = TcpStream::connect(&address)
        .await
        .map_err(|error| ProbeError::Connect(error.to_string()))?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: suede/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        target.path,
        target.host,
        crate::VERSION,
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    // The status line is all that matters, so stop as soon as it is complete.
    let mut buffer = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.contains(&b'\n') || buffer.len() > 8192 {
            break;
        }
    }

    parse_status(&buffer)
}

fn parse_status(response: &[u8]) -> Result<u16, ProbeError> {
    let text = String::from_utf8_lossy(response);
    let line = text.lines().next().ok_or(ProbeError::Malformed)?;
    let mut parts = line.split_whitespace();
    let version = parts.next().ok_or(ProbeError::Malformed)?;
    if !version.starts_with("HTTP/") {
        return Err(ProbeError::Malformed);
    }
    parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or(ProbeError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_a_plain_url() {
        let target = parse_url("http://127.0.0.1:8080/healthz").unwrap();
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8080);
        assert_eq!(target.path, "/healthz");
    }

    #[test]
    fn defaults_the_port_and_path() {
        let target = parse_url("http://example.com").unwrap();
        assert_eq!(target.port, 80);
        assert_eq!(target.path, "/");
    }

    #[test]
    fn keeps_the_query_string() {
        let target = parse_url("http://host/ready?deep=1&x=2").unwrap();
        assert_eq!(target.path, "/ready?deep=1&x=2");
    }

    #[test]
    fn handles_an_ipv6_literal() {
        let target = parse_url("http://[::1]:9000/up").unwrap();
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, 9000);
    }

    #[test]
    fn rejects_https_clearly() {
        // Better an explicit error than a silent failure to ever become ready.
        let error = parse_url("https://example.com/").unwrap_err();
        assert!(matches!(error, ProbeError::UnsupportedScheme { .. }));
        assert!(error.to_string().contains("only http://"));
    }

    #[test]
    fn rejects_nonsense() {
        for url in ["", "example.com/ready", "http://", "http://host:notaport/"] {
            assert!(parse_url(url).is_err(), "{url} should not parse");
        }
    }

    #[test]
    fn parses_status_lines() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n\r\n").unwrap(), 200);
        assert_eq!(
            parse_status(b"HTTP/1.0 503 Service Unavailable\r\n").unwrap(),
            503
        );
        assert!(parse_status(b"garbage\r\n").is_err());
        assert!(parse_status(b"").is_err());
    }

    /// A one-shot server that answers with `status`, for probe tests.
    async fn serve_once(status: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await;
            }
        });
        format!("http://{address}/ready")
    }

    #[tokio::test]
    async fn reads_a_real_success() {
        let url = serve_once("200 OK").await;
        assert_eq!(status_of(&url, Duration::from_secs(5)).await.unwrap(), 200);
    }

    #[tokio::test]
    async fn reads_a_real_failure_status() {
        let url = serve_once("503 Service Unavailable").await;
        assert_eq!(status_of(&url, Duration::from_secs(5)).await.unwrap(), 503);
    }

    #[tokio::test]
    async fn a_closed_port_never_reports_ready() {
        // Nothing is listening here; this is exactly what "the service is not
        // up yet" looks like. What matters is that it is an error rather than
        // a status code, so an app keeps waiting instead of launching early.
        // The precise variant varies by platform, so it is not asserted.
        let result = status_of("http://127.0.0.1:1/ready", Duration::from_secs(5)).await;
        assert!(result.is_err(), "a closed port must not yield a status");
    }

    #[tokio::test]
    async fn a_silent_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // Accept but never answer, so the probe must give up on its own.
        tokio::spawn(async move {
            let _keep = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let error = status_of(&format!("http://{address}/"), Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(error, ProbeError::Timeout(_)));
    }
}
