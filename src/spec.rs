//! The 0.7.0 unified routing spec grammar (`docs/ROUTING-SIMPLIFICATION.md` §5,
//! epic #81):
//!
//! ```text
//! --listen  '<service>/<addr>[,proto=raw][,cert=PATH,key=PATH][,route=request]'
//! --backend '<service>[@<host>]/<target>'
//! ```
//!
//! A listener defaults to auto-detection (classify first bytes; TLS routes by
//! SNI, HTTP/1 by Host, WebSocket upgrades transparently, anything else is an
//! opaque tunnel). The options cover the decisions the wire cannot answer:
//! `proto=raw` skips the peek entirely (server-speaks-first protocols),
//! `cert=`/`key=` mean the bridge terminates TLS there (**cert implies
//! termination** — there is no `tls=` keyword; no cert = passthrough, zero key
//! material), and `route=request` re-routes each HTTP/1.1 request on a
//! keep-alive connection independently.
//!
//! A backend's protocol is a property of its target: `host:port` is TCP,
//! `ws://…`/`wss://…` is WebSocket. The optional `@host` registers
//! `{service}/{host}/available` for hostname routing.
//!
//! Separators are unambiguous by construction: [`validate_service_name`]
//! rejects `@`, `,`, and `/` in service names, and an IPv6 listen address
//! (`[::1]:8080`) contains no comma. Option values may contain `=` (split on
//! the first) but not `,` — a comma in a certificate path is not supported.

use crate::config::validate_service_name;
use crate::dns::normalize_dns;
use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

/// What a listener assumes about incoming bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoMode {
    /// Peek and classify the first bytes; dispatch per protocol.
    Auto,
    /// No peek, no routing key: an opaque L4 tunnel. Required for
    /// server-speaks-first protocols (SMTP, MySQL), where waiting for client
    /// bytes would stall the connection.
    Raw,
}

/// Who holds the TLS private key. Derived, never spelled: `cert=`+`key=`
/// present means the bridge terminates, absent means passthrough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsMode {
    /// TLS flows through encrypted end-to-end; routing uses the cleartext SNI.
    Passthrough,
    /// The bridge is the TLS endpoint; backends receive plaintext.
    Terminate { cert: PathBuf, key: PathBuf },
}

/// Routing granularity for plaintext HTTP/1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMode {
    /// Route once on the first request head, then relay the connection.
    Connection,
    /// Re-evaluate every request's Host on a keep-alive connection
    /// (an L7 proxy: both directions are parsed and framed).
    Request,
}

/// One `--listen` attachment point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenSpec {
    pub service: String,
    pub addr: SocketAddr,
    pub proto: ProtoMode,
    pub tls: TlsMode,
    pub route: RouteMode,
}

/// Where a `--backend` connects when a client appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendTarget {
    Tcp(SocketAddr),
    /// A `ws://` or `wss://` URL, kept verbatim.
    Ws(String),
}

/// One `--backend` attachment point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSpec {
    pub service: String,
    /// Hostname this backend serves, registered as
    /// `{service}/{host}/available`. `None` = the service's only backend.
    pub host: Option<String>,
    pub target: BackendTarget,
}

impl std::fmt::Display for ListenSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.service, self.addr)?;
        if self.proto == ProtoMode::Raw {
            write!(f, ",proto=raw")?;
        }
        if matches!(self.tls, TlsMode::Terminate { .. }) {
            write!(f, ",tls=terminate")?;
        }
        if self.route == RouteMode::Request {
            write!(f, ",route=request")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for BackendSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.host {
            Some(host) => write!(f, "{}@{}", self.service, host)?,
            None => write!(f, "{}", self.service)?,
        }
        match &self.target {
            BackendTarget::Tcp(addr) => write!(f, "/{addr}"),
            BackendTarget::Ws(url) => write!(f, "/{url}"),
        }
    }
}

impl FromStr for ListenSpec {
    type Err = anyhow::Error;

