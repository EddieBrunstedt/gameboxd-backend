use gameboxd_backend::{
    api::{EntryInput, PageQuery, PlaythroughFields, PlaythroughInput},
    config::Config,
    igdb::{Endpoints, IgdbClient, validate_search},
};
use std::{sync::Arc, time::Duration};

use serde_json::json;
use tokio::{
    sync::{Semaphore, mpsc},
    time::{Instant, timeout},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header, method, path},
};

async fn client(server: &MockServer) -> IgdbClient {
    IgdbClient::new(
        Some(("client-id".into(), "client-secret".into())),
        Endpoints {
            games: format!("{}/games", server.uri()),
            token: format!("{}/token", server.uri()),
        },
    )
    .unwrap()
}

async fn token(server: &MockServer, count: u64) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("client_secret=client-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token":"provider-token","expires_in":3600})),
        )
        .expect(count)
        .mount(server)
        .await;
}

#[tokio::test]
async fn search_escapes_query_and_reuses_token() {
    let server = MockServer::start().await;
    token(&server, 1).await;
    Mock::given(method("POST"))
        .and(path("/games"))
        .and(header("Client-ID", "client-id"))
        .and(header("Authorization", "Bearer provider-token"))
        .and(body_string_contains(r#"search "Halo\"; limit 500; \\";"#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id":1,"name":"Halo","cover":{"image_id":"co_123"},"first_release_date":1000000000
        }])))
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server).await;
    for _ in 0..2 {
        let games = client
            .search(r#"Halo"; limit 500; \"#, 20, 0)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(games[0].id, 1);
        assert_eq!(
            games[0].cover_url().unwrap(),
            "https://images.igdb.com/igdb/image/upload/t_cover_big/co_123.jpg"
        );
        assert!(games[0].release_date().is_some());
        assert!(games[0].platforms.is_empty());
    }
}

#[tokio::test]
async fn concurrent_queries_queue_instead_of_failing() {
    let server = MockServer::start().await;
    // A single token fetch serves all eight callers: refreshes are single-flight.
    token(&server, 1).await;
    Mock::given(path("/games"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!([{"id":1,"name":"Halo"}])),
        )
        .expect(8)
        .mount(&server)
        .await;
    let client = client(&server).await;
    // Overlapping callers wait for a slot; none of them is turned away busy.
    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let client = client.clone();
        callers.spawn(async move { client.get(1).await });
    }
    while let Some(result) = callers.join_next().await {
        assert_eq!(
            result
                .unwrap()
                .unwrap_or_else(|error| panic!("{}", error.code))
                .id,
            1
        );
    }
}

