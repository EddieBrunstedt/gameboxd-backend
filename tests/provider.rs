use gameboxd_backend::{
    api::{EntryInput, PageQuery},
    config::Config,
    igdb::{Endpoints, IgdbClient, validate_search},
};
use std::time::Duration;

use serde_json::json;
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
    // A single token fetch serves all four callers: refreshes are single-flight.
    token(&server, 1).await;
    Mock::given(path("/games"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!([{"id":1,"name":"Halo"}])),
        )
        .expect(4)
        .mount(&server)
        .await;
    let client = client(&server).await;
    // Overlapping callers wait for a slot; none of them is turned away busy.
    let results = tokio::join!(client.get(1), client.get(1), client.get(1), client.get(1));
    for result in [results.0, results.1, results.2, results.3] {
        assert_eq!(
            result.unwrap_or_else(|error| panic!("{}", error.code)).id,
            1
        );
    }
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
        assert_eq!(client.get(1).await.unwrap_err().code, "igdb_rate_limited");
    }
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
        client.search("Halo", 20, 0).await.unwrap_err().code,
        "igdb_not_configured"
    );
    assert_eq!(client.get(-1).await.unwrap_err().status, 400);
    assert_eq!(client.search("Halo", 500, 0).await.unwrap_err().status, 400);
    assert!(validate_search(" \n ").is_err());
    assert!(validate_search("hello\nworld").is_err());
    assert!(validate_search(&"a".repeat(101)).is_err());
}

#[test]
fn library_input_enforces_rating_review_status_and_pagination() {
    for rating in [0, 11, -1] {
        let input: EntryInput =
            serde_json::from_value(json!({"status":"playing","rating":rating})).unwrap();
        assert!(input.validate().is_err());
    }
    let oversized: EntryInput =
        serde_json::from_value(json!({"status":"completed","review":"a".repeat(10001)})).unwrap();
    assert!(oversized.validate().is_err());
    let nul: EntryInput =
        serde_json::from_value(json!({"status":"backlog","review":"bad\u{0000}text"})).unwrap();
    assert!(nul.validate().is_err());
    assert!(serde_json::from_value::<EntryInput>(json!({"status":"invented"})).is_err());
    assert!(
        serde_json::from_value::<EntryInput>(json!({"status":"backlog","user_id":"someone-else"}))
            .is_err()
    );
    let page: PageQuery = serde_json::from_value(json!({"limit":0})).unwrap();
    assert!(page.validate().is_err());
    let valid: EntryInput =
        serde_json::from_value(json!({"status":"on_hold","rating":null,"review":null})).unwrap();
    assert!(valid.validate().is_ok());
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