    fn from_str(spec: &str) -> Result<Self> {
        let (service, rest) = spec.split_once('/').ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --listen spec '{spec}': expected \
                 '<service>/<addr>[,proto=raw][,cert=PATH,key=PATH][,route=request]'"
            )
        })?;
        validate_service_name(service)?;

        let mut parts = rest.split(',');
        let addr_part = parts.next().expect("split yields at least one part");
        let addr: SocketAddr = addr_part
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid listen address '{addr_part}': {e}"))?;

        let mut proto = None;
        let mut route = None;
        let mut cert: Option<PathBuf> = None;
        let mut key: Option<PathBuf> = None;

        for opt in parts {
            let (name, value) = opt.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid --listen option '{opt}': expected 'key=value'")
            })?;
            let duplicate = |name: &str| anyhow::anyhow!("duplicate --listen option '{name}='");
            match name {
                "proto" => {
                    if proto.is_some() {
                        return Err(duplicate(name));
                    }
                    proto = Some(match value {
                        "auto" => ProtoMode::Auto,
                        "raw" => ProtoMode::Raw,
                        other => {
                            return Err(anyhow::anyhow!(
                                "invalid proto '{other}': expected 'auto' or 'raw'"
                            ));
                        }
                    });
                }
                "route" => {
                    if route.is_some() {
                        return Err(duplicate(name));
                    }
                    route = Some(match value {
                        "connection" => RouteMode::Connection,
                        "request" => RouteMode::Request,
                        other => {
                            return Err(anyhow::anyhow!(
                                "invalid route '{other}': expected 'connection' or 'request'"
                            ));
                        }
                    });
                }
                "cert" => {
                    if cert.is_some() {
                        return Err(duplicate(name));
                    }
                    cert = Some(PathBuf::from(value));
                }
                "key" => {
                    if key.is_some() {
                        return Err(duplicate(name));
                    }
                    key = Some(PathBuf::from(value));
                }
                other => {
                    return Err(anyhow::anyhow!(
                        "unknown --listen option '{other}=' \
                         (known: proto=, cert=, key=, route=)"
                    ));
                }
            }
        }

        // Cert implies termination; a lone half is a config mistake, not a mode.
        let tls = match (cert, key) {
            (Some(cert), Some(key)) => TlsMode::Terminate { cert, key },
            (None, None) => TlsMode::Passthrough,
            (Some(_), None) => {
                return Err(anyhow::anyhow!("cert= requires key= (and vice versa)"));
            }
            (None, Some(_)) => {
                return Err(anyhow::anyhow!("key= requires cert= (and vice versa)"));
            }
        };
        let proto = proto.unwrap_or(ProtoMode::Auto);
        let route = route.unwrap_or(RouteMode::Connection);

        // The combinations that contradict themselves. proto=raw reads no
        // heads, so it can neither terminate for Host routing nor re-route
        // per request; route=request is the plaintext HTTP/1.1 proxy plane
        // (docs/ROUTING-SIMPLIFICATION.md §5.5), so it cannot terminate.
        if proto == ProtoMode::Raw && route == RouteMode::Request {
            return Err(anyhow::anyhow!(
                "proto=raw and route=request are contradictory: an opaque tunnel \
                 parses no requests"
            ));
        }
        if proto == ProtoMode::Raw && matches!(tls, TlsMode::Terminate { .. }) {
            return Err(anyhow::anyhow!(
                "proto=raw cannot terminate TLS: an opaque tunnel never decrypts; \
                 drop proto=raw or drop cert=/key="
            ));
        }
        if route == RouteMode::Request && matches!(tls, TlsMode::Terminate { .. }) {
            return Err(anyhow::anyhow!(
                "route=request is plaintext-HTTP/1.1-only and cannot terminate TLS"
            ));
        }

        Ok(ListenSpec {
            service: service.to_string(),
            addr,
            proto,
            tls,
            route,
        })
    }
}

impl FromStr for BackendSpec {
    type Err = anyhow::Error;