#[tokio::test]
async fn catalog_queue_bounds_admission_concurrency_and_dispatch_rate() {
    let token_server = MockServer::start().await;
    token(&token_server, 1).await;
    // Hold every games response until the test releases it. This makes queue
    // saturation and the in-flight cap observable without relying on latency.
    let responses = Arc::new(Semaphore::new(0));
    let (dispatches, mut received) = mpsc::unbounded_channel();
    let app = axum::Router::new().route(
        "/games",
        axum::routing::post({
            let responses = responses.clone();
            move || {
                let responses = responses.clone();
                let dispatches = dispatches.clone();
                async move {
                    dispatches.send(Instant::now()).unwrap();
                    responses.acquire().await.unwrap().forget();
                    axum::Json(json!([{"id":1,"name":"Halo"}]))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = IgdbClient::new(
        Some(("client-id".into(), "client-secret".into())),
        Endpoints {
            games: format!("http://{address}/games"),
            token: format!("{}/token", token_server.uri()),
        },
    )
    .unwrap();
    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..33 {
        let client = client.clone();
        callers.spawn(async move { client.get(1).await });
    }
    let error = timeout(Duration::from_secs(5), callers.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.status, 429);
    assert_eq!(error.code.as_str(), "catalog_busy");

    let mut times = Vec::new();
    for _ in 0..8 {
        times.push(
            timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    // More than one spacing interval passes with all eight responses held:
    // the ninth request must still be waiting for an in-flight permit.
    assert!(
        timeout(Duration::from_millis(550), received.recv())
            .await
            .is_err()
    );
    responses.add_permits(32);
    timeout(Duration::from_secs(15), async {
        while let Some(result) = callers.join_next().await {
            assert_eq!(
                result
                    .unwrap()
                    .unwrap_or_else(|error| panic!("{}", error.code))
                    .id,
                1
            );
        }
        for _ in 8..32 {
            times.push(received.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
    for pair in times.windows(2) {
        // Allow a little transport/timer jitter around the 275 ms schedule.
        assert!(pair[1].duration_since(pair[0]) >= Duration::from_millis(250));
    }
    server.abort();
}

#[tokio::test]
async fn unauthorized_refreshes_token_once() {
    let server = MockServer::start().await;
    token(&server, 2).await;
    Mock::given(path("/games"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&server)
        .await;
    let error = client(&server).await.get(1).await.unwrap_err();
    assert_eq!(error.status, 502);
    assert!(!error.message.contains("provider-token"));
}

#[tokio::test]
async fn provider_rate_limit_stops_subsequent_upstream_requests() {
    let server = MockServer::start().await;
    token(&server, 1).await;
    Mock::given(path("/games"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "60"))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server).await;
    for _ in 0..2 {
        let error = client.get(1).await.unwrap_err();
        assert_eq!(error.status, 503);
        assert_eq!(error.code.as_str(), "igdb_rate_limited");
    }
}

#[tokio::test]
async fn provider_cooldown_stops_already_waiting_requests() {
    let server = MockServer::start().await;
    token(&server, 1).await;
    Mock::given(path("/games"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "60")
                .set_delay(Duration::from_millis(50)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let client = client.clone();
        callers.spawn(async move { client.get(1).await });
    }
    timeout(Duration::from_secs(5), async {
        while let Some(result) = callers.join_next().await {
            let error = result.unwrap().unwrap_err();
            assert_eq!(error.status, 503);
            assert_eq!(error.code.as_str(), "igdb_rate_limited");
        }
    })
    .await
    .unwrap();
    let error = client.get(1).await.unwrap_err();
    assert_eq!(error.code.as_str(), "igdb_rate_limited");
}

#[tokio::test]
async fn malformed_upstream_data_becomes_safe_gateway_error() {
    let server = MockServer::start().await;
    token(&server, 1).await;
    Mock::given(path("/games"))
        .respond_with(ResponseTemplate::new(200).set_body_string("internal provider secret"))
        .mount(&server)
        .await;
    let error = client(&server).await.get(1).await.unwrap_err();
    assert_eq!(error.status, 502);
    assert!(!error.message.contains("secret"));
}

#[tokio::test]
async fn empty_result_is_not_found() {
    let server = MockServer::start().await;
    token(&server, 1).await;
    Mock::given(path("/games"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    assert_eq!(client(&server).await.get(44).await.unwrap_err().status, 404);
}

#[tokio::test]
async fn credentials_are_optional_and_invalid_inputs_do_not_call_provider() {
    let client = IgdbClient::new(None, Endpoints::default()).unwrap();
    assert_eq!(
        client
            .search("Halo", 20, 0)
            .await
            .unwrap_err()
            .code
            .as_str(),
        "igdb_not_configured"
    );
    assert_eq!(
        client.get(42).await.unwrap_err().code.as_str(),
        "igdb_not_configured"
    );
    assert_eq!(client.get(-1).await.unwrap_err().status, 400);
    assert_eq!(client.search("Halo", 500, 0).await.unwrap_err().status, 400);
    assert!(validate_search(" \n ").is_err());
    assert!(validate_search("hello\nworld").is_err());
    assert!(validate_search(&"a".repeat(101)).is_err());
}

#[test]
fn library_and_playthrough_inputs_enforce_their_separate_contracts() {
    for rating in [0, 11, -1] {
        let input: PlaythroughInput = serde_json::from_value(json!({"rating":rating})).unwrap();
        assert!(PlaythroughFields::from(input).validated().is_err());
    }
    for invalid in [
        json!({"review":"a".repeat(10001)}),
        json!({"review":"bad\u{0000}text"}),
        json!({"platform":" "}),
        json!({"platform":"a".repeat(101)}),
        json!({"platform":"PC\n"}),
        json!({"started_on":"2026-02-02","finished_on":"2026-02-01"}),
    ] {
        let input: PlaythroughInput = serde_json::from_value(invalid).unwrap();
        assert!(PlaythroughFields::from(input).validated().is_err());
    }
    for invalid in [
        json!({"status":"invented"}),
        json!({"status":"backlog","user_id":"someone-else"}),
        json!({"status":"playing","rating":8}),
        json!({"status":"completed","review":"Moved to playthroughs"}),
    ] {
        assert!(serde_json::from_value::<EntryInput>(invalid).is_err());
    }
    assert!(serde_json::from_value::<EntryInput>(json!({"status":"on_hold"})).is_ok());
    assert!(
        serde_json::from_value::<PlaythroughInput>(json!({"started_on":"2026-02-30"})).is_err()
    );
    assert!(serde_json::from_value::<PlaythroughInput>(json!({"user_id":"someone-else"})).is_err());
    for valid in [
        json!({}),
        json!({"rating":null,"review":null}),
        json!({"rating":1}),
        json!({"rating":10,"review":"é".repeat(10000)}),
        json!({"started_on":"2026-01-01","platform":" PC "}),
        json!({"finished_on":"2022-01-01","review":"Never finished the game"}),
        json!({"started_on":"2026-01-01","finished_on":"2026-01-01"}),
    ] {
        let input: PlaythroughInput = serde_json::from_value(valid).unwrap();
        assert!(PlaythroughFields::from(input).validated().is_ok());
    }
    let page: PageQuery = serde_json::from_value(json!({"limit":0})).unwrap();
    assert!(page.validate().is_err());
}

#[test]
fn configuration_rejects_unsafe_browser_origins() {
    let mut config = Config {
        bind_addr: "127.0.0.1:3000".parse().unwrap(),
        database_url: "unused".into(),
        frontend_origin: "http://localhost:5173".parse().unwrap(),
        rp_id: "localhost".into(),
        production: false,
        igdb_credentials: None,
        smtp: None,
    };
    assert!(config.validate().is_ok());
    for origin in [
        "http://example.com",
        "https://example.com/path",
        "https://user:pass@example.com",
        "https://example.com?q=1",
    ] {
        config.frontend_origin = origin.parse().unwrap();
        assert!(config.validate().is_err());
    }
    config.frontend_origin = "http://localhost:5173".parse().unwrap();
    config.production = true;
    assert!(config.validate().is_err());
}
