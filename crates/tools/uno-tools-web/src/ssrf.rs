use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Проверяет, что URL использует `http`/`https` и не указывает на локальные или приватные сети
pub fn validate_public_url(raw: &str, allow_private: bool) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|err| format!("invalid URL: {err}"))?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("unsupported URL scheme `{scheme}`")),
    }

    let Some(host) = url.host() else {
        return Err("URL has no host".to_owned());
    };

    if allow_private {
        return Ok(url);
    }

    match host {
        Host::Domain(domain) => {
            let lower = domain.trim_end_matches('.').to_ascii_lowercase();
            if is_blocked_domain(&lower) {
                return Err(format!("blocked private/local host `{domain}`"));
            }
        }
        Host::Ipv4(ip) => {
            if is_private_ipv4(ip) {
                return Err(format!("blocked private/local IPv4 `{ip}`"));
            }
        }
        Host::Ipv6(ip) => {
            if is_private_ipv6(ip) {
                return Err(format!("blocked private/local IPv6 `{ip}`"));
            }
        }
    }

    Ok(url)
}

fn is_blocked_domain(lower: &str) -> bool {
    lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower == "localtest.me"
        || lower.ends_with(".localtest.me")
        || lower == "lvh.me"
        || lower.ends_with(".lvh.me")
        || lower.ends_with(".nip.io")
        || lower.ends_with(".sslip.io")
        || lower.ends_with(".xip.io")
}

/// Проверяет, что все разрешённые IP-адреса хоста являются публичными (защита от DNS Rebinding)
pub fn validate_resolved_ips(
    host: &str,
    ips: impl IntoIterator<Item = IpAddr>,
    allow_private: bool,
) -> Result<(), String> {
    if allow_private {
        return Ok(());
    }
    let mut count = 0usize;
    for ip in ips {
        count += 1;
        if !is_public_ip(ip) {
            return Err(format!(
                "blocked host `{host}` resolving to private/local IP `{ip}`"
            ));
        }
    }
    if count == 0 {
        return Err(format!("DNS lookup returned no addresses for `{host}`"));
    }
    Ok(())
}

/// Валидирует URL и дополнительно проверяет DNS-записи домена через системный резолвер
pub async fn resolve_and_validate_url(raw: &str, allow_private: bool) -> Result<Url, String> {
    let url = validate_public_url(raw, allow_private)?;
    if allow_private {
        return Ok(url);
    }
    if let Some(Host::Domain(domain)) = url.host() {
        let port = url.port_or_known_default().unwrap_or(80);
        let addrs = tokio::net::lookup_host((domain, port))
            .await
            .map_err(|err| format!("DNS resolution failed for `{domain}`: {err}"))?;
        validate_resolved_ips(domain, addrs.map(|sa| sa.ip()), allow_private)?;
    }
    Ok(url)
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || o[0] == 0
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_ipv4(v4);
    }
    let seg0 = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        || (seg0 & 0xfe00) == 0xfc00 // fc00::/7 ULA
        || (seg0 & 0xffc0) == 0xfe80 // fe80::/10 link-local
}

/// Проверяет публичность уже разрешённого `IpAddr`
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !is_private_ipv4(v4),
        IpAddr::V6(v6) => !is_private_ipv6(v6),
    }
}
