//! Proxy support for the hand-rolled WebSocket connection in [`crate::listen`].
//!
//! Every other subcommand goes through reqwest, which reads the usual proxy
//! environment variables on its own. `listen` opens its own socket, so without
//! this it would be the one command that ignores them: on a host that can only
//! reach the API through a proxy, `taskshoot task show` would work while
//! `taskshoot listen` retried forever.
//!
//! What is supported is an HTTP proxy tunnelling with `CONNECT`, which is what
//! `HTTP(S)_PROXY` means in practice. A proxy that is itself reached over TLS,
//! or a SOCKS one, is refused with a clear error rather than quietly bypassed
//! -- reqwest without its `socks` feature does not speak SOCKS either, so
//! failing keeps the two halves of the CLI consistent.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};

use anyhow::{bail, Context, Result};
use percent_encoding::percent_decode_str;

/// Largest `CONNECT` response accepted before giving up on the proxy.
///
/// A proxy answers with a status line and a few headers; anything past this is
/// a peer that will never send the blank line this waits for.
const MAX_RESPONSE: usize = 8 * 1024;

/// A proxy to tunnel through, already resolved from the environment.
#[derive(Debug, PartialEq, Eq)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    /// `Proxy-Authorization` value, from the userinfo in the proxy URL.
    pub auth: Option<String>,
    /// The variable it came from, so the log can say why a proxy is in use.
    pub source: String,
}

/// Pick the proxy for `host` from `lookup`, or `None` to connect directly.
///
/// `secure` selects the variable pair the way reqwest and curl do: `wss` reads
/// `HTTPS_PROXY`, `ws` reads `http_proxy`, and `ALL_PROXY` backs both up.
///
/// `http_proxy` is read in lower case only, which is deliberate rather than an
/// oversight: in a CGI environment `HTTP_PROXY` is attacker-controlled (it is
/// just the `Proxy:` request header), and honouring it is the httpoxy
/// vulnerability. The `https` variants have no such header behind them.
pub fn select(
    secure: bool,
    host: &str,
    port: u16,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<Proxy>> {
    let names: &[&str] = if secure {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["http_proxy", "ALL_PROXY", "all_proxy"]
    };
    let found = names.iter().find_map(|name| match lookup(name) {
        // An empty value is how a wrapper script turns an inherited proxy
        // off; treating it as a URL would just fail to parse.
        Some(value) if !value.trim().is_empty() => Some((*name, value)),
        _ => None,
    });
    let Some((name, value)) = found else {
        return Ok(None);
    };
    if bypasses(
        host,
        port,
        lookup("NO_PROXY").or_else(|| lookup("no_proxy")),
    ) {
        return Ok(None);
    }
    let mut proxy = parse(value.trim()).with_context(|| format!("{name} is not a usable proxy"))?;
    proxy.source = name.to_string();
    Ok(Some(proxy))
}

/// Whether `NO_PROXY` exempts this host and port.
///
/// The syntax is the de facto one, read the way reqwest reads it so that the
/// two halves of the CLI agree on which hosts skip the proxy: a comma separated
/// list where `*` means every host, a name entry matches that name or any
/// subdomain of it (a leading dot means the same thing), and an entry carrying
/// a port only matches that port.
///
/// An entry that is an IP address is matched exactly rather than by suffix. As
/// a name, `2.3.4` would "match" `1.2.3.4`, which is not a subdomain relation
/// at all -- it would exempt an unrelated host from the proxy.
fn bypasses(host: &str, port: u16, no_proxy: Option<String>) -> bool {
    let Some(list) = no_proxy else {
        return false;
    };
    let host = host.trim_matches(|c| c == '[' || c == ']').to_lowercase();
    let host_ip = host.parse::<IpAddr>().ok();
    list.split(',').any(|entry| {
        let entry = entry.trim().to_lowercase();
        if entry == "*" {
            return true;
        }
        let (entry_host, entry_port) = split_no_proxy_entry(&entry);
        if entry_port.is_some_and(|wanted| wanted != port) {
            return false;
        }
        let entry_host = entry_host.trim_start_matches('.');
        if entry_host.is_empty() {
            return false;
        }
        match (host_ip, entry_host.parse::<IpAddr>()) {
            // Compared as addresses, so the several spellings of one IPv6
            // address (`::1`, `0:0:0:0:0:0:0:1`) all match.
            (Some(host_ip), Ok(entry_ip)) => host_ip == entry_ip,
            (Some(_), Err(_)) | (None, Ok(_)) => false,
            (None, Err(_)) => host == entry_host || host.ends_with(&format!(".{entry_host}")),
        }
    })
}

/// Split one `NO_PROXY` entry into its host and, if it has one, its port.
///
/// A bracketed IPv6 literal keeps its address whole, and an unbracketed one
/// (which `NO_PROXY` lists are often written with) is left alone rather than
/// having its last group read as a port.
fn split_no_proxy_entry(entry: &str) -> (&str, Option<u16>) {
    if let Some(after_bracket) = entry.strip_prefix('[') {
        if let Some((host, rest)) = after_bracket.split_once(']') {
            return (host, rest.strip_prefix(':').and_then(|p| p.parse().ok()));
        }
        return (after_bracket, None);
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host, Some(port)),
            Err(_) => (entry, None),
        },
        _ => (entry, None),
    }
}

