//! One small blocking HTTP client for eep and eep-build (ureq + rustls).
//! Bodies sent with `Content-Encoding: zstd` (the cache's `.ls` files) are
//! decoded here, so callers always get plain bytes.

use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Largest body read into memory (listings, narinfos, indexes).
const MEM_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Http {
    redirects: u32,
    /// Built on first use: picking the IP family costs one TCP connect.
    agent: OnceLock<ureq::Agent>,
}

/// Response metadata eep cares about.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub location: Option<String>,
    pub content_length: Option<u64>,
}

#[derive(Debug)]
pub enum Conditional {
    NotModified,
    Missing,
    Body(Vec<u8>, Meta),
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        Self::with_redirects(10)
    }

    /// `0` → redirects are returned to the caller (see [`Meta::location`]).
    pub fn with_redirects(max: u32) -> Self {
        Self {
            redirects: max,
            agent: OnceLock::new(),
        }
    }

    fn agent(&self) -> &ureq::Agent {
        self.agent.get_or_init(|| {
            ureq::Agent::config_builder()
                .user_agent(concat!("eep/", env!("CARGO_PKG_VERSION")))
                .http_status_as_error(false)
                .max_redirects(self.redirects)
                .ip_family(ip_family())
                .timeout_connect(Some(Duration::from_secs(20)))
                .timeout_recv_response(Some(Duration::from_secs(60)))
                // a stalled body must not hold eep's lock forever (the
                // biggest piece is ~1 GB: generous even on a slow link)
                .timeout_recv_body(Some(Duration::from_secs(45 * 60)))
                .build()
                .into()
        })
    }

    /// GET into memory. `Ok(None)` for 404/403 (S3 answers 403 for a
    /// missing key); any other non-2xx is an error.
    pub fn get_bytes(&self, url: &str) -> Result<Option<Vec<u8>>> {
        match self.conditional(url, None, None)? {
            Conditional::Body(b, _) => Ok(Some(b)),
            Conditional::Missing => Ok(None),
            Conditional::NotModified => bail!("{url}: unexpected 304"),
        }
    }

    /// GET with `If-None-Match` / `If-Modified-Since`.
    pub fn conditional(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<Conditional> {
        let mut req = self.agent().get(url);
        if let Some(e) = etag {
            req = req.header("If-None-Match", e);
        }
        if let Some(l) = last_modified {
            req = req.header("If-Modified-Since", l);
        }
        let mut resp = req.call().with_context(|| format!("GET {url}"))?;
        let meta = meta_of(&resp);
        match meta.status {
            304 => return Ok(Conditional::NotModified),
            403 | 404 => return Ok(Conditional::Missing),
            200..=299 => {}
            s => bail!("GET {url}: HTTP {s}"),
        }
        let zstd = is_zstd(&resp);
        let raw = resp
            .body_mut()
            .with_config()
            .limit(MEM_LIMIT)
            .read_to_vec()
            .with_context(|| format!("reading {url}"))?;
        let body = if zstd {
            zstd::decode_all(raw.as_slice()).context("zstd body")?
        } else {
            raw
        };
        Ok(Conditional::Body(body, meta))
    }

    /// GET as a stream (for NARs: hundreds of MB, never held in memory).
    pub fn get_reader(&self, url: &str) -> Result<Option<(Box<dyn Read + Send>, Meta)>> {
        let resp = self
            .agent()
            .get(url)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let meta = meta_of(&resp);
        match meta.status {
            403 | 404 => return Ok(None),
            200..=299 => {}
            s => bail!("GET {url}: HTTP {s}"),
        }
        let zstd = is_zstd(&resp);
        let reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
        if zstd {
            return Ok(Some((Box::new(zstd::Decoder::new(reader)?), meta)));
        }
        Ok(Some((reader, meta)))
    }

    pub fn head(&self, url: &str) -> Result<Meta> {
        let resp = self
            .agent()
            .head(url)
            .call()
            .with_context(|| format!("HEAD {url}"))?;
        Ok(meta_of(&resp))
    }
}

fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn is_zstd(resp: &ureq::http::Response<ureq::Body>) -> bool {
    header(resp, "content-encoding").is_some_and(|v| v.eq_ignore_ascii_case("zstd"))
}

fn meta_of(resp: &ureq::http::Response<ureq::Body>) -> Meta {
    Meta {
        status: resp.status().as_u16(),
        etag: header(resp, "etag"),
        last_modified: header(resp, "last-modified"),
        location: header(resp, "location"),
        content_length: header(resp, "content-length").and_then(|v| v.parse().ok()),
    }
}

/// IPv4 only when IPv6 is broken here, otherwise both.
///
/// ureq tries addresses one after another, and a black-holed IPv6 route
/// (common on home links) costs a full connect timeout per new host. So,
/// once per process, race a TCP connect over each family to the cache
/// (the idea of RFC 8305 "happy eyeballs"): if IPv4 connects and IPv6
/// hasn't within 300 ms, use IPv4 only. `EEP_IP_FAMILY=4|6|any` overrides.
fn ip_family() -> IpFamily {
    static FAMILY: OnceLock<IpFamily> = OnceLock::new();
    *FAMILY.get_or_init(|| match std::env::var("EEP_IP_FAMILY").as_deref() {
        Ok("4") => IpFamily::Ipv4Only,
        Ok("6") => IpFamily::Ipv6Only,
        Ok("any") => IpFamily::Any,
        _ => race("cache.nixos.org:443"),
    })
}

use ureq::config::IpFamily;

fn race(host: &str) -> IpFamily {
    let Ok(addrs) = host.to_socket_addrs() else {
        return IpFamily::Any;
    };
    let addrs: Vec<SocketAddr> = addrs.collect();
    let (Some(v4), Some(v6)) = (
        addrs.iter().find(|a| a.is_ipv4()).copied(),
        addrs.iter().find(|a| a.is_ipv6()).copied(),
    ) else {
        return IpFamily::Any; // only one family: nothing to choose
    };
    let (tx, rx) = mpsc::channel();
    for addr in [v4, v6] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let ok = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok();
            let _ = tx.send((addr.is_ipv6(), ok));
        });
    }
    drop(tx);
    let mut v4_ok = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        let wait = if v4_ok {
            Duration::from_millis(300)
        } else {
            deadline.saturating_duration_since(std::time::Instant::now())
        };
        match rx.recv_timeout(wait) {
            Ok((true, true)) => return IpFamily::Any, // IPv6 works
            Ok((false, true)) => v4_ok = true,
            Ok((_, false)) => {}
            Err(_) => {
                return if v4_ok {
                    IpFamily::Ipv4Only
                } else {
                    IpFamily::Any
                };
            }
        }
    }
}
