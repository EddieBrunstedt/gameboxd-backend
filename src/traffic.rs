//! Single-process admission control. Resolve identity before session lookup.
use crate::error::{ApiError, ErrorCode};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use doorman::axum::extractors::ClientIp;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Traffic {
    proxies: Vec<IpAddr>,
    buckets: Arc<Mutex<Registry>>,
}
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Class {
    Requests,
    Catalog,
    Writes,
    Transfer,
    Reports,
}
impl Class {
    fn policy(self) -> (f64, f64, usize) {
        // burst, refill per second, concurrent requests
        match self {
            Self::Requests => (600., 10., 32),
            Self::Catalog => (5., 0.5, 2),
            Self::Writes => (120., 2., 8),
            Self::Transfer => (2., 0.1, 1),
            Self::Reports => (5., 1. / 6., 2),
        }
    }
}
struct Registry {
    buckets: HashMap<(IpAddr, Class), Bucket>,
    last_sweep: Instant,
}
struct Bucket {
    tokens: f64,
    updated: Instant,
    active: usize,
}
const MAX_BUCKETS: usize = 50_000;
const IDLE: Duration = Duration::from_secs(600);
impl Traffic {
    pub fn new(proxies: Vec<IpAddr>) -> Self {
        Self {
            proxies: proxies.into_iter().map(|ip| ip.to_canonical()).collect(),
            buckets: Arc::new(Mutex::new(Registry {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            })),
        }
    }
    fn acquire(&self, ip: IpAddr, class: Class, now: Instant) -> Result<Permit, u64> {
        let mut registry = self.buckets.lock().expect("traffic lock poisoned");
        // Aggregate native IPv6 addresses by /64; rotating interface IDs must
        // not multiply a visitor's quota. Audit identity retains the full IP.
        let ip = match ip.to_canonical() {
            IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & (u128::MAX << 64)).into()),
            v4 => v4,
        };
        let key = (ip, class);
        if now.duration_since(registry.last_sweep) >= Duration::from_secs(60) {
            registry
                .buckets
                .retain(|_, b| b.active > 0 || now.duration_since(b.updated) < IDLE);
            registry.last_sweep = now;
        }
        let buckets = &mut registry.buckets;
        if !buckets.contains_key(&key) && buckets.len() >= MAX_BUCKETS {
            // Cycling identities must not evict and reset live limits.
            return Err(60);
        }
        let (burst, refill, concurrent) = class.policy();
        let b = buckets.entry(key).or_insert(Bucket {
            tokens: burst,
            updated: now,
            active: 0,
        });
        b.tokens = (b.tokens + now.duration_since(b.updated).as_secs_f64() * refill).min(burst);
        b.updated = now;
        if b.tokens < 1. {
            return Err(((1. - b.tokens) / refill).ceil() as u64);
        }
        if b.active >= concurrent {
            return Err(1);
        }
        b.tokens -= 1.;
        b.active += 1;
        Ok(Permit {
            traffic: self.clone(),
            key,
        })
    }
}
struct Permit {
    traffic: Traffic,
    key: (IpAddr, Class),
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(b) = self
            .traffic
            .buckets
            .lock()
            .expect("traffic lock poisoned")
            .buckets
            .get_mut(&self.key)
        {
            b.active -= 1;
        }
    }
}
fn resolve(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    proxies: &[IpAddr],
) -> Result<IpAddr, ApiError> {
    let peer = peer
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                "Client connection metadata unavailable",
            )
        })?
        .ip()
        .to_canonical();
    if !proxies.contains(&peer) {
        return Ok(peer);
    }
    // ONE edge proxy overwrites XFF with its socket client. Never accept
    // appended chains or fall back to the proxy's shared identity.
    let mut values = headers.get_all("x-forwarded-for").iter();
    let ip = values
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    if values.next().is_some() || ip.is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidClientIp,
            "Trusted proxy must supply one X-Forwarded-For IP address",
        ));
    }
    Ok(ip.expect("checked IP").to_canonical())
}
pub async fn admit(State(traffic): State<Traffic>, mut request: Request, next: Next) -> Response {
    if matches!(request.uri().path(), "/health/live" | "/health/ready")
        || request.method() == Method::OPTIONS
    {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|p| p.0);
    let ip = match resolve(peer, request.headers(), &traffic.proxies) {
        Ok(ip) => ip,
        Err(error) => return error.into_response(),
    };
    request.extensions_mut().insert(ClientIp(ip));
    let path = request.uri().path();
    let write = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let mut classes = vec![Class::Requests];
    if path.starts_with("/api/catalog/") {
        classes.push(Class::Catalog);
    }
    if write && path.starts_with("/api/") {
        classes.push(Class::Writes);
    }
    if matches!(
        path,
        "/api/me/export" | "/api/me/import" | "/api/me/import/preview"
    ) {
        classes.push(Class::Transfer);
    }
    if write && path == "/api/me/reports" {
        classes.push(Class::Reports);
    }
    let mut permits = Vec::new();
    for class in classes {
        match traffic.acquire(ip, class, Instant::now()) {
            Ok(permit) => permits.push(permit),
            Err(seconds) => {
                tracing::info!(limit_class = ?class, "Request admission rejected");
                let mut response = ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    ErrorCode::RateLimited,
                    "Too many requests; wait before retrying",
                )
                .into_response();
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    seconds.max(1).to_string().parse().expect("integer header"),
                );
                return response;
            }
        }
    }
    // RAII releases concurrency slots when a request future is cancelled too.
    let response = next.run(request).await;
    drop(permits);
    response
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_identity_fails_closed_and_canonicalizes() {
        let proxy = "127.0.0.1:5000".parse().unwrap();
        let trusted = ["127.0.0.1".parse().unwrap()];
        let mut h = HeaderMap::new();
        assert!(resolve(None, &h, &trusted).is_err());
        assert!(resolve(Some(proxy), &h, &trusted).is_err());
        for invalid in [
            "unknown",
            "203.0.113.1, 127.0.0.1",
            "203.0.113.1:80",
            "[::1]",
            "",
        ] {
            h.insert("x-forwarded-for", invalid.parse().unwrap());
            assert!(resolve(Some(proxy), &h, &trusted).is_err());
            assert_eq!(resolve(Some(proxy), &h, &[]).unwrap(), proxy.ip());
        }
        h.insert("x-forwarded-for", "::ffff:203.0.113.1".parse().unwrap());
        h.insert("forwarded", "for=198.51.100.1".parse().unwrap());
        h.insert("x-real-ip", "198.51.100.2".parse().unwrap());
        assert_eq!(
            resolve(Some(proxy), &h, &trusted).unwrap(),
            "203.0.113.1".parse::<IpAddr>().unwrap()
        );
        h.append("x-forwarded-for", "203.0.113.1".parse().unwrap());
        assert!(resolve(Some(proxy), &h, &trusted).is_err());
    }
    #[test]
    fn limits_are_independent_refill_and_release_on_cancellation() {
        let traffic = Traffic::new(vec![]);
        let ip = "203.0.113.1".parse().unwrap();
        let other = "203.0.113.2".parse().unwrap();
        let now = Instant::now();
        let a = traffic.acquire(ip, Class::Catalog, now).unwrap();
        let b = traffic.acquire(ip, Class::Catalog, now).unwrap();
        assert_eq!(traffic.acquire(ip, Class::Catalog, now).err(), Some(1));
        drop(a);
        drop(b);
        for _ in 0..3 {
            drop(traffic.acquire(ip, Class::Catalog, now).unwrap());
        }
        assert_eq!(traffic.acquire(ip, Class::Catalog, now).err(), Some(2));
        drop(traffic.acquire(other, Class::Catalog, now).unwrap());
        drop(traffic.acquire(ip, Class::Writes, now).unwrap());
        drop(
            traffic
                .acquire(ip, Class::Catalog, now + Duration::from_secs(2))
                .unwrap(),
        );
    }
    #[test]
    fn ipv6_interface_rotation_cannot_multiply_quota() {
        let traffic = Traffic::new(vec![]);
        let now = Instant::now();
        for n in 1..=5 {
            drop(
                traffic
                    .acquire(
                        format!("2001:db8:1:2::{n}").parse().unwrap(),
                        Class::Catalog,
                        now,
                    )
                    .unwrap(),
            );
        }
        assert_eq!(
            traffic
                .acquire("2001:db8:1:2::99".parse().unwrap(), Class::Catalog, now)
                .err(),
            Some(2)
        );
        drop(
            traffic
                .acquire("2001:db8:1:3::1".parse().unwrap(), Class::Catalog, now)
                .unwrap(),
        );
    }

    #[test]
    fn table_is_bounded_without_resetting_live_buckets() {
        let traffic = Traffic::new(vec![]);
        let now = Instant::now();
        for n in 0..MAX_BUCKETS {
            drop(
                traffic
                    .acquire(IpAddr::V4((n as u32).into()), Class::Requests, now)
                    .unwrap(),
            );
        }
        let new = "203.0.113.1".parse().unwrap();
        assert_eq!(traffic.acquire(new, Class::Requests, now).err(), Some(60));
        drop(traffic.acquire(new, Class::Requests, now + IDLE).unwrap());
        assert_eq!(traffic.buckets.lock().unwrap().buckets.len(), 1);
    }
}