/// Read a proxy URL. A bare `host:port` is accepted, as curl accepts it.
fn parse(value: &str) -> Result<Proxy> {
    let (scheme, rest) = match value.split_once("://") {
        Some((scheme, rest)) => (scheme.to_lowercase(), rest),
        None => ("http".to_string(), value),
    };
    match scheme.as_str() {
        "http" => {}
        // Bypassing the proxy silently would be worse than failing: the whole
        // point of the setting is that the direct route does not work.
        "https" => bail!("a TLS connection to the proxy itself is not supported"),
        other => bail!("unsupported proxy scheme {other}"),
    }
    // Trim the path and query: only the authority is used to open the tunnel.
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let (userinfo, authority) = match authority.rsplit_once('@') {
        Some((userinfo, authority)) => (Some(userinfo), authority),
        None => (None, authority),
    };
    if authority.is_empty() {
        bail!("the proxy URL has no host");
    }
    let (host, port) = split_authority(authority)?;
    Ok(Proxy {
        host,
        // The scheme's default, which is what reqwest uses for a proxy URL
        // without a port. (curl would say 1080 here; matching the other half of
        // this CLI matters more than matching curl.)
        port: port.unwrap_or(80),
        auth: userinfo.map(basic_auth).transpose()?,
        source: String::new(),
    })
}

/// Split `host:port`, keeping an IPv6 literal's brackets out of the host.
fn split_authority(authority: &str) -> Result<(String, Option<u16>)> {
    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, rest) = after_bracket
            .split_once(']')
            .context("the proxy URL has an unterminated IPv6 literal")?;
        // Anything between the literal and the port is a typo in a setting that
        // decides where credentials get sent, so it is worth failing over.
        if !rest.is_empty() && !rest.starts_with(':') {
            bail!("the proxy URL has trailing text after its IPv6 literal");
        }
        (host, rest.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        bail!("the proxy URL has no host");
    }
    let port = match port {
        Some(port) => Some(
            port.parse()
                .with_context(|| format!("malformed proxy port {port:?}"))?,
        ),
        None => None,
    };
    Ok((host.to_string(), port))
}

/// `Basic <base64>` for the userinfo of a proxy URL.
///
/// The userinfo is percent-encoded in the URL and sent decoded. The user and
/// the password are decoded separately, because the colon between them is
/// structure: a password containing one arrives as `%3A` and has to stay part
/// of the password rather than splitting it again.
fn basic_auth(userinfo: &str) -> Result<String> {
    let (user, password) = match userinfo.split_once(':') {
        Some((user, password)) => (user, Some(password)),
        None => (userinfo, None),
    };
    let mut credentials = decode(user)?;
    if let Some(password) = password {
        credentials.push(':');
        credentials.push_str(&decode(password)?);
    }
    Ok(format!("Basic {}", base64(credentials.as_bytes())))
}

/// Percent-decode one userinfo field, refusing anything that is not text.
///
/// A credential that does not decode is a mistake worth reporting: sending the
/// undecoded bytes instead would just fail authentication somewhere less
/// obvious.
fn decode(field: &str) -> Result<String> {
    percent_decode_str(field)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .with_context(|| "the proxy URL's credentials are not valid UTF-8".to_string())
}

