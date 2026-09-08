use std::{sync::Arc, time::Duration};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, Semaphore},
    time::Instant,
};

use crate::error::{ApiError, ApiResult};

/// Upstream requests allowed to be in flight at once. Callers beyond this
/// queue on the semaphore rather than failing.
const MAX_IN_FLIGHT: usize = 8;
/// In-flight plus queued callers. Past this the queue is refused outright, so
/// a slow upstream cannot pile up an unbounded backlog of waiters.
const MAX_ADMITTED: usize = 32;
/// Keeps the request rate below IGDB's four per second.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(275);
/// Longer upstream cooldowns are reported as rate limiting instead of held open.
/// Ordinary request-spacing waits are bounded by admission capacity instead.
const MAX_COOLDOWN_WAIT: Duration = Duration::from_secs(1);

const FIELDS: &str = "name,slug,summary,cover.image_id,first_release_date,platforms.name,genres.name,involved_companies.developer,involved_companies.company.name,game_type,parent_game,version_parent,url";

#[derive(Clone)]
pub struct IgdbClient {
    client: reqwest::Client,
    credentials: Option<(String, String)>,
    endpoints: Endpoints,
    state: Arc<ClientState>,
}

#[derive(Clone)]
pub struct Endpoints {
    pub games: String,
    pub token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            games: "https://api.igdb.com/v4/games".into(),
            token: "https://id.twitch.tv/oauth2/token".into(),
        }
    }
}

struct ClientState {
    /// Held across the token round-trip on purpose: it makes refreshes
    /// single-flight, so racing callers await one fetch instead of each
    /// starting their own.
    token: Mutex<Option<(String, Instant)>>,
    /// Request spacing and upstream cooldown share a lock so callers see
    /// a consistent schedule. Never held across I/O.
    throttle: Mutex<Throttle>,
    in_flight: Semaphore,
    admitted: Semaphore,
}

