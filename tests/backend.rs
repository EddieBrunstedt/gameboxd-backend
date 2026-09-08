use std::str::FromStr;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use chrono::Utc;
use doorman::{
    core::storage::AccountStatus,
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
            .call("GET", "/api/catalog/search?q=Halo", None, Value::Null)
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
                json!({"status":"playing","rating":8,"review":"Worth playing"}),
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(saved["rating"], 8);
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
                json!({"status":"completed","rating":11})
            )
            .await
            .status(),
        400
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
                json!({"status":"completed","rating":null,"review":null}),
            )
            .await,
        StatusCode::OK,
    )
    .await;
    assert!(cleared["rating"].is_null());
    assert!(cleared["review"].is_null());

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
    assert_eq!((profiles, entries), (0, 0));
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