    fn from_str(spec: &str) -> Result<Self> {
        let (head, target_part) = spec.split_once('/').ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --backend spec '{spec}': expected '<service>[@<host>]/<target>' \
                 where target is 'host:port' or a ws:// / wss:// URL"
            )
        })?;

        // First '@' before the first '/': a ws:// URL's userinfo can never be
        // mistaken for the host separator because the split happens on `head`.
        let (service, host) = match head.split_once('@') {
            Some((service, host)) => {
                if host.is_empty() {
                    return Err(anyhow::anyhow!(
                        "invalid --backend spec '{spec}': '@' with an empty host"
                    ));
                }
                (service, Some(normalize_dns(host)))
            }
            None => (head, None),
        };
        validate_service_name(service)?;
        if let Some(host) = &host
            && host.is_empty()
        {
            return Err(anyhow::anyhow!(
                "invalid --backend spec '{spec}': host normalizes to empty"
            ));
        }

        let target = if target_part.starts_with("ws://") || target_part.starts_with("wss://") {
            BackendTarget::Ws(target_part.to_string())
        } else {
            let addr: SocketAddr = target_part.parse().map_err(|e| {
                anyhow::anyhow!(
                    "invalid backend target '{target_part}': not a socket address ({e}) \
                     and not a ws:// / wss:// URL"
                )
            })?;
            BackendTarget::Tcp(addr)
        };

        Ok(BackendSpec {
            service: service.to_string(),
            host,
            target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ListenSpec ---

    #[test]
    fn listen_minimal_is_auto_passthrough_connection() {
        let s: ListenSpec = "web/0.0.0.0:8080".parse().unwrap();
        assert_eq!(s.service, "web");
        assert_eq!(s.addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(s.proto, ProtoMode::Auto);
        assert_eq!(s.tls, TlsMode::Passthrough);
        assert_eq!(s.route, RouteMode::Connection);
    }

    #[test]
    fn listen_ipv6_address() {
        let s: ListenSpec = "web/[::1]:8080".parse().unwrap();
        assert_eq!(s.addr, "[::1]:8080".parse().unwrap());
    }

    #[test]
    fn listen_ipv6_with_options() {
        let s: ListenSpec = "smtp/[::]:2525,proto=raw".parse().unwrap();
        assert_eq!(s.proto, ProtoMode::Raw);
    }

    #[test]
    fn listen_proto_raw() {
        let s: ListenSpec = "smtp/0.0.0.0:2525,proto=raw".parse().unwrap();
        assert_eq!(s.proto, ProtoMode::Raw);
    }

    #[test]
    fn listen_route_request() {
        let s: ListenSpec = "api/0.0.0.0:8080,route=request".parse().unwrap();
        assert_eq!(s.route, RouteMode::Request);
    }

    #[test]
    fn listen_cert_and_key_imply_termination() {
        let s: ListenSpec = "web/0.0.0.0:8443,cert=/etc/tls/c.pem,key=/etc/tls/k.pem"
            .parse()
            .unwrap();
        match s.tls {
            TlsMode::Terminate { cert, key } => {
                assert_eq!(cert, PathBuf::from("/etc/tls/c.pem"));
                assert_eq!(key, PathBuf::from("/etc/tls/k.pem"));
            }
            TlsMode::Passthrough => panic!("cert+key must imply termination"),
        }
    }

    #[test]
    fn listen_explicit_defaults_accepted() {
        let s: ListenSpec = "web/0.0.0.0:8080,proto=auto,route=connection"
            .parse()
            .unwrap();
        assert_eq!(s.proto, ProtoMode::Auto);
        assert_eq!(s.route, RouteMode::Connection);
    }

    #[test]
    fn listen_option_value_may_contain_equals() {
        // Only the first '=' splits.
        let s: ListenSpec = "web/0.0.0.0:8443,cert=/p/a=b.pem,key=/p/k.pem"
            .parse()
            .unwrap();
        assert!(matches!(s.tls, TlsMode::Terminate { cert, .. }
            if cert.as_os_str() == "/p/a=b.pem"));
    }

    #[test]
    fn listen_cert_without_key_rejected() {
        let err = "web/0.0.0.0:8443,cert=/c.pem"
            .parse::<ListenSpec>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("key="), "{err}");
    }

    #[test]
    fn listen_key_without_cert_rejected() {
        assert!("web/0.0.0.0:8443,key=/k.pem".parse::<ListenSpec>().is_err());
    }

    #[test]
    fn listen_raw_with_request_rejected() {
        let err = "web/0.0.0.0:8080,proto=raw,route=request"
            .parse::<ListenSpec>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("contradictory"), "{err}");
    }

    #[test]
    fn listen_raw_with_cert_rejected() {
        assert!(
            "web/0.0.0.0:8443,proto=raw,cert=/c.pem,key=/k.pem"
                .parse::<ListenSpec>()
                .is_err()
        );
    }

    #[test]
    fn listen_request_with_cert_rejected() {
        let err = "web/0.0.0.0:8443,route=request,cert=/c.pem,key=/k.pem"
            .parse::<ListenSpec>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("plaintext"), "{err}");
    }

    #[test]
    fn listen_unknown_option_rejected() {
        let err = "web/0.0.0.0:8080,tls=terminate"
            .parse::<ListenSpec>()
            .unwrap_err()
            .to_string();
        // The 0.7.0 grammar has no tls= keyword — cert presence implies it.
        assert!(err.contains("unknown"), "{err}");
    }

    #[test]
    fn listen_duplicate_option_rejected() {
        assert!(
            "web/0.0.0.0:8080,proto=raw,proto=raw"
                .parse::<ListenSpec>()
                .is_err()
        );
    }

    #[test]
    fn listen_option_without_value_rejected() {
        assert!("web/0.0.0.0:8080,raw".parse::<ListenSpec>().is_err());
    }

    #[test]
    fn listen_missing_slash_rejected() {
        assert!("just-a-service".parse::<ListenSpec>().is_err());
    }

    #[test]
    fn listen_bad_address_rejected() {
        assert!("web/not-an-addr".parse::<ListenSpec>().is_err());
    }

    #[test]
    fn listen_wildcard_service_rejected() {
        // '*' would become a Zenoh key wildcard.
        assert!("*/0.0.0.0:8080".parse::<ListenSpec>().is_err());
    }

    #[test]
    fn listen_port_bounds() {
        assert!("web/127.0.0.1:0".parse::<ListenSpec>().is_ok());
        assert!("web/127.0.0.1:65535".parse::<ListenSpec>().is_ok());
        assert!("web/127.0.0.1:65536".parse::<ListenSpec>().is_err());
    }

    // --- BackendSpec ---

    #[test]
    fn backend_tcp_minimal() {
        let s: BackendSpec = "web/127.0.0.1:8003".parse().unwrap();
        assert_eq!(s.service, "web");
        assert_eq!(s.host, None);
        assert_eq!(
            s.target,
            BackendTarget::Tcp("127.0.0.1:8003".parse().unwrap())
        );
    }

    #[test]
    fn backend_with_host() {
        let s: BackendSpec = "web@api.example.com/127.0.0.1:8003".parse().unwrap();
        assert_eq!(s.host.as_deref(), Some("api.example.com"));
    }

    #[test]
    fn backend_host_is_normalized() {
        // normalize_dns: lowercase, default ports collapse.
        let s: BackendSpec = "web@API.Example.COM:443/127.0.0.1:8003".parse().unwrap();
        assert_eq!(s.host.as_deref(), Some("api.example.com"));
    }

    #[test]
    fn backend_ws_url() {
        let s: BackendSpec = "chat/ws://127.0.0.1:9000".parse().unwrap();
        assert_eq!(s.target, BackendTarget::Ws("ws://127.0.0.1:9000".into()));
    }

    #[test]
    fn backend_wss_url() {
        let s: BackendSpec = "chat/wss://backend.example:443/path".parse().unwrap();
        assert_eq!(
            s.target,
            BackendTarget::Ws("wss://backend.example:443/path".into())
        );
    }

    #[test]
    fn backend_host_with_ws_url_parses() {
        // Accepted by the grammar; whether the runtime supports it is
        // validated at startup (#75 threads dns through the ws export).
        let s: BackendSpec = "chat@chat.example.com/ws://127.0.0.1:9000".parse().unwrap();
        assert_eq!(s.host.as_deref(), Some("chat.example.com"));
        assert!(matches!(s.target, BackendTarget::Ws(_)));
    }

    #[test]
    fn backend_ws_userinfo_is_not_a_host_separator() {
        // '@' inside the URL part must not be parsed as service@host.
        let s: BackendSpec = "chat/ws://user@127.0.0.1:9000".parse().unwrap();
        assert_eq!(s.host, None);
        assert_eq!(s.service, "chat");
    }

    #[test]
    fn backend_empty_host_rejected() {
        assert!("web@/127.0.0.1:8003".parse::<BackendSpec>().is_err());
    }

    #[test]
    fn backend_bad_target_rejected() {
        let err = "web/http://127.0.0.1:8003"
            .parse::<BackendSpec>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("ws://"), "{err}");
    }

    #[test]
    fn backend_missing_slash_rejected() {
        assert!("just-a-service".parse::<BackendSpec>().is_err());
    }

    #[test]
    fn backend_ipv6_target() {
        let s: BackendSpec = "db/[::1]:5432".parse().unwrap();
        assert_eq!(s.target, BackendTarget::Tcp("[::1]:5432".parse().unwrap()));
    }

    // --- Display round-trips the routing-relevant shape ---

    #[test]
    fn listen_display_shows_options() {
        let s: ListenSpec = "web/0.0.0.0:8443,cert=/c.pem,key=/k.pem".parse().unwrap();
        assert_eq!(s.to_string(), "web/0.0.0.0:8443,tls=terminate");
    }

    #[test]
    fn backend_display_round_trip() {
        for spec in [
            "web/127.0.0.1:8003",
            "web@api.example.com/127.0.0.1:8003",
            "chat/ws://127.0.0.1:9000",
        ] {
            let s: BackendSpec = spec.parse().unwrap();
            assert_eq!(s.to_string(), spec);
        }
    }
}
