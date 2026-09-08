use std::{net::SocketAddr, str::FromStr, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use chrono::Utc;
use doorman::{
    core::{audit::Actor, storage::AccountStatus},
    prelude::{AuthService, OperatorActor, SessionContext, User, UserId},
};
use gameboxd_backend::{
    AppState, auth,
    config::Config,
    db::Database,
    igdb::{Endpoints, IgdbClient},
    router,
};
use serde_json::{Value, json};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

struct Fixture {
    name: String,
    admin: PgPool,
    database: Database,
    auth: AuthService,
    app: Router,
    server: MockServer,
}

impl Fixture {
    async fn new() -> Self {
        dotenvy::dotenv().ok();
        let url = std::env::var("TEST_DATABASE_URL").expect(
            "Set TEST_DATABASE_URL to a PostgreSQL account with CREATE DATABASE permission",
        );
        let options = PgConnectOptions::from_str(&url).unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        let name = format!("gameboxd_test_{}", Uuid::new_v4().simple());
        // This identifier contains only our fixed prefix and generated UUID hex;
        // PostgreSQL cannot bind database identifiers as query parameters.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .unwrap();
        // Only replace the URL path; preserve credentials, TLS and connection options.
        let mut test_url = url::Url::parse(&url).unwrap();
        test_url.set_path(&name);
        let database = Database::connect(test_url.as_str()).await.unwrap();
        // A second startup must leave both independent migration histories intact.
        let restarted = Database::connect(test_url.as_str()).await.unwrap();
        restarted.app.close().await;
        restarted.auth.pool().close().await;
        let config = Config {
            bind_addr: "127.0.0.1:3000".parse().unwrap(),
            database_url: test_url.to_string(),
            frontend_origin: "http://localhost:5173".parse().unwrap(),
            rp_id: "localhost".into(),
            production: false,
            igdb_credentials: None,
            smtp: None,
        };
        let auth = auth::build(&config, &database.auth, database.app.clone()).unwrap();
        let server = MockServer::start().await;
        Mock::given(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":"test-provider-token","expires_in":3600})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/games"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id":42,"name":"Test Game","slug":"test-game","summary":"A fixture game",
                "platforms":[{"id":6,"name":"PC"}],"genres":[{"id":12,"name":"RPG"}],
                "involved_companies":[{"developer":true,"company":{"id":7,"name":"Test Studio"}}],
                "game_type":0,"url":"https://www.igdb.com/games/test-game"
            }])))
            .mount(&server)
            .await;
        let igdb = IgdbClient::new(
            Some(("id".into(), "secret".into())),
            Endpoints {
                games: format!("{}/games", server.uri()),
                token: format!("{}/token", server.uri()),
            },
        )
        .unwrap();
        let app = router(
            &config,
            AppState {
                pool: database.app.clone(),
                igdb,
            },
            &auth,
        );
        Self {
            name,
            admin,
            database,
            auth,
            app,
            server,
        }
    }

    async fn user(&self, name: &str) -> (UserId, String) {
        let id = UserId::new();
        self.auth
            .state()
            .user_store()
            .create_user(User {
                id,
                email: Some(format!("{name}@example.test")),
                email_verified_at: Some(Utc::now()),
                status: AccountStatus::Active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await
            .unwrap();
        let session = self
            .auth
            .state()
            .session_manager()
            .create(id, SessionContext::default())
            .await
            .unwrap();
        (id, session.token)
    }

    async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Value) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .extension(ConnectInfo(
                "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
            ))
            .header("Content-Type", "application/json");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        self.app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn finish(self) {
        drop(self.app);
        drop(self.auth);
        self.database.app.close().await;
        self.database.auth.pool().close().await;
        // Same locally generated identifier used in CREATE DATABASE above.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE {}", self.name)))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }

    async fn value(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Value,
        status: StatusCode,
    ) -> Value {
        json_body(self.call(method, path, Some(token), body).await, status).await
    }
}

async fn json_body(response: Response, status: StatusCode) -> Value {
    let actual = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    assert_eq!(actual, status, "{}", String::from_utf8_lossy(&bytes));
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn public_catalog_details_work_without_search_or_import_and_never_write() {
    let fixture = Fixture::new().await;
    let detail_path = "/api/catalog/games/42";
    // A direct URL works before any search, import or sign-in.
    let detail = json_body(
        fixture.call("GET", detail_path, None, Value::Null).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["igdb_id"], 42);
    assert_eq!(detail["developers"][0]["name"], "Test Studio");
    for field in ["id", "imported_at", "refreshed_at"] {
        assert!(detail.get(field).is_none());
    }
    let search = json_body(
        fixture
            .call("GET", "/api/catalog/search?q=Test", None, Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail, search["data"][0]);
    let reloaded = json_body(
        fixture.call("GET", detail_path, None, Value::Null).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(reloaded, detail);
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM games), (SELECT count(*) FROM library_entries),
                (SELECT count(*) FROM playthroughs)",
    )
    .fetch_one(&fixture.database.app)
    .await
    .unwrap();
    assert_eq!(counts, (0, 0, 0));
    json_body(
        fixture
            .call("GET", "/api/games/42", None, Value::Null)
            .await,
        StatusCode::NOT_FOUND,
    )
    .await;

    let (_, token) = fixture.user("catalog").await;
    fixture
        .value(
            "POST",
            "/api/catalog/import/42",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    // Even stale stored metadata is untouched when the upstream view changes.
    sqlx::query("UPDATE games SET name='Stored title', refreshed_at=now()-interval '2 days'")
        .execute(&fixture.database.app)
        .await
        .unwrap();
    let stored = json_body(
        fixture
            .call("GET", "/api/games/42", None, Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        json_body(
            fixture.call("GET", detail_path, None, Value::Null).await,
            StatusCode::OK
        )
        .await,
        detail
    );
    let after = json_body(
        fixture
            .call("GET", "/api/games/42", None, Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(after, stored);

    // Provider failure is not a missing game; local reads still work offline.
    fixture.server.reset().await;
    Mock::given(path("/games"))
        .respond_with(ResponseTemplate::new(500).set_body_string("provider secret"))
        .expect(1)
        .mount(&fixture.server)
        .await;
    let error = json_body(
        fixture.call("GET", detail_path, None, Value::Null).await,
        StatusCode::BAD_GATEWAY,
    )
    .await;
    assert_eq!(error["error"]["code"], "igdb_unavailable");
    assert!(!error.to_string().contains("provider secret"));
    assert_eq!(
        json_body(
            fixture
                .call("GET", "/api/games/42", None, Value::Null)
                .await,
            StatusCode::OK
        )
        .await,
        stored
    );
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn public_catalog_lookup_validates_ids_and_reports_missing_games() {
    let fixture = Fixture::new().await;
    for id in ["0", "-1", "not-an-id", "9223372036854775808", "42;limit=50"] {
        let error = json_body(
            fixture
                .call(
                    "GET",
                    &format!("/api/catalog/games/{id}"),
                    None,
                    Value::Null,
                )
                .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(error["error"]["code"], "validation_error");
    }
    assert!(fixture.server.received_requests().await.unwrap().is_empty());
    Mock::given(path("/games"))
        .and(wiremock::matchers::body_string_contains(
            "where id = 999; limit 1;",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .with_priority(1)
        .expect(1)
        .mount(&fixture.server)
        .await;
    let missing = json_body(
        fixture
            .call("GET", "/api/catalog/games/999", None, Value::Null)
            .await,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(missing["error"]["code"], "not_found");
    fixture.finish().await;
}

const AUTHENTICATE_START: &str = "/auth/passkey/authenticate/start";

fn authentication_request(peer: Option<SocketAddr>) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(AUTHENTICATE_START)
        .header("Origin", "http://localhost:5173")
        .body(Body::empty())
        .unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    request
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn socket_authentication_limits_and_audit_ignore_forwarding_headers() {
    let fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = fixture.app.clone();
    let (shutdown, stopping) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            stopping.await.ok();
        })
        .await
        .unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    // Bound the sequence so a slow test cannot silently cross a limiter window.
    tokio::time::timeout(Duration::from_secs(30), async {
        for number in 1..=61 {
            let response = client
                .post(format!("{base}{AUTHENTICATE_START}"))
                .send()
                .await
                .unwrap();
            let expected = if number <= 60 {
                StatusCode::OK
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(response.status(), expected, "request {number}");
            assert_eq!(response.headers()["cache-control"], "no-store");
            let body = response.json::<Value>().await.unwrap();
            if expected == StatusCode::OK {
                assert!(body["challenge_id"].is_string());
            } else {
                assert_eq!(body["error"]["code"], "rate_limited");
            }
        }
        for headers in [
            vec![("x-forwarded-for", "203.0.113.1")],
            vec![("x-real-ip", "203.0.113.2")],
            vec![
                ("x-forwarded-for", "203.0.113.3, 10.0.0.1"),
                ("x-real-ip", "203.0.113.4"),
                ("forwarded", "for=203.0.113.5"),
            ],
        ] {
            let mut request = client.post(format!("{base}{AUTHENTICATE_START}"));
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(
                response.headers()["retry-after"]
                    .to_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    > 0
            );
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["code"],
                "rate_limited"
            );
        }
    })
    .await
    .expect("rate-limit regression must finish within one window");

    // Exercise the real database audit sink, not a context-only test endpoint.
    for number in 1..=2 {
        let response = client
            .post(format!("{base}/auth/signup/request"))
            .header("x-forwarded-for", format!("203.0.113.{number}"))
            .header("x-real-ip", format!("198.51.100.{number}"))
            .header("user-agent", "client-ip-regression")
            .json(&json!({"email": format!("ip-test-{number}@example.test")}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    let events: Vec<sqlx::types::Json<doorman::core::audit::AuditEvent>> =
        sqlx::query_scalar("SELECT event FROM auth_audit_events")
            .fetch_all(&fixture.database.app)
            .await
            .unwrap();
    assert!(events.len() >= 2, "signup must persist audit events");
    for event in events {
        assert!(matches!(event.0.actor,
            Actor::Subject { ip: Some(ref ip), user_agent: Some(ref ua) }
            if ip == "127.0.0.1" && ua == "client-ip-regression"
        ));
    }
    shutdown.send(()).unwrap();
    server.await.unwrap();
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn authentication_requires_socket_metadata_and_buckets_by_ip() {
    let fixture = Fixture::new().await;
    // Headers cannot replace absent server metadata.
    for spoofed in [false, true] {
        let mut request = authentication_request(None);
        if spoofed {
            request
                .headers_mut()
                .insert("x-forwarded-for", "203.0.113.1".parse().unwrap());
            request
                .headers_mut()
                .insert("x-real-ip", "203.0.113.2".parse().unwrap());
        }
        let response = fixture.app.clone().oneshot(request).await.unwrap();
        let body = json_body(response, StatusCode::INTERNAL_SERVER_ERROR).await;
        assert_eq!(body["error"]["code"], "internal_error");
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        // Changing the connection's source port must not create a new bucket.
        for port in 1000..1060 {
            let request = authentication_request(Some(SocketAddr::from(([192, 0, 2, 1], port))));
            json_body(
                fixture.app.clone().oneshot(request).await.unwrap(),
                StatusCode::OK,
            )
            .await;
        }
        for peer in ["192.0.2.1:9000", "[::ffff:192.0.2.1]:9001"] {
            let request = authentication_request(Some(peer.parse().unwrap()));
            let body = json_body(
                fixture.app.clone().oneshot(request).await.unwrap(),
                StatusCode::TOO_MANY_REQUESTS,
            )
            .await;
            assert_eq!(body["error"]["code"], "rate_limited");
        }
        for peer in ["192.0.2.2:1000", "[2001:db8::1]:1000"] {
            let request = authentication_request(Some(peer.parse().unwrap()));
            json_body(
                fixture.app.clone().oneshot(request).await.unwrap(),
                StatusCode::OK,
            )
            .await;
        }
    })
    .await
    .expect("rate-limit regression must finish within one window");
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn authenticated_catalog_and_library_flow_with_ownership_and_logout() {
    let fixture = Fixture::new().await;
    let (alice_id, alice) = fixture.user("alice").await;
    let (_, bob) = fixture.user("bob").await;
    assert_eq!(
        fixture
            .call("GET", "/api/me", None, Value::Null)
            .await
            .status(),
        401
    );
    assert_eq!(
        fixture
            .call("POST", "/api/catalog/import/42", None, Value::Null)
            .await
            .status(),
        401
    );
    let me = json_body(
        fixture
            .call("GET", "/api/me", Some(&alice), Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(me["id"], alice_id.0.to_string());
    json_body(
        fixture
            .call(
                "PUT",
                "/api/me",
                Some(&alice),
                json!({"display_name":"Alice","bio":"RPG fan"}),
            )
            .await,
        StatusCode::OK,
    )
    .await;

    let result = json_body(
        fixture
            .call(
                "GET",
                "/api/catalog/search?q=Test",
                Some(&alice),
                Value::Null,
            )
            .await,
        StatusCode::OK,
    )
    .await;
    let game = result["data"][0].clone();
    assert_eq!(game["developers"][0]["name"], "Test Studio");
    // Search is a read-only proxy. Results are named by upstream ID and carry
    // no local identity, because nothing has been stored.
    let igdb_id = game["igdb_id"].as_i64().unwrap();
    assert!(game.get("id").is_none());
    let game_path = format!("/api/games/{igdb_id}");
    let entry_path = format!("/api/me/library/{igdb_id}");
    assert_eq!(
        fixture
            .call("GET", &game_path, None, Value::Null)
            .await
            .status(),
        404,
        "search must not import the games it returns"
    );
    let imported = json_body(
        fixture
            .call("POST", "/api/catalog/import/42", Some(&alice), Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(imported["igdb_id"], igdb_id);
    let local_id = imported["id"].clone();
    json_body(
        fixture.call("GET", &game_path, None, Value::Null).await,
        StatusCode::OK,
    )
    .await;
    let requests = fixture.server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/games")
            .count(),
        2,
        "search and import each fetch upstream; search warms no cache"
    );

    // Local search is an unanchored substring match served by the trigram
    // index, and LIKE wildcards in the term are escaped rather than honoured.
    let found = json_body(
        fixture
            .call("GET", "/api/games?q=est%20Gam", None, Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(found["data"][0]["igdb_id"], igdb_id);
    for term in ["T%25Game", "T%25", "Test_Game"] {
        let escaped = json_body(
            fixture
                .call("GET", &format!("/api/games?q={term}"), None, Value::Null)
                .await,
            StatusCode::OK,
        )
        .await;
        assert_eq!(escaped["data"], json!([]), "{term} must match literally");
    }

    let saved = json_body(
        fixture
            .call(
                "PUT",
                &entry_path,
                Some(&alice),
                json!({"status":"playing"}),
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(saved["rating"].is_null());
    assert!(saved.get("review").is_none());
    assert_eq!(
        fixture
            .call("GET", &entry_path, Some(&bob), Value::Null)
            .await
            .status(),
        404
    );
    assert_eq!(
        fixture
            .call("DELETE", &entry_path, Some(&bob), Value::Null)
            .await
            .status(),
        204
    );
    assert_eq!(
        fixture
            .call("GET", &entry_path, Some(&alice), Value::Null)
            .await
            .status(),
        200
    );
    let empty = json_body(
        fixture
            .call("GET", "/api/me/library", Some(&bob), Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(empty["data"], json!([]));
    let listed = json_body(
        fixture
            .call(
                "GET",
                "/api/me/library?status=playing&limit=1",
                Some(&alice),
                Value::Null,
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed["data"][0]["game"]["igdb_id"], igdb_id);
    assert_eq!(listed["data"][0]["entry"]["igdb_id"], igdb_id);
    // Both routes publish a game through the same struct over an explicit
    // column list, so neither can start exposing a column the other does not.
    assert_eq!(listed["data"][0]["game"], found["data"][0]);
    assert_eq!(listed["has_more"], false);
    assert!(listed["data"][0]["entry"].get("user_id").is_none());
    // Entries name their game the way every route does; the local UUID is internal.
    assert!(listed["data"][0]["entry"].get("game_id").is_none());
    // The library filter escapes wildcards the same way the catalog does.
    let filtered = json_body(
        fixture
            .call(
                "GET",
                "/api/me/library?q=st%20Ga",
                Some(&alice),
                Value::Null,
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(filtered["data"][0]["game"]["igdb_id"], igdb_id);
    let filtered = json_body(
        fixture
            .call(
                "GET",
                "/api/me/library?q=Test_Game",
                Some(&alice),
                Value::Null,
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(filtered["data"], json!([]));

    assert_eq!(
        fixture
            .call(
                "PUT",
                &entry_path,
                Some(&alice),
                json!({"status":"invented"})
            )
            .await
            .status(),
        422
    );
    assert_eq!(
        fixture
            .call(
                "PUT",
                &entry_path,
                Some(&alice),
                json!({"status":"completed","user_id":alice_id.0})
            )
            .await
            .status(),
        422
    );
    let cleared = json_body(
        fixture
            .call(
                "PUT",
                &entry_path,
                Some(&alice),
                json!({"status":"completed"}),
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(cleared["rating"].is_null());
    assert!(cleared.get("review").is_none());

    // Refresh must preserve both the local ID and the attached library entry.
    sqlx::query("UPDATE games SET refreshed_at=now()-interval '2 days'")
        .execute(&fixture.database.app)
        .await
        .unwrap();
    let refreshed = json_body(
        fixture
            .call("POST", "/api/catalog/import/42", Some(&alice), Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        refreshed["id"], local_id,
        "a refresh must reuse the local row, keeping attached entries valid"
    );
    assert_eq!(
        fixture
            .call("GET", &entry_path, Some(&alice), Value::Null)
            .await
            .status(),
        200
    );
    assert_eq!(
        fixture
            .call("DELETE", &entry_path, Some(&alice), Value::Null)
            .await
            .status(),
        204
    );
    assert_eq!(
        fixture
            .call("GET", &entry_path, Some(&alice), Value::Null)
            .await
            .status(),
        404
    );
    assert_eq!(
        fixture
            .call("POST", "/auth/logout", Some(&alice), json!({}))
            .await
            .status(),
        204
    );
    assert_eq!(
        fixture
            .call("GET", "/api/me", Some(&alice), Value::Null)
            .await
            .status(),
        401,
        "warm session cache must be invalidated"
    );
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn browser_security_validation_and_account_deletion_cascade() {
    let fixture = Fixture::new().await;
    let (id, token) = fixture.user("browser").await;
    for origin in [None, Some("https://evil.example")] {
        let mut request = Request::builder()
            .method("PUT")
            .uri("/api/me")
            .header("Cookie", format!("gameboxd_session={token}"))
            .header("Content-Type", "application/json");
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        let response = fixture
            .app
            .clone()
            .oneshot(
                request
                    .body(Body::from(r#"{"display_name":"Browser","bio":""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response, StatusCode::FORBIDDEN).await;
        assert_eq!(body["error"]["code"], "csrf_origin_mismatch");
    }
    let request = Request::builder()
        .method("PUT")
        .uri("/api/me")
        .header("Cookie", format!("gameboxd_session={token}"))
        .header("Origin", "http://localhost:5173")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Browser","bio":""}"#))
        .unwrap();
    let response = fixture.app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    assert_eq!(
        response.headers()["access-control-allow-credentials"],
        "true"
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response.headers().contains_key("x-request-id"));
    json_body(response, StatusCode::OK).await;

    let preflight = Request::builder()
        .method("OPTIONS")
        .uri("/api/me")
        .header("Origin", "http://localhost:5173")
        .header("Access-Control-Request-Method", "PUT")
        .header("Access-Control-Request-Headers", "content-type")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        fixture
            .app
            .clone()
            .oneshot(preflight)
            .await
            .unwrap()
            .status(),
        200
    );
    json_body(
        fixture
            .call("GET", "/api/games?limit=999", None, Value::Null)
            .await,
        StatusCode::BAD_REQUEST,
    )
    .await;
    json_body(
        fixture
            .call("GET", "/api/games/not-a-number", None, Value::Null)
            .await,
        StatusCode::BAD_REQUEST,
    )
    .await;
    json_body(
        fixture.call("GET", "/missing", None, Value::Null).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        fixture
            .call("GET", "/health/ready", None, Value::Null)
            .await
            .status(),
        200
    );
    // Exercise the actual mounted signup and sign-in beginnings. An email request
    // or challenge must never issue a session before the browser proves a passkey.
    let signup = fixture
        .call(
            "POST",
            "/auth/signup/request",
            None,
            json!({"email":"new-player@example.test"}),
        )
        .await;
    assert_eq!(signup.status(), 204);
    assert!(!signup.headers().contains_key("set-cookie"));
    let challenge = fixture
        .call("POST", "/auth/passkey/authenticate/start", None, json!({}))
        .await;
    assert!(!challenge.headers().contains_key("set-cookie"));
    let challenge = json_body(challenge, StatusCode::OK).await;
    assert!(challenge["challenge_id"].is_string());
    assert!(challenge.get("options").is_some());
    assert_eq!(
        fixture
            .call("GET", "/api/me", None, Value::Null)
            .await
            .status(),
        401
    );
    let oversized = json!({"display_name":"a".repeat(70_000),"bio":""});
    json_body(
        fixture
            .call("PUT", "/api/me", Some(&token), oversized)
            .await,
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;
    // Adding an unimported game must materialize it, with no prior import call.
    json_body(
        fixture
            .call(
                "PUT",
                "/api/me/library/42",
                Some(&token),
                json!({"status":"backlog"}),
            )
            .await,
        StatusCode::OK,
    )
    .await;
    json_body(
        fixture
            .call(
                "POST",
                "/api/me/playthroughs",
                Some(&token),
                json!({"igdb_id":42,"rating":8}),
            )
            .await,
        StatusCode::CREATED,
    )
    .await;
    // The user-facing route must demand a fresh passkey proof.
    let delete = json_body(
        fixture
            .call("DELETE", "/auth/account", Some(&token), json!({}))
            .await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(delete["error"]["code"], "reauth_required");
    // Exercise Doorman's actual deletion manager (never expose this as an unauthenticated route).
    fixture
        .auth
        .state()
        .operator()
        .delete_account(
            id,
            OperatorActor::new("integration-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    let profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles")
        .fetch_one(&fixture.database.app)
        .await
        .unwrap();
    let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM library_entries")
        .fetch_one(&fixture.database.app)
        .await
        .unwrap();
    let playthroughs: i64 = sqlx::query_scalar("SELECT count(*) FROM playthroughs")
        .fetch_one(&fixture.database.app)
        .await
        .unwrap();
    assert_eq!((profiles, entries, playthroughs), (0, 0, 0));
    assert_eq!(
        fixture
            .call("GET", "/api/me", Some(&token), Value::Null)
            .await
            .status(),
        401
    );
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_audit_events")
        .fetch_one(&fixture.database.app)
        .await
        .unwrap();
    assert!(audits > 0);
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn repeat_playthroughs_preserve_history_and_derive_library_rating() {
    let fixture = Fixture::new().await;
    let (_, alice) = fixture.user("diary-alice").await;
    let (_, bob) = fixture.user("diary-bob").await;
    let collection = "/api/me/playthroughs";
    let library = "/api/me/library/42";
    for method in ["GET", "POST"] {
        assert_eq!(
            fixture
                .call(method, collection, None, json!({"igdb_id":42}))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    // Logging does not require library membership, dates, or finishing a game.
    let original = fixture.value("POST", collection, &alice,
        json!({"igdb_id":42,"rating":9,"review":"First experience","finished_on":"2022-04-01","platform":" PC "}),
        StatusCode::CREATED).await;
    assert_eq!(original["platform"], "PC");
    assert!(original["started_on"].is_null());
    assert!(original.get("user_id").is_none());
    assert!(original.get("game_id").is_none());
    fixture
        .value("GET", library, &alice, Value::Null, StatusCode::NOT_FOUND)
        .await;
    let entry = fixture
        .value(
            "PUT",
            library,
            &alice,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(entry["rating"], 9);
    assert_eq!(entry["rating_playthrough_id"], original["id"]);

    let replay = fixture.value("POST", collection, &alice,
        json!({"igdb_id":42,"rating":7,"review":"Replay","started_on":"2026-01-01","finished_on":"2026-02-01"}),
        StatusCode::CREATED).await;
    // Backfilling an old experience must not replace the newer rating.
    let older = fixture
        .value(
            "POST",
            collection,
            &alice,
            json!({"igdb_id":42,"rating":10,"finished_on":"2020-01-01"}),
            StatusCode::CREATED,
        )
        .await;
    let unrated = fixture
        .value(
            "POST",
            collection,
            &alice,
            json!({"igdb_id":42,"review":"Still playing","started_on":"2026-03-01"}),
            StatusCode::CREATED,
        )
        .await;
    let saved = fixture
        .value(
            "PUT",
            library,
            &alice,
            json!({"status":"on_hold"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(saved["rating"], 7);
    assert_eq!(saved["rating_playthrough_id"], replay["id"]);
    assert!(saved.get("review").is_none());
    let listed = fixture
        .value(
            "GET",
            "/api/me/library",
            &alice,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed["data"][0]["entry"], saved);

    // Per-game and whole-diary pagination use the same chronology.
    let page = fixture
        .value(
            "GET",
            "/api/me/playthroughs?igdb_id=42&limit=2",
            &alice,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(page["data"][0]["id"], unrated["id"]);
    assert_eq!(page["data"][1]["id"], replay["id"]);
    assert_eq!(page["has_more"], true);
    let page = fixture
        .value(
            "GET",
            "/api/me/playthroughs?limit=2&offset=2",
            &alice,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(page["data"][0]["id"], original["id"]);
    assert_eq!(page["data"][1]["id"], older["id"]);
    assert_eq!(page["has_more"], false);
    for query in ["igdb_id=999", "offset=4"] {
        let page = fixture
            .value(
                "GET",
                &format!("{collection}?{query}"),
                &alice,
                Value::Null,
                StatusCode::OK,
            )
            .await;
        assert_eq!(page["data"], json!([]));
    }
    let original_path = format!("{collection}/{}", original["id"].as_str().unwrap());
    let replay_path = format!("{collection}/{}", replay["id"].as_str().unwrap());
    for method in ["GET", "PUT", "DELETE"] {
        assert_eq!(
            fixture
                .call(method, &original_path, None, json!({}))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    fixture
        .value(
            "GET",
            &original_path,
            &bob,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    fixture
        .value(
            "PUT",
            &original_path,
            &bob,
            json!({"rating":1}),
            StatusCode::NOT_FOUND,
        )
        .await;
    fixture
        .value(
            "DELETE",
            &original_path,
            &bob,
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    let private = fixture
        .value("GET", collection, &bob, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(private["data"], json!([]));
    // A different user's newer rating cannot affect Alice's library.
    fixture
        .value(
            "POST",
            collection,
            &bob,
            json!({"igdb_id":42,"rating":1}),
            StatusCode::CREATED,
        )
        .await;
    let unchanged = fixture
        .value("GET", &original_path, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(unchanged, original);

    // Editing an older review preserves its identity/recording time and recency.
    let edited = fixture
        .value(
            "PUT",
            &original_path,
            &alice,
            json!({"rating":8,"review":"Edited first experience","finished_on":"2022-04-01"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(edited["id"], original["id"]);
    assert_eq!(edited["created_at"], original["created_at"]);
    assert!(edited["platform"].is_null(), "PUT replaces nullable fields");
    let current = fixture
        .value("GET", library, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["rating"], 7);
    let preserved = fixture
        .value("GET", &replay_path, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(preserved, replay);

    // Clearing the current rating falls back; changing its experience date
    // also recomputes which rated playthrough is the newest.
    fixture
        .value(
            "PUT",
            &replay_path,
            &alice,
            json!({"review":"Replay without a rating","finished_on":"2026-02-01","rating":null}),
            StatusCode::OK,
        )
        .await;
    let current = fixture
        .value("GET", library, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["rating"], 8);
    fixture
        .value(
            "PUT",
            &replay_path,
            &alice,
            json!({"rating":6,"finished_on":"2019-01-01"}),
            StatusCode::OK,
        )
        .await;
    let current = fixture
        .value("GET", library, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["rating"], 8);

    // Removing library membership leaves all four experiences accessible.
    fixture
        .value(
            "DELETE",
            library,
            &alice,
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    let history = fixture
        .value("GET", collection, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(history["data"].as_array().unwrap().len(), 4);
    fixture
        .value(
            "PUT",
            library,
            &alice,
            json!({"status":"dropped"}),
            StatusCode::OK,
        )
        .await;
    for (id, expected) in [
        (&original["id"], json!(10)),
        (&older["id"], json!(6)),
        (&replay["id"], Value::Null),
    ] {
        let path = format!("{collection}/{}", id.as_str().unwrap());
        fixture
            .value("DELETE", &path, &alice, Value::Null, StatusCode::NO_CONTENT)
            .await;
        fixture
            .value("DELETE", &path, &alice, Value::Null, StatusCode::NO_CONTENT)
            .await;
        fixture
            .value("GET", &path, &alice, Value::Null, StatusCode::NOT_FOUND)
            .await;
        fixture
            .value("PUT", &path, &alice, json!({}), StatusCode::NOT_FOUND)
            .await;
        let current = fixture
            .value("GET", library, &alice, Value::Null, StatusCode::OK)
            .await;
        assert_eq!(current["rating"], expected);
        if expected.is_null() {
            assert!(current["rating_playthrough_id"].is_null());
        }
    }
    // Undated ratings use recording time; a later empty entry doesn't erase it.
    let undated = fixture
        .value(
            "POST",
            collection,
            &alice,
            json!({"igdb_id":42,"rating":5}),
            StatusCode::CREATED,
        )
        .await;
    assert!(undated["started_on"].is_null());
    assert!(undated["finished_on"].is_null());
    fixture
        .value(
            "POST",
            collection,
            &alice,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    let current = fixture
        .value("GET", library, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["rating"], 5);
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn playthrough_validation_rejects_bad_input_before_importing() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.user("validation").await;
    for (body, status) in [
        (json!({"igdb_id":0}), StatusCode::BAD_REQUEST),
        (json!({"igdb_id":42,"rating":11}), StatusCode::BAD_REQUEST),
        (
            json!({"igdb_id":42,"review":"x".repeat(10001)}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"igdb_id":42,"review":"bad\u{0000}text"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"igdb_id":42,"platform":" "}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"igdb_id":42,"started_on":"2026-02-01","finished_on":"2026-01-01"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"igdb_id":42,"started_on":"2026-02-30"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"igdb_id":42,"user_id":"someone-else"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"igdb_id":42,"status":"completed"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (json!({"rating":8}), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        fixture
            .value("POST", "/api/me/playthroughs", &token, body, status)
            .await;
    }
    for query in [
        "limit=0",
        "limit=51",
        "offset=10001",
        "igdb_id=0",
        "igdb_id=-1",
        "user_id=someone",
    ] {
        fixture
            .value(
                "GET",
                &format!("/api/me/playthroughs?{query}"),
                &token,
                Value::Null,
                StatusCode::BAD_REQUEST,
            )
            .await;
    }
    for method in ["GET", "PUT", "DELETE"] {
        fixture
            .value(
                method,
                "/api/me/playthroughs/not-a-uuid",
                &token,
                json!({}),
                StatusCode::BAD_REQUEST,
            )
            .await;
    }
    assert!(fixture.server.received_requests().await.unwrap().is_empty());
    let row = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &token,
            json!({"igdb_id":42,"rating":8}),
            StatusCode::CREATED,
        )
        .await;
    let path = format!("/api/me/playthroughs/{}", row["id"].as_str().unwrap());
    fixture
        .value(
            "PUT",
            &path,
            &token,
            json!({"rating":0}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    fixture
        .value(
            "PUT",
            &path,
            &token,
            json!({"igdb_id":43}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    let unchanged = fixture
        .value("GET", &path, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(unchanged, row);
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn migration_preserves_legacy_reviews_without_inventing_play_dates() {
    let fixture = Fixture::new().await;
    let (id, token) = fixture.user("legacy").await;
    fixture
        .value(
            "POST",
            "/api/catalog/import/42",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    // Restore the exact old library shape in this disposable database, then
    // exercise the real migrator, including its migration history and restart.
    sqlx::raw_sql("DROP VIEW library_entry_details; DROP TABLE playthroughs;
        ALTER TABLE library_entries ADD COLUMN rating SMALLINT CHECK (rating BETWEEN 1 AND 10),
        ADD COLUMN review TEXT CHECK (char_length(review) <= 10000);
        DELETE FROM _sqlx_migrations WHERE version=202609080002;
        INSERT INTO games (id,igdb_id,name,slug,source_url)
        SELECT gen_random_uuid(),n,'Legacy game','legacy-game','https://www.igdb.com' FROM generate_series(43,45) n;")
        .execute(&fixture.database.app).await.unwrap();
    sqlx::query(
        "INSERT INTO library_entries (user_id,game_id,status,rating,review,created_at,updated_at)
        SELECT $1,id,'dropped',CASE WHEN igdb_id IN (42,43) THEN 9 END,
        CASE WHEN igdb_id IN (42,44) THEN 'Preserved review' END,
        '2022-01-02T23:30:00Z'::timestamptz,'2026-05-06T12:00:00Z'::timestamptz
        FROM games",
    )
    .bind(id.0.as_bytes().as_slice())
    .execute(&fixture.database.app)
    .await
    .unwrap();
    let migrator = sqlx::migrate!("./migrations");
    migrator.run(&fixture.database.app).await.unwrap();
    migrator.run(&fixture.database.app).await.unwrap();
    let diary = fixture
        .value(
            "GET",
            "/api/me/playthroughs",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(diary["data"].as_array().unwrap().len(), 3);
    for entry in diary["data"].as_array().unwrap() {
        let game = entry["igdb_id"].as_i64().unwrap();
        assert_eq!(
            entry["rating"],
            if game == 44 { Value::Null } else { json!(9) }
        );
        assert_eq!(
            entry["review"],
            if game == 43 {
                Value::Null
            } else {
                json!("Preserved review")
            }
        );
        assert!(entry["started_on"].is_null());
        assert!(entry["finished_on"].is_null());
        assert!(entry["platform"].is_null());
        assert_eq!(entry["created_at"], "2022-01-02T23:30:00Z");
        assert_eq!(entry["updated_at"], "2026-05-06T12:00:00Z");
    }
    let library = fixture
        .value(
            "GET",
            "/api/me/library",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(library["data"].as_array().unwrap().len(), 4);
    for row in library["data"].as_array().unwrap() {
        assert_eq!(row["entry"]["status"], "dropped");
        assert_eq!(row["entry"]["created_at"], "2022-01-02T23:30:00Z");
        assert_eq!(row["entry"]["updated_at"], "2026-05-06T12:00:00Z");
        assert!(row["entry"].get("review").is_none());
    }
    fixture.finish().await;
}