struct Throttle {
    next_request: Instant,
    cooldown_until: Instant,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NamedItem {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ProviderGame {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub slug: String,
    pub summary: Option<String>,
    pub cover: Option<Cover>,
    pub first_release_date: Option<i64>,
    #[serde(default)]
    pub platforms: Vec<NamedItem>,
    #[serde(default)]
    pub genres: Vec<NamedItem>,
    #[serde(default)]
    pub involved_companies: Vec<InvolvedCompany>,
    pub game_type: Option<i64>,
    pub parent_game: Option<i64>,
    pub version_parent: Option<i64>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Cover {
    pub image_id: String,
}

#[derive(Debug, Deserialize)]
pub struct InvolvedCompany {
    #[serde(default)]
    pub developer: bool,
    pub company: NamedItem,
}

impl ProviderGame {
    pub fn cover_url(&self) -> Option<String> {
        self.cover
            .as_ref()
            .filter(|cover| {
                !cover.image_id.is_empty()
                    && cover
                        .image_id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            })
            .map(|cover| {
                format!(
                    "https://images.igdb.com/igdb/image/upload/t_cover_big/{}.jpg",
                    cover.image_id
                )
            })
    }
    pub fn developers(&self) -> Vec<NamedItem> {
        self.involved_companies
            .iter()
            .filter(|company| company.developer)
            .map(|company| company.company.clone())
            .collect()
    }
    pub fn release_date(&self) -> Option<DateTime<Utc>> {
        self.first_release_date
            .and_then(|time| DateTime::from_timestamp(time, 0))
    }
    pub fn source_url(&self) -> String {
        if let Some(url) = self
            .url
            .as_deref()
            .and_then(|url| url::Url::parse(url).ok())
            && url.scheme() == "https"
            && url.host_str() == Some("www.igdb.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().starts_with("/games/")
        {
            return url.to_string();
        }
        let mut url = url::Url::parse("https://www.igdb.com/games/").expect("static URL");
        url.path_segments_mut()
            .expect("hierarchical URL")
            .pop_if_empty()
            .push(if self.slug.is_empty() {
                &self.name
            } else {
                &self.slug
            });
        url.to_string()
    }
}

/// One upstream game as the API exposes it. Distinct from [`crate::models::Game`]:
/// this carries no local identity and no import timestamps, because a viewed
/// game need not exist in the database at all. Games are named publicly by
/// `igdb_id`; the local UUID is an internal primary key and never appears in
/// a URL or a catalog response.
#[derive(Debug, Serialize)]
pub struct CatalogGame {
    pub igdb_id: i64,
    pub name: String,
    pub slug: String,
    pub summary: Option<String>,
    pub cover_url: Option<String>,
    pub first_release_date: Option<DateTime<Utc>>,
    pub platforms: Vec<NamedItem>,
    pub genres: Vec<NamedItem>,
    pub developers: Vec<NamedItem>,
    pub game_type: Option<i64>,
    pub parent_igdb_id: Option<i64>,
    pub version_parent_igdb_id: Option<i64>,
    pub source_url: String,
}

impl From<ProviderGame> for CatalogGame {
    fn from(game: ProviderGame) -> Self {
        Self {
            igdb_id: game.id,
            cover_url: game.cover_url(),
            first_release_date: game.release_date(),
            source_url: game.source_url(),
            developers: game.developers(),
            name: game.name,
            slug: game.slug,
            summary: game.summary,
            platforms: game.platforms,
            genres: game.genres,
            game_type: game.game_type,
            parent_igdb_id: game.parent_game,
            version_parent_igdb_id: game.version_parent,
        }
    }
}

impl IgdbClient {
    pub fn new(
        credentials: Option<(String, String)>,
        endpoints: Endpoints,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            credentials,
            endpoints,
            state: Arc::new(ClientState {
                token: Mutex::new(None),
                throttle: Mutex::new(Throttle {
                    next_request: Instant::now(),
                    cooldown_until: Instant::now(),
                }),
                in_flight: Semaphore::new(MAX_IN_FLIGHT),
                admitted: Semaphore::new(MAX_ADMITTED),
            }),
        })
    }

    pub async fn search(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> ApiResult<Vec<ProviderGame>> {
        let query = validate_search(query)?;
        if !(1..=50).contains(&limit) || offset > 10_000 {
            return Err(ApiError::invalid(
                "limit must be 1–50 and offset at most 10000",
            ));
        }
        // Escape the two APICalypse string delimiters; control characters are rejected above.
        let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
        self.query(format!(
            "search \"{escaped}\"; fields {FIELDS}; limit {limit}; offset {offset};"
        ))
        .await
    }

    pub async fn get(&self, id: i64) -> ApiResult<ProviderGame> {
        if id <= 0 {
            return Err(ApiError::invalid("IGDB ID must be positive"));
        }
        self.query(format!("fields {FIELDS}; where id = {id}; limit 1;"))
            .await?
            .into_iter()
            .find(|game| game.id == id)
            .ok_or_else(ApiError::not_found)
    }

    async fn query(&self, query: String) -> ApiResult<Vec<ProviderGame>> {
        let (id, secret) = self.credentials.as_ref().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "igdb_not_configured",
                "IGDB credentials have not been configured",
            )
        })?;
        // Bound the backlog first, then queue for one of the in-flight slots.
        // One shared client per process.
        let _admitted = self.state.admitted.try_acquire().map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "catalog_busy",
                "Catalog is busy; retry shortly",
            )
        })?;
        let _in_flight = self
            .state
            .in_flight
            .acquire()
            .await
            .map_err(|_| ApiError::upstream())?;
        for attempt in 0..2 {
            let token = self.token(id, secret).await?;
            self.reserve_slot().await?;
            let response = self
                .client
                .post(&self.endpoints.games)
                .header("Client-ID", id)
                .bearer_auth(&token)
                .header("Content-Type", "text/plain")
                .body(query.clone())
                .send()
                .await
                .map_err(|_| ApiError::upstream())?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.invalidate_token(&token).await;
                continue;
            }
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let retry = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(5)
                    .clamp(1, 300);
                self.back_off(Duration::from_secs(retry)).await;
                return Err(rate_limited());
            }
            if !response.status().is_success() {
                return Err(ApiError::upstream());
            }
            let games: Vec<ProviderGame> =
                response.json().await.map_err(|_| ApiError::upstream())?;
            if games
                .iter()
                .any(|game| game.id <= 0 || game.name.trim().is_empty())
            {
                return Err(ApiError::upstream());
            }
            return Ok(games);
        }
        Err(ApiError::upstream())
    }

    /// Returns a live access token, fetching one if the cache is empty or
    /// expired. Only one caller fetches; the rest await it and reuse the result.
    async fn token(&self, id: &str, secret: &str) -> ApiResult<String> {
        let mut cached = self.state.token.lock().await;
        if let Some((token, expiry)) = cached.as_ref()
            && *expiry > Instant::now()
        {
            return Ok(token.clone());
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            expires_in: u64,
        }
        let response = self
            .client
            .post(&self.endpoints.token)
            .form(&[
                ("client_id", id),
                ("client_secret", secret),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await
            .map_err(|_| ApiError::upstream())?;
        if !response.status().is_success() {
            return Err(ApiError::upstream());
        }
        let token: Token = response.json().await.map_err(|_| ApiError::upstream())?;
        if token.access_token.is_empty() || token.expires_in == 0 {
            return Err(ApiError::upstream());
        }
        // Clamp untrusted upstream TTL before adding to Instant.
        let lifetime = token
            .expires_in
            .saturating_sub(60)
            .clamp(1, 60 * 60 * 24 * 60);
        *cached = Some((
            token.access_token.clone(),
            Instant::now() + Duration::from_secs(lifetime),
        ));
        Ok(token.access_token)
    }

    /// Drops the cached token if it is still the one that was rejected, so a
    /// 401 racing a refresh does not discard the replacement.
    async fn invalidate_token(&self, rejected: &str) {
        let mut cached = self.state.token.lock().await;
        if cached.as_ref().is_some_and(|(token, _)| token == rejected) {
            *cached = None;
        }
    }

    /// Waits until dispatch is allowed, rechecking the schedule after every
    /// sleep. Admitted callers may wait for normal request spacing;
    /// only an upstream cooldown beyond [`MAX_COOLDOWN_WAIT`] rejects them.
    async fn reserve_slot(&self) -> ApiResult<()> {
        loop {
            let slot = {
                let mut throttle = self.state.throttle.lock().await;
                let now = Instant::now();
                if throttle.cooldown_until.saturating_duration_since(now) > MAX_COOLDOWN_WAIT {
                    return Err(rate_limited());
                }
                let slot = throttle.next_request.max(throttle.cooldown_until);
                if slot <= now {
                    // Claim only a ready slot. Other waking callers must wait
                    // another interval, including after a cooldown extension.
                    throttle.next_request = now + MIN_REQUEST_INTERVAL;
                    return Ok(());
                }
                slot
            };
            tokio::time::sleep_until(slot).await;
        }
    }

    /// Records upstream cooldown separately from the ordinary dispatch queue.
    async fn back_off(&self, wait: Duration) {
        let mut throttle = self.state.throttle.lock().await;
        throttle.cooldown_until = throttle.cooldown_until.max(Instant::now() + wait);
    }
}

fn rate_limited() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "igdb_rate_limited",
        "Game provider is rate limited; retry later",
    )
}

