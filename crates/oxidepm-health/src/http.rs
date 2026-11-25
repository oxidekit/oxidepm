//! The HTTP probe's client: one `GET`, the status line of the answer, nothing else.
//!
//! The daemon keeps no general HTTP library. A probe needs the status code and nothing of the
//! body, redirects are answers (never followed), and the whole exchange runs under the probe's
//! timeout, which the caller sets. `https://` is checked against the public web roots.

use std::sync::{Arc, OnceLock};

use rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// The most of the answer that is read: the status line is the first one, so this is far more
/// than needed, and a server that sends endless header bytes cannot make the probe grow.
const MAX_HEAD: usize = 8 * 1024;

/// A URL taken apart. Only `http` and `https` are accepted.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub tls: bool,
    /// The host without brackets: what is connected to and what the certificate must name.
    pub host: String,
    pub port: u16,
    /// What the request line carries: the path and the query, at least `/`.
    pub path: String,
    /// The `Host` header value (brackets for IPv6, the port when it is not the default).
    pub authority: String,
    /// `user[:password]` of the URL, decoded, sent as basic authentication.
    pub userinfo: Option<String>,
}

pub(crate) fn parse_url(url: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(rest) = strip_prefix_ci(url, "https://") {
        (true, rest)
    } else if let Some(rest) = strip_prefix_ci(url, "http://") {
        (false, rest)
    } else {
        return Err("the URL must start with http:// or https://".to_string());
    };
    let rest = rest.split('#').next().unwrap_or("");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((user, hostport)) => (Some(percent_decode(user)), hostport),
        None => (None, authority),
    };
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let (host, after) = inner
            .split_once(']')
            .ok_or("an IPv6 address is missing its ]")?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| "the port is not a number")?,
            None if after.is_empty() => default_port,
            None => return Err("the address is not valid".to_string()),
        };
        (host.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((host, p)) => (
                host.to_string(),
                p.parse::<u16>().map_err(|_| "the port is not a number")?,
            ),
            None => (hostport.to_string(), default_port),
        }
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the URL has no valid host".to_string());
    }
    // the request line is built from this: no space or control character may get into it
    let path = match tail {
        "" => "/".to_string(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_string(),
    };
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the URL has a space or a control character in it".to_string());
    }
    let shown_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let authority = if port == default_port {
        shown_host
    } else {
        format!("{shown_host}:{port}")
    };
    Ok(Target {
        tls,
        host,
        port,
        path,
        authority,
        userinfo,
    })
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    (text.len() >= prefix.len()
        && text.is_char_boundary(prefix.len())
        && text[..prefix.len()].eq_ignore_ascii_case(prefix))
    .then(|| &text[prefix.len()..])
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(value) = hex {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            Arc::new(
                ClientConfig::builder_with_provider(
                    rustls::crypto::ring::default_provider().into(),
                )
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                ))
                .with_no_client_auth(),
            )
        })
        .clone()
}

/// `GET url` and the status code of the answer. A redirect is returned as it is. There is no
/// time limit here: the caller wraps this in one.
pub(crate) async fn get_status(url: &str) -> Result<u16, String> {
    let target = parse_url(url)?;
    let stream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|e| format!("connection failed: {e}"))?;
    if target.tls {
        let name = ServerName::try_from(target.host.as_str())
            .map_err(|_| "the host is not a valid name for a certificate".to_string())?
            .to_owned();
        let stream = TlsConnector::from(tls_config())
            .connect(name, stream)
            .await
            .map_err(|e| format!("TLS failed: {e}"))?;
        exchange(stream, &target).await
    } else {
        exchange(stream, &target).await
    }
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    target: &Target,
) -> Result<u16, String> {
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: oxidepm-health\r\nAccept: */*\r\nConnection: close\r\n",
        target.path, target.authority
    );
    if let Some(userinfo) = &target.userinfo {
        request.push_str(&format!(
            "Authorization: Basic {}\r\n",
            base64(userinfo.as_bytes())
        ));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("sending failed: {e}"))?;

    let mut head = Vec::with_capacity(256);
    let mut buf = [0u8; 512];
    while !head.contains(&b'\n') && head.len() < MAX_HEAD {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| format!("reading failed: {e}"))?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = head.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let line = String::from_utf8_lossy(line);
    parse_status_line(line.trim_end())
}

fn parse_status_line(line: &str) -> Result<u16, String> {
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") || code.len() != 3 {
        return Err("the answer is not HTTP".to_string());
    }
    code.parse::<u16>()
        .ok()
        .filter(|c| (100..=599).contains(c))
        .ok_or_else(|| "the answer is not HTTP".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_taken_apart() {
        let t = parse_url("http://localhost:3000/health?x=1#frag").unwrap();
        assert_eq!(
            (
                t.tls,
                t.host.as_str(),
                t.port,
                t.path.as_str(),
                t.authority.as_str()
            ),
            (false, "localhost", 3000, "/health?x=1", "localhost:3000")
        );
        let t = parse_url("HTTPS://example.com").unwrap();
        assert_eq!(
            (t.tls, t.port, t.path.as_str(), t.authority.as_str()),
            (true, 443, "/", "example.com")
        );
        let t = parse_url("http://[::1]:8080/h").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.authority.as_str()),
            ("::1", 8080, "[::1]:8080")
        );
        let t = parse_url("http://[::1]/").unwrap();
        assert_eq!((t.port, t.authority.as_str()), (80, "[::1]"));
        let t = parse_url("http://host?a=b").unwrap();
        assert_eq!(t.path, "/?a=b");
        let t = parse_url("https://us%40er:p%3Aw@example.com:8443/h").unwrap();
        assert_eq!(t.userinfo.as_deref(), Some("us@er:p:w"));
        assert_eq!((t.host.as_str(), t.port), ("example.com", 8443));
    }

    #[test]
    fn urls_that_cannot_be_asked_are_errors() {
        for bad in [
            "ftp://x/",
            "http://",
            "http:///p",
            "http://:80/",
            "http://a b/",
            "http://h/pa th",
            "http://h:99999/",
            "http://h:x/",
            "http://[::1/",
            "localhost:80",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_lines() {
        assert_eq!(parse_status_line("HTTP/1.1 200 OK"), Ok(200));
        assert_eq!(parse_status_line("HTTP/1.0 301 Moved Permanently"), Ok(301));
        assert_eq!(parse_status_line("HTTP/1.1 204"), Ok(204));
        for bad in [
            "",
            "SSH-2.0-OpenSSH",
            "HTTP/1.1",
            "HTTP/1.1 20 OK",
            "HTTP/1.1 abc OK",
            "HTTP/1.1 999 Odd",
            "HTTP/1.1 000 Odd",
        ] {
            assert!(parse_status_line(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn base64_matches_the_standard_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }
}