/// Standard base64, spelled out rather than pulled in as a dependency for the
/// one credential header this crate sends.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let bits = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(bits >> (18 - 6 * i)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Ask an already connected proxy to tunnel to `host:port`.
///
/// The stream is left exactly at the first byte after the proxy's response, so
/// the caller can start its TLS handshake on it.
pub fn tunnel(stream: &mut TcpStream, proxy: &Proxy, host: &str, port: u16) -> Result<()> {
    // An IPv6 literal has to go back inside brackets here, or the colons in it
    // read as the port separator.
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = &proxy.auth {
        request.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .context("cannot send CONNECT to the proxy")?;
    stream.flush().context("cannot send CONNECT to the proxy")?;

    let response = read_headers(stream)?;
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .context("the proxy did not answer CONNECT with a status line")?;
    if !(200..300).contains(&status) {
        bail!("the proxy refused CONNECT to {target} with status {status}");
    }
    Ok(())
}

/// Read up to the blank line that ends the proxy's response headers.
///
/// One byte at a time so that not a single byte of what follows is consumed:
/// the caller's TLS handshake starts there.
fn read_headers(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => bail!("the proxy closed the connection during CONNECT"),
            Ok(_) => buf.push(byte[0]),
            Err(e) => return Err(e).context("cannot read the proxy's CONNECT response"),
        }
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
            break;
        }
        if buf.len() > MAX_RESPONSE {
            bail!("the proxy's CONNECT response has no end of headers");
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env lookup backed by a list, so no test has to touch the process
    /// environment (which every other test would see too).
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn wss_prefers_https_proxy_and_ws_never_reads_uppercase_http_proxy() {
        let vars = env(&[
            ("HTTPS_PROXY", "http://secure:3128"),
            ("HTTP_PROXY", "http://attacker:3128"),
            ("ALL_PROXY", "http://fallback:3128"),
        ]);
        let secure = select(true, "api.example.com", 443, &vars)
            .unwrap()
            .unwrap();
        assert_eq!((secure.host.as_str(), secure.port), ("secure", 3128));
        assert_eq!(secure.source, "HTTPS_PROXY");

        // httpoxy: upper case HTTP_PROXY can be a request header, so a plain ws
        // target falls through to ALL_PROXY instead of trusting it.
        let plain = select(false, "api.example.com", 80, &vars)
            .unwrap()
            .unwrap();
        assert_eq!(plain.host, "fallback");
        assert_eq!(plain.source, "ALL_PROXY");
    }

    #[test]
    fn no_proxy_exempts_the_host_and_its_subdomains() {
        let vars = env(&[
            ("ALL_PROXY", "http://proxy:3128"),
            ("NO_PROXY", "localhost, .example.com:443"),
        ]);
        let bypassed = |host, port| select(true, host, port, &vars).unwrap().is_none();
        assert!(bypassed("api.example.com", 443));
        assert!(bypassed("example.com", 443));
        assert!(bypassed("localhost", 8443));
        assert!(!bypassed("notexample.com", 443));
        // The entry named a port, so another one is not exempt.
        assert!(!bypassed("api.example.com", 8443));

        let all = env(&[("ALL_PROXY", "http://proxy:3128"), ("no_proxy", "*")]);
        assert!(select(true, "api.example.com", 443, &all)
            .unwrap()
            .is_none());
    }

    #[test]
    fn no_proxy_matches_addresses_exactly_and_literals_in_any_spelling() {
        // `2.3.4` is not a parent domain of `1.2.3.4`; reading it as a suffix
        // would exempt an unrelated host from the proxy.
        let suffix = env(&[("ALL_PROXY", "http://proxy:3128"), ("NO_PROXY", "2.3.4")]);
        assert!(select(true, "1.2.3.4", 443, &suffix).unwrap().is_some());

        let exact = env(&[("ALL_PROXY", "http://proxy:3128"), ("NO_PROXY", "1.2.3.4")]);
        assert!(select(true, "1.2.3.4", 443, &exact).unwrap().is_none());

        let v6 = env(&[
            ("ALL_PROXY", "http://proxy:3128"),
            ("NO_PROXY", "[0:0:0:0:0:0:0:1]:443"),
        ]);
        assert!(select(true, "::1", 443, &v6).unwrap().is_none());
        assert!(select(true, "::1", 8443, &v6).unwrap().is_some());

        // Unbracketed, which NO_PROXY lists are often written with: the last
        // group must not be read as a port.
        let bare_v6 = env(&[("ALL_PROXY", "http://proxy:3128"), ("NO_PROXY", "::1")]);
        assert!(select(true, "::1", 443, &bare_v6).unwrap().is_none());
    }

    #[test]
    fn an_empty_variable_means_no_proxy() {
        let vars = env(&[("HTTPS_PROXY", "  "), ("ALL_PROXY", "")]);
        assert_eq!(select(true, "api.example.com", 443, &vars).unwrap(), None);
    }

    #[test]
    fn a_proxy_url_may_carry_credentials_a_bare_authority_or_neither() {
        let bare = parse("proxy.internal:3128").unwrap();
        assert_eq!(
            (bare.host.as_str(), bare.port, bare.auth),
            ("proxy.internal", 3128, None)
        );

        // The http scheme's default, as reqwest resolves it.
        let default_port = parse("http://proxy.internal").unwrap();
        assert_eq!(default_port.port, 80);

        let with_auth = parse("http://user:p%40ss@proxy.internal:8888/path").unwrap();
        assert_eq!(with_auth.host, "proxy.internal");
        assert_eq!(with_auth.port, 8888);
        // base64("user:p@ss")
        assert_eq!(with_auth.auth.as_deref(), Some("Basic dXNlcjpwQHNz"));

        // Lower case escapes, a space, and an encoded colon inside the password
        // (which must not split the credentials again). base64("a b:p:ss")
        let escapes = parse("http://a%20b:p%3ass@proxy.internal:8888").unwrap();
        assert_eq!(escapes.auth.as_deref(), Some("Basic YSBiOnA6c3M="));

        let v6 = parse("http://[::1]:3128").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 3128));
    }

    #[test]
    fn a_proxy_that_cannot_be_tunnelled_through_is_an_error_not_a_bypass() {
        // Silently connecting directly would defeat the setting: the direct
        // route is exactly what does not work when a proxy is configured.
        assert!(parse("https://proxy.internal:3128").is_err());
        assert!(parse("socks5://proxy.internal:1080").is_err());
        assert!(parse("http://").is_err());
        // A typo between the literal and the port decides where credentials go,
        // so it is reported rather than read as a bare address.
        assert!(parse("http://[::1]garbage").is_err());
        assert!(parse("http://[::1:3128").is_err());
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// A proxy that answers `CONNECT` with `status`, reporting what it was
    /// asked for and what it saw straight after the blank line.
    fn fake_proxy(
        status: &'static str,
        trailer: &'static [u8],
    ) -> (u16, std::thread::JoinHandle<(String, Vec<u8>)>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if peer.read(&mut byte).unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            peer.write_all(status.as_bytes()).unwrap();
            peer.write_all(trailer).unwrap();
            peer.flush().unwrap();
            // Whatever the client sends next: it must not have eaten it while
            // reading the response headers.
            let mut after = [0u8; 8];
            let read = peer.read(&mut after).unwrap_or(0);
            (
                String::from_utf8_lossy(&request).into_owned(),
                after[..read].to_vec(),
            )
        });
        (port, handle)
    }

    #[test]
    fn tunnel_sends_connect_and_leaves_the_stream_at_the_payload() {
        let (port, proxy_side) = fake_proxy("HTTP/1.1 200 Connection Established\r\n\r\n", b"");
        let proxy = Proxy {
            host: "127.0.0.1".to_string(),
            port,
            auth: Some("Basic dXNlcjpwYXNz".to_string()),
            source: "HTTPS_PROXY".to_string(),
        };
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        tunnel(&mut stream, &proxy, "api.example.com", 443).unwrap();
        // The handshake the caller writes next has to start at the byte after
        // the proxy's blank line, so nothing may have been read ahead.
        stream.write_all(b"HELLO").unwrap();
        stream.flush().unwrap();

        let (request, after) = proxy_side.join().unwrap();
        assert!(
            request.starts_with("CONNECT api.example.com:443 HTTP/1.1\r\n"),
            "unexpected CONNECT request: {request}"
        );
        assert!(request.contains("Host: api.example.com:443\r\n"));
        assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
        assert_eq!(&after, b"HELLO");
    }

    #[test]
    fn a_refused_tunnel_reports_the_status() {
        let (port, _proxy_side) =
            fake_proxy("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n", b"");
        let proxy = Proxy {
            host: "127.0.0.1".to_string(),
            port,
            auth: None,
            source: "HTTPS_PROXY".to_string(),
        };
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let error = format!(
            "{:#}",
            tunnel(&mut stream, &proxy, "api.example.com", 443).unwrap_err()
        );
        assert!(error.contains("407"), "unexpected error: {error}");
    }
}