pub fn validate_search(query: &str) -> ApiResult<&str> {
    let query = query.trim();
    if !(1..=100).contains(&query.chars().count()) || query.chars().any(char::is_control) {
        Err(ApiError::invalid(
            "q must contain 1–100 characters without control characters",
        ))
    } else {
        Ok(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{sync::mpsc, time::advance};

    #[tokio::test(start_paused = true)]
    async fn pending_dispatches_honor_cooldown_extensions_and_resume_with_spacing() {
        let client = IgdbClient::new(None, Endpoints::default()).unwrap();
        assert!(client.reserve_slot().await.is_ok());
        let (sent, mut received) = mpsc::unbounded_channel();
        let mut callers = tokio::task::JoinSet::new();
        for _ in 0..3 {
            let client = client.clone();
            let sent = sent.clone();
            callers.spawn(async move {
                assert!(client.reserve_slot().await.is_ok());
                sent.send(Instant::now()).unwrap();
            });
        }
        // Let all callers start waiting on ordinary request spacing.
        tokio::task::yield_now().await;
        advance(Duration::from_millis(50)).await;
        client.back_off(Duration::from_secs(1)).await;
        advance(MIN_REQUEST_INTERVAL).await;
        assert!(received.try_recv().is_err());

        // A second in-flight response extends the cooldown while they sleep.
        client.back_off(Duration::from_secs(1)).await;
        let cooldown_until = Instant::now() + Duration::from_secs(1);
        // A shorter response must not overwrite that extension.
        client.back_off(Duration::from_millis(100)).await;
        // This crosses the original cooldown expiry, but not the extension.
        advance(Duration::from_millis(800)).await;
        assert!(received.try_recv().is_err());

        // Include a caller arriving during the extended cooldown.
        let arrival = client.clone();
        callers.spawn(async move {
            assert!(arrival.reserve_slot().await.is_ok());
            sent.send(Instant::now()).unwrap();
        });
        while let Some(result) = callers.join_next().await {
            result.unwrap();
        }
        let mut times = Vec::new();
        while let Ok(time) = received.try_recv() {
            times.push(time);
        }
        assert_eq!(times.len(), 4);
        assert!(times[0] >= cooldown_until);
        for pair in times.windows(2) {
            assert!(pair[1].duration_since(pair[0]) >= MIN_REQUEST_INTERVAL);
        }
    }
}
