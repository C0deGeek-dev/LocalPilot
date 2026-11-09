//! Which push subscriptions this computer accepts, and which addresses it
//! will send to. A subscription's endpoint comes from the phone's browser, so
//! it is untrusted: only HTTPS on port 443 at a known browser push service,
//! with no user name or fragment, and only to public addresses.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use crate::remote_push_crypto::{AUTH_LEN, PUBLIC_KEY_LEN};

/// Push services named exactly.
const EXACT_HOSTS: [&str; 2] = ["fcm.googleapis.com", "updates.push.services.mozilla.com"];
/// Push services named by a suffix: at least one label before it.
const SUFFIX_HOSTS: [&str; 2] = [".push.apple.com", ".notify.windows.com"];
/// The longest endpoint accepted.
const MAX_ENDPOINT: usize = 1024;

/// Why a subscription was refused. The text names no part of the endpoint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PushRefusal {
    #[error("the notification address is not a valid https address")]
    NotHttps,
    #[error("the notification address is not a known browser push service")]
    UnknownService,
    #[error("the notification address may not carry a user name, port or fragment")]
    Extras,
    #[error("the notification address is too long")]
    TooLong,
    #[error("the subscription's keys are not valid")]
    BadKeys,
}

/// A checked endpoint: its host, for sending and for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) host: String,
}

/// Check an endpoint the browser gave.
pub(crate) fn check_endpoint(endpoint: &str) -> Result<Endpoint, PushRefusal> {
    if endpoint.len() > MAX_ENDPOINT {
        return Err(PushRefusal::TooLong);
    }
    let url = reqwest::Url::parse(endpoint).map_err(|_| PushRefusal::NotHttps)?;
    if url.scheme() != "https" {
        return Err(PushRefusal::NotHttps);
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return Err(PushRefusal::Extras);
    }
    let Some(host) = url.host_str() else {
        return Err(PushRefusal::UnknownService);
    };
    let host = host.to_ascii_lowercase();
    if !known_service(&host) {
        return Err(PushRefusal::UnknownService);
    }
    Ok(Endpoint { host })
}

fn known_service(host: &str) -> bool {
    EXACT_HOSTS.contains(&host)
        || SUFFIX_HOSTS.iter().any(|suffix| {
            host.strip_suffix(suffix)
                .is_some_and(|label| !label.is_empty())
        })
}

/// Decode and check a subscription's keys (base64url): an uncompressed P-256
/// point and a 16-byte authentication secret.
pub(crate) fn check_keys(p256dh: &str, auth: &str) -> Result<(Vec<u8>, Vec<u8>), PushRefusal> {
    let decode = |text: &str| {
        URL_SAFE_NO_PAD
            .decode(text.trim_end_matches('='))
            .map_err(|_| PushRefusal::BadKeys)
    };
    let public = decode(p256dh)?;
    let auth = decode(auth)?;
    if public.len() != PUBLIC_KEY_LEN || public.first() != Some(&0x04) || auth.len() != AUTH_LEN {
        return Err(PushRefusal::BadKeys);
    }
    Ok((public, auth))
}

/// Whether an address is one a push may go to: not private, loopback,
/// link-local, shared, documentation, multicast or otherwise reserved.
pub(crate) fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            is_public_v6(v6)
        }
    }
}

fn is_public_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    !(v4.is_unspecified()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || a == 0
        // Shared address space (carrier-grade NAT), 100.64.0.0/10.
        || (a == 100 && (64..128).contains(&b))
        // IETF protocol assignments, 192.0.0.0/24.
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking, 198.18.0.0/15.
        || (a == 198 && (b == 18 || b == 19))
        // Reserved, 240.0.0.0/4.
        || a >= 240)
}

fn is_public_v6(v6: Ipv6Addr) -> bool {
    let first = v6.segments()[0];
    !(v6.is_unspecified()
        || v6.is_loopback()
        || v6.is_multicast()
        // Unique local, fc00::/7.
        || (first & 0xfe00) == 0xfc00
        // Link-local, fe80::/10.
        || (first & 0xffc0) == 0xfe80
        // Documentation, 2001:db8::/32.
        || (first == 0x2001 && v6.segments()[1] == 0x0db8)
        // IPv4-compatible and other ::/96 forms, and NAT64 well-known prefix.
        || v6.segments()[..6].iter().all(|segment| *segment == 0)
        || (first == 0x0064 && v6.segments()[1] == 0xff9b))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn only_known_push_services_over_https_on_443_are_accepted() {
        for good in [
            "https://fcm.googleapis.com/fcm/send/abc:def",
            "https://updates.push.services.mozilla.com/wpush/v2/gAAAA",
            "https://web.push.apple.com/QGuQyavXutnMH",
            "https://wns2-par02p.notify.windows.com/w/?token=x",
            "https://FCM.googleapis.com:443/x",
        ] {
            assert!(check_endpoint(good).is_ok(), "{good}");
        }
        for (bad, why) in [
            ("http://fcm.googleapis.com/x", PushRefusal::NotHttps),
            ("not a url", PushRefusal::NotHttps),
            ("https://push.apple.com/x", PushRefusal::UnknownService),
            (
                "https://evilpush.apple.com.example/x",
                PushRefusal::UnknownService,
            ),
            ("https://xpush.apple.com/x", PushRefusal::UnknownService),
            ("https://.push.apple.com/x", PushRefusal::UnknownService),
            (
                "https://fcm.googleapis.com.evil.test/x",
                PushRefusal::UnknownService,
            ),
            ("https://127.0.0.1/x", PushRefusal::UnknownService),
            ("https://[::1]/x", PushRefusal::UnknownService),
            ("https://localhost/x", PushRefusal::UnknownService),
            ("https://user@fcm.googleapis.com/x", PushRefusal::Extras),
            ("https://fcm.googleapis.com:8443/x", PushRefusal::Extras),
            ("https://fcm.googleapis.com/x#frag", PushRefusal::Extras),
        ] {
            assert_eq!(check_endpoint(bad), Err(why), "{bad}");
        }
        let long = format!("https://fcm.googleapis.com/{}", "a".repeat(MAX_ENDPOINT));
        assert_eq!(check_endpoint(&long), Err(PushRefusal::TooLong));
    }

    #[test]
    fn subscription_keys_must_be_a_point_and_a_16_byte_secret() {
        let point = URL_SAFE_NO_PAD.encode([4u8; 65]);
        let auth = URL_SAFE_NO_PAD.encode([1u8; 16]);
        assert!(check_keys(&point, &auth).is_ok());
        assert!(
            check_keys(&format!("{point}="), &auth).is_ok(),
            "padding tolerated"
        );
        assert_eq!(
            check_keys(&URL_SAFE_NO_PAD.encode([3u8; 65]), &auth),
            Err(PushRefusal::BadKeys)
        );
        assert_eq!(
            check_keys(&point, &URL_SAFE_NO_PAD.encode([1u8; 15])),
            Err(PushRefusal::BadKeys)
        );
        assert_eq!(check_keys("!!", &auth), Err(PushRefusal::BadKeys));
    }

    #[test]
    fn only_public_addresses_are_reachable() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "192.0.0.8",
            "240.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["142.250.1.1", "2607:f8b0::1", "::ffff:142.250.1.1"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }
}
