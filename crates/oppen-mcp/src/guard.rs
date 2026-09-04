//! What every request must satisfy before it reaches a tool
//! (`docs/spec.md` item 14, `docs/decisions.md` D2).
//!
//! The gateway listens on loopback, which stops another host from connecting
//! but does **not** stop a web page the operator is merely visiting. A page on
//! `evil.example` can issue `fetch("http://127.0.0.1:7433/mcp")`, and it can
//! point its own hostname at `127.0.0.1` so the browser believes the request is
//! same-origin — DNS rebinding. Binding to loopback is therefore necessary and
//! not sufficient; the two header checks here are the rest of it.
//!
//! Both checks are pure functions of the request headers, separately from any
//! socket, because that is what makes them testable.

use crate::auth::AuthError;

/// Why a request was refused before any tool ran.
///
/// Deliberately coarse where the caller's behaviour is the same. `Host` and
/// `Origin` are separate variants because the operator needs to know which one
/// tripped — one means a rebinding attempt, the other a browser page — and
/// they are logged, never returned to the caller in detail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("Host header absent")]
    HostMissing,
    #[error("Host {0:?} is not loopback")]
    HostNotLoopback(String),
    #[error("Origin {0:?} is not loopback")]
    OriginNotLoopback(String),
    #[error("Authorization header absent or not a bearer token")]
    BearerMissing,
    #[error(transparent)]
    Auth(#[from] AuthError),
}

/// Hostnames that denote this machine and cannot be pointed elsewhere by an
/// attacker's DNS.
///
/// `localhost` is included because it is what a human types and because no
/// public resolver may return a non-loopback address for it. Any *other* name
/// is rejected even when it currently resolves to `127.0.0.1`: that resolution
/// is the attack, not a reason to trust it.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost", "[::1]", "::1"];

/// Validate the `Host` header.
///
/// Absent is refused rather than waved through: HTTP/1.1 requires it, and a
/// client that omits it is not one oppen supports.
pub fn check_host(host: Option<&str>) -> Result<(), Refusal> {
    let host = host.ok_or(Refusal::HostMissing)?;
    if is_loopback_authority(host) {
        Ok(())
    } else {
        Err(Refusal::HostNotLoopback(host.to_owned()))
    }
}

/// Validate the `Origin` header.
///
/// **Absent is allowed, and that is not a hole.** A browser always attaches
/// `Origin` to a cross-origin `fetch` and cannot be told not to, so "no
/// `Origin`" means the caller is not a web page — it is the MCP client, which
/// is who the gateway is for. Requiring the header instead would reject every
/// legitimate client and accept every attacking page that sets it.
pub fn check_origin(origin: Option<&str>) -> Result<(), Refusal> {
    let Some(origin) = origin else { return Ok(()) };

    // `null` is what a sandboxed iframe or a `file://` page sends. It denotes
    // an opaque origin, which is precisely an origin that cannot be vouched
    // for.
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .filter(|_| origin != "null");

    match authority {
        Some(authority) if is_loopback_authority(authority) => Ok(()),
        _ => Err(Refusal::OriginNotLoopback(origin.to_owned())),
    }
}

/// Extract the token from an `Authorization: Bearer <token>` header.
///
/// The scheme is matched case-insensitively because RFC 7235 says it is
/// case-insensitive, and a client that sends `bearer` is correct.
pub fn bearer_token(authorization: Option<&str>) -> Result<&str, Refusal> {
    let value = authorization.ok_or(Refusal::BearerMissing)?;
    let (scheme, token) = value.split_once(' ').ok_or(Refusal::BearerMissing)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(Refusal::BearerMissing);
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(Refusal::BearerMissing);
    }
    Ok(token)
}

/// Whether an authority (`host` or `host:port`) names this machine.
fn is_loopback_authority(authority: &str) -> bool {
    let host = strip_port(authority);
    LOOPBACK_HOSTS
        .iter()
        .any(|candidate| host.eq_ignore_ascii_case(candidate))
}

/// Split the host from an optional `:port`, handling the bracketed IPv6 form.
///
/// Done by hand rather than by splitting on the last colon, because
/// `[::1]:7433` and `::1` both contain colons that are not port separators.
fn strip_port(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:7433` -> `[::1]`; an unterminated bracket is not an authority
        // this function can make sense of, so it is returned whole and fails
        // the comparison.
        return match rest.find(']') {
            Some(end) => &authority[..=end + 1],
            None => authority,
        };
    }
    match authority.split_once(':') {
        // A bare `::1` has more than one colon and is not `host:port`.
        Some(_) if authority.matches(':').count() > 1 => authority,
        Some((host, _port)) => host,
        None => authority,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_accepted_with_and_without_a_port() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:7433",
            "localhost",
            "localhost:7433",
            "LocalHost:7433",
            "[::1]",
            "[::1]:7433",
            "::1",
        ] {
            assert_eq!(check_host(Some(host)), Ok(()), "rejected {host:?}");
        }
    }

    #[test]
    fn a_rebinding_host_is_refused() {
        // The whole point: these resolve to 127.0.0.1 in a rebinding attack,
        // and the Host header is what gives them away.
        for host in [
            "evil.example",
            "evil.example:7433",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "notlocalhost",
            "192.168.1.5:7433",
            "0.0.0.0",
        ] {
            assert!(
                matches!(check_host(Some(host)), Err(Refusal::HostNotLoopback(_))),
                "accepted {host:?}"
            );
        }
    }

    #[test]
    fn a_missing_host_is_refused() {
        assert_eq!(check_host(None), Err(Refusal::HostMissing));
    }

    #[test]
    fn an_absent_origin_is_allowed_because_that_is_the_mcp_client() {
        assert_eq!(check_origin(None), Ok(()));
    }

    #[test]
    fn a_loopback_origin_is_allowed() {
        for origin in [
            "http://127.0.0.1:7433",
            "http://localhost:7433",
            "http://[::1]:7433",
        ] {
            assert_eq!(check_origin(Some(origin)), Ok(()), "rejected {origin:?}");
        }
    }

    #[test]
    fn a_foreign_or_opaque_origin_is_refused() {
        for origin in [
            "http://evil.example",
            "https://evil.example",
            "http://localhost.evil.example",
            "null",
            "file://",
            "http://127.0.0.1.evil.example",
        ] {
            assert!(
                matches!(
                    check_origin(Some(origin)),
                    Err(Refusal::OriginNotLoopback(_))
                ),
                "accepted {origin:?}"
            );
        }
    }

    #[test]
    fn a_bearer_token_is_extracted_whatever_the_scheme_case() {
        assert_eq!(bearer_token(Some("Bearer abc123")), Ok("abc123"));
        assert_eq!(bearer_token(Some("bearer abc123")), Ok("abc123"));
        assert_eq!(bearer_token(Some("BEARER abc123")), Ok("abc123"));
    }

    #[test]
    fn a_non_bearer_or_empty_authorization_is_refused() {
        for value in [
            "",
            "Basic dXNlcjpwYXNz",
            "Bearer",
            "Bearer ",
            "abc123",
            "Bearer  ",
        ] {
            assert_eq!(
                bearer_token(Some(value)),
                Err(Refusal::BearerMissing),
                "accepted {value:?}"
            );
        }
        assert_eq!(bearer_token(None), Err(Refusal::BearerMissing));
    }
}
