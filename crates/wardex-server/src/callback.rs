//! Outbound MCP Events only. Generic operator-configured webhooks retain
//! their existing transport and wire format.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::HeaderMap;
use sha2::Sha256;

pub(crate) struct Post {
    pub verification: bool,
    pub url: String,
    pub body: Vec<u8>,
    pub headers: HeaderMap,
}
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<u64>,
}
pub(crate) type PostFuture = Pin<Box<dyn Future<Output = Result<Reply, &'static str>> + Send>>;
pub(crate) type Transport = Arc<dyn Fn(Post) -> PostFuture + Send + Sync>;

pub(crate) fn signing_key(secret: &str) -> Option<Vec<u8>> {
    let key = STANDARD.decode(secret.strip_prefix("whsec_")?).ok()?;
    (24..=64).contains(&key.len()).then_some(key)
}

pub(crate) fn signature(secret: &str, id: &str, seconds: i64, body: &[u8]) -> Option<String> {
    let key = signing_key(secret)?;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key).ok()?;
    mac.update(format!("{id}.{seconds}.").as_bytes());
    mac.update(body);
    Some(format!(
        "v1,{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}

pub(crate) fn headers(
    id: &str,
    sub: &str,
    seconds: i64,
    body: &[u8],
    secret: &str,
    previous: Option<&str>,
) -> Option<HeaderMap> {
    let mut signature = signature(secret, id, seconds, body)?;
    if let Some(old) = previous {
        signature.push(' ');
        signature.push_str(&self::signature(old, id, seconds, body)?);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json".to_owned()),
        ("webhook-id", id.to_owned()),
        ("webhook-timestamp", seconds.to_string()),
        ("webhook-signature", signature),
        ("x-mcp-subscription-id", sub.to_owned()),
    ] {
        headers.insert(
            reqwest::header::HeaderName::from_static(name),
            value.parse().ok()?,
        );
    }
    Some(headers)
}

pub(crate) fn validate_url(value: &str) -> Result<reqwest::Url, &'static str> {
    if value.len() > 4096 {
        return Err("invalid_url");
    }
    let url = reqwest::Url::parse(value).map_err(|_| "invalid_url")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() == Some(0)
        || url.host_str().is_none()
    {
        return Err("invalid_url");
    }
    let host = url
        .host_str()
        .ok_or("invalid_url")?
        .trim_matches(['[', ']']);
    if let Ok(ip) = host.parse::<IpAddr>()
        && !public_ip(ip)
    {
        return Err("non_public_address");
    }
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return Err("non_public_address");
    }
    Ok(url)
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19)))
}

pub(crate) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = ip.segments();
            // Global unicast only; exclude protocol assignments, documentation,
            // transition/tunnel ranges and all local/unspecified addresses.
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

fn validate_addresses(addresses: &[SocketAddr]) -> Result<(), &'static str> {
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err("non_public_address");
    }
    Ok(())
}

pub(crate) fn transport(timeout: Duration) -> Transport {
    Arc::new(move |post| {
        Box::pin(async move {
            tokio::time::timeout(timeout, send(post, timeout))
                .await
                .map_err(|_| "timeout")?
        })
    })
}

async fn send(post: Post, timeout: Duration) -> Result<Reply, &'static str> {
    let verification = post.verification;
    let url = validate_url(&post.url)?;
    let host = url
        .host_str()
        .ok_or("invalid_url")?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url.port_or_known_default().ok_or("invalid_url")?;
    let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| "dns_error")?
        .collect();
    validate_addresses(&addresses)?;
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(timeout)
        .resolve_to_addrs(&host, &addresses)
        .build()
        .map_err(|_| "client_error")?;
    let mut response = client
        .post(url)
        .headers(post.headers)
        .body(post.body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "connection_failed"
            } else {
                "request_failed"
            }
        })?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let mut body = Vec::new();
    // Only the challenge response is used. Never log or return remote bodies.
    if verification {
        while let Some(chunk) = response.chunk().await.map_err(|_| "response_failed")? {
            if body.len() + chunk.len() > 4096 {
                return Err("response_too_large");
            }
            body.extend_from_slice(&chunk);
        }
    }
    Ok(Reply {
        status,
        body,
        retry_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_addresses_and_parser_bypasses() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
        ] {
            assert!(!public_ip(value.parse().unwrap()), "{value}");
        }
        for value in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(value.parse().unwrap()), "{value}");
        }
        for value in [
            "http://example.com",
            "https://user@example.com",
            "https://example.com/#x",
            "https://127.1",
            "https://2130706433",
            "https://[::ffff:127.0.0.1]",
            "https://localhost",
        ] {
            assert!(validate_url(value).is_err(), "{value}");
        }
    }
    #[test]
    fn standard_webhooks_matches_independent_vector_and_rotation() {
        let key = STANDARD.encode([7_u8; 32]);
        let secret = format!("whsec_{key}");
        let body = br#"{"data":{"value":"test"}}"#;
        let actual = signature(&secret, "evt_test", 1700000000, body).unwrap();
        // Fixed vector calculated with Python's hmac/hashlib, independent of this implementation.
        assert_eq!(actual, "v1,OXws6FGLet3Bjtq8Qn3ODSlHck//SkPZ+QUiaQuaB3Q=");
        let headers = headers(
            "evt_test",
            "sub_test",
            1700000000,
            body,
            &secret,
            Some(&secret),
        )
        .unwrap();
        assert_eq!(
            headers["webhook-signature"]
                .to_str()
                .unwrap()
                .split(' ')
                .count(),
            2
        );
        assert!(signing_key("whsec_bad").is_none());
        assert!(signing_key(&format!("whsec_{}", STANDARD.encode([1; 23]))).is_none());
    }
    #[test]
    fn dns_sets_are_rejected_if_any_answer_is_non_public() {
        let public = "8.8.8.8:443".parse().unwrap();
        let private = "127.0.0.1:443".parse().unwrap();
        assert!(validate_addresses(&[]).is_err());
        assert!(validate_addresses(&[public, private]).is_err());
        assert!(validate_addresses(&[private, public]).is_err());
        assert!(validate_addresses(&[public]).is_ok());
        assert!(validate_addresses(&["[::ffff:127.0.0.1]:443".parse().unwrap()]).is_err());
    }
}
