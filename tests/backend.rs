use std::{
    net::SocketAddr,
    str::FromStr,
    time::{Duration, Instant},
};

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
    refresh::Refresher,
    router,
};
use serde_json::{Value, json};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::{task::JoinHandle, time::timeout};
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
    refresh: Option<JoinHandle<()>>,
}

impl Fixture {
    /// A fixture that runs no background metadata refresh, so every upstream
    /// request a test observes was made by the request under test.
    async fn new() -> Self {
        Self::build(false).await
    }

    /// A fixture whose background metadata refresher really runs, for the
    /// tests that assert a stale game is brought up to date after a save.
    async fn refreshing() -> Self {
        Self::build(true).await
    }

    async fn build(refresh: bool) -> Self {
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
            cookie_same_site: cookie::SameSite::Lax,
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
        let (refresher, refresh) = if refresh {
            let (refresher, worker) = Refresher::spawn(database.app.clone(), igdb.clone());
            (refresher, Some(worker))
        } else {
            (Refresher::disabled(), None)
        };
        let app = router(
            &config,
            AppState {
                pool: database.app.clone(),
                igdb,
                refresher,
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
            refresh,
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
        self.send(method, path, token, &[], body).await
    }

    /// Sends a request carrying extra headers, such as the `If-Match`
    /// precondition every playthrough edit requires.
    async fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Value,
    ) -> Response {
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
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    /// Sends a body exactly as given, with a chosen content type. `send` always
    /// serializes valid JSON, which cannot express the two failures below: a
    /// malformed document, and a body that never claimed to be JSON at all.
    async fn raw(&self, path: &str, token: &str, content_type: &str, body: &str) -> Response {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .extension(ConnectInfo(
                "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
            ))
            .header("Content-Type", content_type)
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_owned()))
            .unwrap();
        self.app.clone().oneshot(request).await.unwrap()
    }

    async fn finish(self) {
        drop(self.app);
        drop(self.auth);
        if let Some(worker) = self.refresh {
            // The router held the only queue handle, so the worker stops on its
            // own; waiting for it keeps a refresh from racing the drop below.
            timeout(Duration::from_secs(10), worker)
                .await
                .expect("refresh worker must stop when the router is dropped")
                .unwrap();
        }
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

    /// A conditional playthrough edit, asserting its status and returning the
    /// response body.
    async fn edit(
        &self,
        method: &str,
        path: &str,
        token: &str,
        etag: &str,
        body: Value,
        status: StatusCode,
    ) -> Value {
        json_body(
            self.send(method, path, Some(token), &[("If-Match", etag)], body)
                .await,
            status,
        )
        .await
    }
}

/// The entity tag naming a playthrough's current revision, in the form
/// `If-Match` expects.
fn tag(playthrough: &Value) -> String {
    format!(
        "\"{}\"",
        playthrough["version"]
            .as_i64()
            .expect("every playthrough publishes its version")
    )
}

const START_RUN: &str = "/api/me/playthroughs/start";

fn run_path(run: &Value) -> String {
    format!("/api/me/playthroughs/{}", run["id"].as_str().unwrap())
}

impl Fixture {
    async fn lifecycle(&self, token: &str, run: &Value, body: Value, status: StatusCode) -> Value {
        self.edit(
            "POST",
            &format!("{}/lifecycle", run_path(run)),
            token,
            &tag(run),
            body,
            status,
        )
        .await
    }

    async fn current_entry(&self, token: &str) -> Value {
        self.value(
            "GET",
            "/api/me/library/42",
            token,
            Value::Null,
            StatusCode::OK,
        )
        .await
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn lifecycle_preserves_history_and_supports_replay_and_corrections() {
    let f = Fixture::new().await;
    let (_, token) = f.user("lifecycle").await;
    let run = f
        .value(
            "POST",
            START_RUN,
            &token,
            json!({"igdb_id":42,"started_on":"2025-01-01","platform":" PC "}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(run["state"], "active");
    assert_eq!(run["is_current"], true);
    assert_eq!(run["platform"], "PC");
    assert!(run["presentation_on"].is_null());
    let entry = f.current_entry(&token).await;
    assert_eq!(entry["status"], "playing");
    assert_eq!(entry["current_playthrough_id"], run["id"]);
    let paused = f
        .lifecycle(&token, &run, json!({"action":"pause"}), StatusCode::OK)
        .await;
    assert_eq!(paused["state"], "paused");
    assert_eq!(f.current_entry(&token).await["status"], "on_hold");
    let active = f
        .lifecycle(&token, &paused, json!({"action":"resume"}), StatusCode::OK)
        .await;
    assert_eq!(active["state"], "active");
    // Open-ended games can be reviewed and rated without claiming completion.
    let reviewed = f
        .edit(
            "PATCH",
            &run_path(&active),
            &token,
            &tag(&active),
            json!({"rating":8,"review":"Still playing","presentation_on":"2025-02-01"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(reviewed["state"], "active");
    let ended = f
        .lifecycle(
            &token,
            &reviewed,
            json!({"action":"complete","finished_on":"2025-02-01"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(ended["state"], "completed");
    assert_eq!(ended["is_current"], false);
    assert_eq!(f.current_entry(&token).await["status"], "completed");
    assert!(f.current_entry(&token).await["current_playthrough_id"].is_null());
    let replay = f
        .value(
            "POST",
            START_RUN,
            &token,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    assert_ne!(replay["id"], ended["id"]);
    assert!(replay["started_on"].is_null());
    let entry_before = f.current_entry(&token).await;
    let corrected = f
        .edit(
            "PATCH",
            &run_path(&ended),
            &token,
            &tag(&ended),
            json!({"state":"abandoned","finished_on":"2025-03-01","started_on":null}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(corrected["state"], "abandoned");
    assert_eq!(corrected["rating"], 8);
    assert_eq!(corrected["review"], "Still playing");
    assert_eq!(corrected["presentation_on"], ended["presentation_on"]);
    f.value(
        "POST",
        "/api/me/playthroughs",
        &token,
        json!({"igdb_id":42,"state":"completed","finished_on":"2010-01-01"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(f.current_entry(&token).await, entry_before);
    // PUT can freely correct a historical outcome, including unknown/open states.
    let corrected = f.edit("PUT", &run_path(&corrected), &token, &tag(&corrected),
        json!({"state":"paused","rating":8,"review":"Still playing","presentation_on":"2025-02-01"}), StatusCode::OK).await;
    assert_eq!(corrected["is_current"], false);
    assert_eq!(f.current_entry(&token).await, entry_before);
    let abandoned = f
        .lifecycle(&token, &replay, json!({"action":"abandon"}), StatusCode::OK)
        .await;
    assert_eq!(abandoned["state"], "abandoned");
    assert!(abandoned["finished_on"].is_null());
    assert_eq!(f.current_entry(&token).await["status"], "dropped");
    assert_eq!(f.current_entry(&token).await["rating"], 8);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn lifecycle_enforces_ownership_preconditions_and_current_status() {
    let f = Fixture::new().await;
    let (_, token) = f.user("lifecycle-owner").await;
    let (_, other) = f.user("lifecycle-other").await;
    let run = f
        .value(
            "POST",
            START_RUN,
            &token,
            json!({"igdb_id":42,"started_on":"2025-01-01"}),
            StatusCode::CREATED,
        )
        .await;
    let action_path = format!("{}/lifecycle", run_path(&run));
    assert_eq!(
        f.call("POST", START_RUN, None, json!({"igdb_id":42}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.call("POST", &action_path, None, json!({"action":"pause"}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    f.lifecycle(
        &other,
        &run,
        json!({"action":"pause"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    f.edit(
        "PATCH",
        &run_path(&run),
        &other,
        &tag(&run),
        json!({"state":"completed"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    f.value(
        "POST",
        &action_path,
        &token,
        json!({"action":"pause"}),
        StatusCode::PRECONDITION_REQUIRED,
    )
    .await;
    f.lifecycle(
        &token,
        &run,
        json!({"action":"resume"}),
        StatusCode::CONFLICT,
    )
    .await;
    f.lifecycle(
        &token,
        &run,
        json!({"action":"complete","finished_on":"2024-01-01"}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    f.lifecycle(
        &token,
        &run,
        json!({"action":"pause","finished_on":"2025-01-02"}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    f.value(
        "POST",
        START_RUN,
        &token,
        json!({"igdb_id":42}),
        StatusCode::CONFLICT,
    )
    .await;
    f.value(
        "PUT",
        "/api/me/library/42",
        &token,
        json!({"status":"completed"}),
        StatusCode::CONFLICT,
    )
    .await;
    f.edit(
        "PATCH",
        &run_path(&run),
        &token,
        &tag(&run),
        json!({"state":null}),
        StatusCode::CONFLICT,
    )
    .await;
    f.edit(
        "PATCH",
        &run_path(&run),
        &token,
        &tag(&run),
        json!({"finished_on":"2025-01-02"}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    // Every refusal leaves the row and its revision unchanged.
    assert_eq!(
        f.value("GET", &run_path(&run), &token, Value::Null, StatusCode::OK)
            .await,
        run
    );
    let corrected = f
        .edit(
            "PATCH",
            &run_path(&run),
            &token,
            &tag(&run),
            json!({"started_on":"2020-01-01"}),
            StatusCode::OK,
        )
        .await;
    f.lifecycle(
        &token,
        &run,
        json!({"action":"pause"}),
        StatusCode::PRECONDITION_FAILED,
    )
    .await;
    let paused = f
        .lifecycle(
            &token,
            &corrected,
            json!({"action":"pause"}),
            StatusCode::OK,
        )
        .await;
    f.value(
        "POST",
        START_RUN,
        &token,
        json!({"igdb_id":42}),
        StatusCode::CONFLICT,
    )
    .await;
    let ended = f
        .lifecycle(
            &token,
            &paused,
            json!({"action":"complete"}),
            StatusCode::OK,
        )
        .await;
    f.lifecycle(
        &token,
        &ended,
        json!({"action":"resume"}),
        StatusCode::CONFLICT,
    )
    .await;
    // Independent users and different games have independent current runs.
    f.value(
        "POST",
        START_RUN,
        &other,
        json!({"igdb_id":42}),
        StatusCode::CREATED,
    )
    .await;
    sqlx::query("INSERT INTO games(id,igdb_id,name,slug,source_url) VALUES ($1,43,'Other','other','https://example.test')")
        .bind(Uuid::new_v4()).execute(&f.database.app).await.unwrap();
    f.value(
        "POST",
        START_RUN,
        &other,
        json!({"igdb_id":43}),
        StatusCode::CREATED,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn library_removal_detaches_current_run_and_preserves_history() {
    let f = Fixture::new().await;
    let (_, token) = f.user("lifecycle-remove").await;
    f.value(
        "PUT",
        "/api/me/library/42",
        &token,
        json!({"status":"playing"}),
        StatusCode::OK,
    )
    .await;
    let history = f
        .value(
            "GET",
            "/api/me/playthroughs",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        history["data"],
        json!([]),
        "lightweight status creates no history"
    );
    let run = f
        .value(
            "POST",
            START_RUN,
            &token,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    let run = f
        .edit(
            "PATCH",
            &run_path(&run),
            &token,
            &tag(&run),
            json!({"review":"Keep this","rating":9}),
            StatusCode::OK,
        )
        .await;
    f.value(
        "DELETE",
        "/api/me/library/42",
        &token,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    f.value(
        "GET",
        "/api/me/library/42",
        &token,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let detached = f
        .value("GET", &run_path(&run), &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(detached["state"], "active");
    assert_eq!(detached["review"], "Keep this");
    assert_eq!(detached["rating"], 9);
    assert_eq!(detached["is_current"], false);
    assert_ne!(tag(&detached), tag(&run));
    f.lifecycle(
        &token,
        &run,
        json!({"action":"pause"}),
        StatusCode::PRECONDITION_FAILED,
    )
    .await;
    f.lifecycle(
        &token,
        &detached,
        json!({"action":"pause"}),
        StatusCode::CONFLICT,
    )
    .await;
    let next = f
        .value(
            "POST",
            START_RUN,
            &token,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(f.current_entry(&token).await["rating"], 9);
    f.edit(
        "DELETE",
        &run_path(&next),
        &token,
        &tag(&next),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let entry = f.current_entry(&token).await;
    assert_eq!(
        entry["status"], "playing",
        "deleting a run leaves lightweight status"
    );
    assert!(entry["current_playthrough_id"].is_null());
    f.value(
        "PUT",
        "/api/me/library/42",
        &token,
        json!({"status":"backlog"}),
        StatusCode::OK,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn coordinated_lifecycle_writes_roll_back_on_database_failure() {
    let f = Fixture::new().await;
    let (_, token) = f.user("lifecycle-rollback").await;
    f.value(
        "PUT",
        "/api/me/library/42",
        &token,
        json!({"status":"backlog"}),
        StatusCode::OK,
    )
    .await;
    let before = f.current_entry(&token).await;
    // Fail the second half, after the playthrough INSERT/UPDATE has succeeded.
    sqlx::raw_sql(
        "CREATE FUNCTION reject_test_library_write() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected library failure'; END; $$;
        CREATE TRIGGER reject_test_library_write BEFORE INSERT OR UPDATE ON library_entries
        FOR EACH ROW EXECUTE FUNCTION reject_test_library_write();",
    )
    .execute(&f.database.app)
    .await
    .unwrap();
    f.value(
        "POST",
        START_RUN,
        &token,
        json!({"igdb_id":42}),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert_eq!(f.current_entry(&token).await, before);
    assert_eq!(
        f.value(
            "GET",
            "/api/me/playthroughs",
            &token,
            Value::Null,
            StatusCode::OK
        )
        .await["data"],
        json!([])
    );
    sqlx::query("ALTER TABLE library_entries DISABLE TRIGGER reject_test_library_write")
        .execute(&f.database.app)
        .await
        .unwrap();
    let run = f
        .value(
            "POST",
            START_RUN,
            &token,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    let before = f.current_entry(&token).await;
    sqlx::query("ALTER TABLE library_entries ENABLE TRIGGER reject_test_library_write")
        .execute(&f.database.app)
        .await
        .unwrap();
    f.lifecycle(
        &token,
        &run,
        json!({"action":"complete"}),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert_eq!(
        f.value("GET", &run_path(&run), &token, Value::Null, StatusCode::OK)
            .await,
        run
    );
    assert_eq!(f.current_entry(&token).await, before);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn conflicting_lifecycle_requests_cannot_split_current_state() {
    let f = Fixture::new().await;
    let (_, token) = f.user("lifecycle-races").await;
    f.value(
        "POST",
        "/api/catalog/import/42",
        &token,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let (a, b) = tokio::join!(
        f.call("POST", START_RUN, Some(&token), json!({"igdb_id":42})),
        f.call("POST", START_RUN, Some(&token), json!({"igdb_id":42})),
    );
    let (success, conflict) = if a.status() == StatusCode::CREATED {
        (a, b)
    } else {
        (b, a)
    };
    let run = json_body(success, StatusCode::CREATED).await;
    json_body(conflict, StatusCode::CONFLICT).await;
    let path = format!("{}/lifecycle", run_path(&run));
    let etag = tag(&run);
    let headers = [("If-Match", etag.as_str())];
    let (a, b) = tokio::join!(
        f.send(
            "POST",
            &path,
            Some(&token),
            &headers,
            json!({"action":"pause"})
        ),
        f.send(
            "POST",
            &path,
            Some(&token),
            &headers,
            json!({"action":"complete"})
        ),
    );
    let (success, stale) = if a.status() == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    let winner = json_body(success, StatusCode::OK).await;
    json_body(stale, StatusCode::PRECONDITION_FAILED).await;
    let entry = f.current_entry(&token).await;
    if winner["state"] == "paused" {
        assert_eq!(entry["status"], "on_hold");
        assert_eq!(entry["current_playthrough_id"], winner["id"]);
        f.lifecycle(
            &token,
            &winner,
            json!({"action":"complete"}),
            StatusCode::OK,
        )
        .await;
    } else {
        assert_eq!(entry["status"], "completed");
        assert!(entry["current_playthrough_id"].is_null());
    }
    // A concurrent status-only save either commits before start or conflicts.
    let (start, status) = tokio::join!(
        f.call("POST", START_RUN, Some(&token), json!({"igdb_id":42})),
        f.call(
            "PUT",
            "/api/me/library/42",
            Some(&token),
            json!({"status":"backlog"})
        ),
    );
    let run = json_body(start, StatusCode::CREATED).await;
    assert!(matches!(
        status.status(),
        StatusCode::OK | StatusCode::CONFLICT
    ));
    assert_eq!(f.current_entry(&token).await["status"], "playing");
    let path = format!("{}/lifecycle", run_path(&run));
    let etag = tag(&run);
    let headers = [("If-Match", etag.as_str())];
    let (remove, action) = tokio::join!(
        f.call("DELETE", "/api/me/library/42", Some(&token), Value::Null),
        f.send(
            "POST",
            &path,
            Some(&token),
            &headers,
            json!({"action":"pause"})
        ),
    );
    assert_eq!(remove.status(), StatusCode::NO_CONTENT);
    assert!(matches!(
        action.status(),
        StatusCode::OK | StatusCode::PRECONDITION_FAILED
    ));
    f.value(
        "GET",
        "/api/me/library/42",
        &token,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        f.value("GET", &run_path(&run), &token, Value::Null, StatusCode::OK)
            .await["is_current"],
        false
    );
    f.finish().await;
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
    // An entity tag is useless to the SPA unless the browser lets it be read.
    assert!(
        response.headers()["access-control-expose-headers"]
            .to_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("etag")
    );
    json_body(response, StatusCode::OK).await;

    // Conditional playthrough edits must survive the browser's preflight.
    let preflight = Request::builder()
        .method("OPTIONS")
        .uri("/api/me/playthroughs/00000000-0000-0000-0000-000000000000")
        .header("Origin", "http://localhost:5173")
        .header("Access-Control-Request-Method", "PATCH")
        .header("Access-Control-Request-Headers", "content-type, if-match")
        .body(Body::empty())
        .unwrap();
    let allowed = fixture.app.clone().oneshot(preflight).await.unwrap();
    assert_eq!(allowed.status(), 200);
    let headers = allowed.headers()["access-control-allow-headers"]
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(headers.contains("if-match"), "{headers}");

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
        json!({"igdb_id":42,"rating":9,"review":"First experience","finished_on":"2022-04-01","presentation_on":"2022-04-01","platform":" PC "}),
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
        json!({"igdb_id":42,"rating":7,"review":"Replay","started_on":"2026-01-01","finished_on":"2026-02-01","presentation_on":"2026-02-01"}),
        StatusCode::CREATED).await;
    // Backfilling an old experience must not replace the newer rating.
    let older = fixture
        .value(
            "POST",
            collection,
            &alice,
            json!({"igdb_id":42,"rating":10,"finished_on":"2020-01-01","presentation_on":"2020-01-01"}),
            StatusCode::CREATED,
        )
        .await;
    let unrated = fixture
        .value(
            "POST",
            collection,
            &alice,
            json!({"igdb_id":42,"review":"Still playing","started_on":"2026-03-01","presentation_on":"2026-03-01"}),
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
    for method in ["GET", "PUT", "PATCH", "DELETE"] {
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
    // Ownership is settled before the precondition, so holding a valid entity
    // tag for someone else's playthrough neither edits it nor confirms it.
    for method in ["PUT", "PATCH"] {
        fixture
            .edit(
                method,
                &original_path,
                &bob,
                &tag(&original),
                json!({"rating":1}),
                StatusCode::NOT_FOUND,
            )
            .await;
    }
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
        .edit(
            "PUT",
            &original_path,
            &alice,
            &tag(&original),
            json!({"rating":8,"review":"Edited first experience","finished_on":"2022-04-01","presentation_on":"2022-04-01"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(edited["id"], original["id"]);
    assert_eq!(edited["version"], 2, "an edit is a new revision");
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
    let unrated_replay = fixture
        .edit(
            "PUT",
            &replay_path,
            &alice,
            &tag(&replay),
            json!({"review":"Replay without a rating","finished_on":"2026-02-01","presentation_on":"2026-02-01","rating":null}),
            StatusCode::OK,
        )
        .await;
    let current = fixture
        .value("GET", library, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["rating"], 8);
    fixture
        .edit(
            "PUT",
            &replay_path,
            &alice,
            &tag(&unrated_replay),
            json!({"rating":6,"finished_on":"2019-01-01","presentation_on":"2019-01-01"}),
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
        // Not even `*` resurrects a deleted playthrough.
        fixture
            .edit("PUT", &path, &alice, "*", json!({}), StatusCode::NOT_FOUND)
            .await;
        let current = fixture
            .value("GET", library, &alice, Value::Null, StatusCode::OK)
            .await;
        assert_eq!(current["rating"], expected);
        if expected.is_null() {
            assert!(current["rating_playthrough_id"].is_null());
        }
    }
    // An undated rating remains available when no dated ratings exist; a later empty entry doesn't erase it.
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
        (
            json!({"igdb_id":42,"presentation_on":"2025-02-29"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"igdb_id":42,"presentation_on":"2025-09-09T00:00:00Z"}),
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
    for method in ["GET", "PUT", "PATCH", "DELETE"] {
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
    for method in ["PUT", "PATCH"] {
        fixture
            .edit(
                method,
                &path,
                &token,
                &tag(&row),
                json!({"rating":0}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        // The game a playthrough records cannot be changed by either route.
        fixture
            .edit(
                method,
                &path,
                &token,
                &tag(&row),
                json!({"igdb_id":43}),
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await;
    }
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
        DROP FUNCTION playthrough_revision();
        ALTER TABLE library_entries ADD COLUMN rating SMALLINT CHECK (rating BETWEEN 1 AND 10),
        ADD COLUMN review TEXT CHECK (char_length(review) <= 10000);
        DELETE FROM _sqlx_migrations WHERE version IN (202609080002,202609080003,202609090001,202609090002);
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
        assert!(entry["presentation_on"].is_null());
        assert!(entry["started_on"].is_null());
        assert!(entry["finished_on"].is_null());
        assert!(entry["platform"].is_null());
        assert_eq!(entry["created_at"], "2022-01-02T23:30:00Z");
        assert_eq!(entry["updated_at"], "2026-05-06T12:00:00Z");
        // A converted review is a first revision, editable without a prior read.
        assert_eq!(entry["version"], 1);
    }
    let converted = &diary["data"][0];
    let edited = fixture
        .edit(
            "PATCH",
            &format!("/api/me/playthroughs/{}", converted["id"].as_str().unwrap()),
            &token,
            &tag(converted),
            json!({"platform":"PC"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(edited["version"], 2);
    assert_eq!(edited["review"], converted["review"]);
    assert_ne!(edited["updated_at"], converted["updated_at"]);
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

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn partial_playthrough_edits_reject_stale_and_unconditional_writes() {
    let fixture = Fixture::new().await;
    let (_, alice) = fixture.user("editing-alice").await;
    let (_, bob) = fixture.user("editing-bob").await;
    let collection = "/api/me/playthroughs";
    let library = "/api/me/library/42";

    let response = fixture
        .send(
            "POST",
            collection,
            Some(&alice),
            &[],
            json!({"igdb_id":42,"rating":9,"review":"First pass","started_on":"2026-01-01","platform":"PC"}),
        )
        .await;
    assert_eq!(response.headers()["etag"], "\"1\"");
    let created = json_body(response, StatusCode::CREATED).await;
    assert_eq!(created["version"], 1);
    let path = format!("{collection}/{}", created["id"].as_str().unwrap());
    let response = fixture.call("GET", &path, Some(&alice), Value::Null).await;
    assert_eq!(response.headers()["etag"], tag(&created));
    json_body(response, StatusCode::OK).await;

    // An edit that names no revision is refused, because allowing it is exactly
    // the lost update the entity tag exists to prevent.
    for method in ["PUT", "PATCH"] {
        let error = fixture
            .value(
                method,
                &path,
                &alice,
                json!({"review":"Sent without a precondition"}),
                StatusCode::PRECONDITION_REQUIRED,
            )
            .await;
        assert_eq!(error["error"]["code"], "precondition_required");
    }
    // A precondition this server cannot evaluate is a bad request, not a free pass.
    for malformed in ["", "  ", "1", "\"1", "*, \"1\"", "\"1\","] {
        fixture
            .edit(
                "PATCH",
                &path,
                &alice,
                malformed,
                json!({"review":"Sent with a malformed precondition"}),
                StatusCode::BAD_REQUEST,
            )
            .await;
    }
    // `If-Match` compares strongly, so a weak tag can never satisfy it.
    let error = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            "W/\"1\"",
            json!({"review":"Sent with a weak tag"}),
            StatusCode::PRECONDITION_FAILED,
        )
        .await;
    assert_eq!(error["error"]["code"], "precondition_failed");
    assert_eq!(
        fixture
            .value("GET", &path, &alice, Value::Null, StatusCode::OK)
            .await,
        created,
        "a refused edit changes nothing"
    );

    // A partial edit touches only the fields it names.
    let response = fixture
        .send(
            "PATCH",
            &path,
            Some(&alice),
            &[("If-Match", &tag(&created))],
            json!({"review":"Second thoughts"}),
        )
        .await;
    assert_eq!(response.headers()["etag"], "\"2\"");
    let patched = json_body(response, StatusCode::OK).await;
    assert_eq!(patched["review"], "Second thoughts");
    assert_eq!(patched["rating"], 9);
    assert_eq!(patched["platform"], "PC");
    assert_eq!(patched["started_on"], "2026-01-01");
    assert_eq!(patched["id"], created["id"]);
    assert_eq!(patched["created_at"], created["created_at"]);
    assert_eq!(patched["version"], 2);
    for method in ["PUT", "PATCH"] {
        fixture
            .edit(
                method,
                &path,
                &alice,
                &tag(&created),
                json!({"review":"Sent with the superseded tag"}),
                StatusCode::PRECONDITION_FAILED,
            )
            .await;
    }

    // The review's motivating case: two tabs read one revision, and the second
    // is told to reload rather than silently discarding the first tab's work.
    let read_by_both = fixture
        .value("GET", &path, &alice, Value::Null, StatusCode::OK)
        .await;
    let rated = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&read_by_both),
            json!({"rating":7}),
            StatusCode::OK,
        )
        .await;
    fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&read_by_both),
            json!({"review":"Written in a stale tab"}),
            StatusCode::PRECONDITION_FAILED,
        )
        .await;
    let merged = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&rated),
            json!({"review":"Written after reloading"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(merged["rating"], 7, "the other tab's rating survived");
    assert_eq!(merged["review"], "Written after reloading");

    // Explicit null is the only way a partial edit clears a field.
    let cleared = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&merged),
            json!({"platform":null,"started_on":null}),
            StatusCode::OK,
        )
        .await;
    assert!(cleared["platform"].is_null());
    assert!(cleared["started_on"].is_null());
    assert_eq!(cleared["rating"], 7);
    assert_eq!(cleared["review"], "Written after reloading");
    // A misspelled field is refused rather than quietly changing nothing.
    for unknown in [json!({"reviews":"typo"}), json!({"igdb_id":43})] {
        fixture
            .edit(
                "PATCH",
                &path,
                &alice,
                &tag(&cleared),
                unknown,
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await;
    }

    // Cross-field rules are checked against the merged record, not the body.
    let dated = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&cleared),
            json!({"started_on":"2026-05-01","platform":" Steam Deck "}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(dated["platform"], "Steam Deck");
    fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&dated),
            json!({"finished_on":"2026-04-01"}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    let finished = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&dated),
            json!({"finished_on":"2026-06-01"}),
            StatusCode::OK,
        )
        .await;

    // Re-saving an unchanged form is not a revision, so it neither bumps the
    // timestamp nor invalidates a tag another tab is legitimately holding.
    assert_eq!(
        fixture
            .edit(
                "PATCH",
                &path,
                &alice,
                &tag(&finished),
                json!({}),
                StatusCode::OK
            )
            .await,
        finished
    );
    assert_eq!(
        fixture
            .edit(
                "PUT",
                &path,
                &alice,
                &tag(&finished),
                json!({"rating":finished["rating"],"review":finished["review"],
                       "started_on":finished["started_on"],"finished_on":finished["finished_on"],
                       "platform":finished["platform"]}),
                StatusCode::OK,
            )
            .await,
        finished
    );

    // `*` asserts only that the playthrough still exists.
    let starred = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            "*",
            json!({"rating":10}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        starred["version"].as_i64().unwrap(),
        finished["version"].as_i64().unwrap() + 1
    );

    // Edits keep flowing through to the derived library rating.
    fixture
        .value(
            "PUT",
            library,
            &alice,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    let entry = fixture
        .value("GET", library, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(entry["rating"], 10);
    assert_eq!(entry["rating_playthrough_id"], created["id"]);
    let unrated = fixture
        .edit(
            "PATCH",
            &path,
            &alice,
            &tag(&starred),
            json!({"rating":null}),
            StatusCode::OK,
        )
        .await;
    let entry = fixture
        .value("GET", library, &alice, Value::Null, StatusCode::OK)
        .await;
    assert!(entry["rating"].is_null());
    assert!(entry["rating_playthrough_id"].is_null());
    // Listings carry the version, so an editor never needs a second read first.
    let listed = fixture
        .value("GET", collection, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(listed["data"][0], unrated);

    // Simultaneous edits to different fields are serialized by the row lock, so
    // the later one merges into the committed result rather than into the value
    // it read before the other edit landed.
    let (review, rating) = tokio::join!(
        fixture.send(
            "PATCH",
            &path,
            Some(&alice),
            &[("If-Match", "*")],
            json!({"review":"Written at the same moment"}),
        ),
        fixture.send(
            "PATCH",
            &path,
            Some(&alice),
            &[("If-Match", "*")],
            json!({"rating":4}),
        ),
    );
    assert_eq!(review.status(), StatusCode::OK);
    assert_eq!(rating.status(), StatusCode::OK);
    let current = fixture
        .value("GET", &path, &alice, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["review"], "Written at the same moment");
    assert_eq!(current["rating"], 4);
    assert_eq!(
        current["version"].as_i64().unwrap(),
        unrated["version"].as_i64().unwrap() + 2,
        "both edits are revisions"
    );

    // A conditional deletion asserts a revision; only an unconditional one is
    // idempotent.
    for (token, etag) in [(&alice, tag(&starred)), (&bob, tag(&current))] {
        fixture
            .edit(
                "DELETE",
                &path,
                token,
                &etag,
                Value::Null,
                StatusCode::PRECONDITION_FAILED,
            )
            .await;
    }
    fixture
        .value("GET", &path, &alice, Value::Null, StatusCode::OK)
        .await;
    fixture
        .edit(
            "DELETE",
            &path,
            &alice,
            &tag(&current),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    fixture
        .edit(
            "DELETE",
            &path,
            &alice,
            "*",
            Value::Null,
            StatusCode::PRECONDITION_FAILED,
        )
        .await;
    fixture
        .value("DELETE", &path, &alice, Value::Null, StatusCode::NO_CONTENT)
        .await;
    fixture.finish().await;
}

/// Field-level validation errors, as a form consumes them.
///
/// The envelope gained `fields` without changing any status or code that was
/// already published, so this covers both halves: the errors our own checks
/// raise, and the ones serde raises before a handler runs.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn validation_errors_name_the_fields_a_form_must_mark() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.user("fields").await;

    /// The `(field, code)` pairs of one error response, in order.
    fn reported(body: &Value) -> Vec<(&str, &str)> {
        body["error"]["fields"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|field| {
                (
                    field["field"].as_str().expect("a field name"),
                    field["code"].as_str().expect("a field code"),
                )
            })
            .collect()
    }

    // Every broken field of one submission comes back together, so the form
    // does not reveal its problems one refused save at a time.
    let invalid = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &token,
            json!({
                "igdb_id": 42,
                "rating": 11,
                "started_on": "2026-02-01",
                "finished_on": "2026-01-01",
                "platform": "   ",
            }),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(invalid["error"]["code"], "validation_error");
    assert_eq!(
        reported(&invalid),
        [
            ("rating", "out_of_range"),
            ("finished_on", "before_start"),
            ("platform", "required"),
        ]
    );
    // The summary stays populated for a caller that renders no field detail.
    assert_eq!(invalid["error"]["message"], "3 fields are invalid");

    // Schema failures name their field too, keeping the status and code they
    // have always had rather than being reclassified.
    for (body, field, code) in [
        (json!({}), "igdb_id", "required"),
        (
            json!({"igdb_id": 42, "user_id": "someone-else"}),
            "user_id",
            "unknown_field",
        ),
        (json!({"igdb_id": "forty-two"}), "igdb_id", "invalid_format"),
        // A date that is a string but not a real day belongs to the date
        // picker that sent it, not to the document.
        (
            json!({"igdb_id": 42, "started_on": "2026-02-30"}),
            "started_on",
            "invalid_format",
        ),
    ] {
        let rejected = fixture
            .value(
                "POST",
                "/api/me/playthroughs",
                &token,
                body.clone(),
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await;
        assert_eq!(rejected["error"]["code"], "invalid_json", "{body}");
        assert_eq!(reported(&rejected), [(field, code)], "{body}");
    }

    // A document that never parsed has no field to blame, and says so by
    // omitting `fields` entirely rather than sending an empty array.
    let malformed = json_body(
        fixture
            .raw(
                "/api/me/playthroughs",
                &token,
                "application/json",
                r#"{"igdb_id":42,"#,
            )
            .await,
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(malformed["error"]["code"], "invalid_json");
    assert_eq!(malformed["error"]["fields"], Value::Null);

    // A body that never claimed to be JSON is refused before parsing.
    let wrong_type = json_body(
        fixture
            .raw(
                "/api/me/playthroughs",
                &token,
                "text/plain",
                r#"{"igdb_id":42}"#,
            )
            .await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
    )
    .await;
    assert_eq!(wrong_type["error"]["code"], "invalid_json");

    // The `+json` suffix family is still JSON.
    let suffixed = json_body(
        fixture
            .raw(
                "/api/me/playthroughs",
                &token,
                "application/merge-patch+json; charset=utf-8",
                r#"{"igdb_id":42,"rating":11}"#,
            )
            .await,
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(reported(&suffixed), [("rating", "out_of_range")]);

    // Profiles report both of their fields at once as well.
    let profile = fixture
        .value(
            "PUT",
            "/api/me",
            &token,
            json!({"display_name": "n".repeat(81), "bio": "b".repeat(1001)}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(
        reported(&profile),
        [("display_name", "too_long"), ("bio", "too_long")]
    );

    // Failures that belong to no field carry none: a path, a query parameter,
    // and a missing resource all keep the shape they had before.
    for (method, path, body, status) in [
        (
            "POST",
            "/api/me/playthroughs",
            json!({"igdb_id": 0}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "GET",
            "/api/me/library?limit=0",
            Value::Null,
            StatusCode::BAD_REQUEST,
        ),
        (
            "GET",
            "/api/me/playthroughs/00000000-0000-0000-0000-000000000000",
            Value::Null,
            StatusCode::NOT_FOUND,
        ),
    ] {
        let error = fixture.value(method, path, &token, body, status).await;
        assert_eq!(error["error"]["fields"], Value::Null, "{path}");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "{path}"
        );
    }

    // None of this reached the provider: every rejection happened first.
    assert!(fixture.server.received_requests().await.unwrap().is_empty());
    fixture.finish().await;
}

/// How many game lookups the provider has been asked for. Token fetches are
/// excluded: they are cached and their timing says nothing about whether a
/// request consulted upstream.
async fn provider_lookups(fixture: &Fixture) -> usize {
    fixture
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/games")
        .count()
}

/// Saving a game that is already stored must not depend on IGDB at all, and
/// must not quietly fall back to it either: a provider outage is invisible to
/// the save, and only a game that was never stored still needs upstream.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn saving_a_stored_game_never_waits_for_the_provider() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.user("offline").await;
    fixture
        .value(
            "POST",
            "/api/catalog/import/42",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    // Age the stored metadata past the refresh interval, which under the old
    // policy is exactly what made a save wait for a provider round trip.
    sqlx::query("UPDATE games SET name='Stored title', refreshed_at=now()-interval '2 days'")
        .execute(&fixture.database.app)
        .await
        .unwrap();
    let stale = json_body(
        fixture
            .call("GET", "/api/games/42", None, Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;

    // Every upstream call from here on fails.
    fixture.server.reset().await;
    Mock::given(path("/games"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&fixture.server)
        .await;
    let before = provider_lookups(&fixture).await;

    let entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &token,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(entry["status"], "playing");
    let playthrough = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &token,
            json!({"igdb_id":42,"rating":9,"review":"Saved during an outage"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(playthrough["igdb_id"], 42);
    let entry = fixture
        .value(
            "GET",
            "/api/me/library/42",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(entry["rating"], 9, "the save attached to the stored game");
    assert_eq!(
        provider_lookups(&fixture).await,
        before,
        "a stored game must be reused without an upstream lookup"
    );

    // The saves reused the stale row untouched; with no worker running, nothing
    // else may write it either.
    assert_eq!(
        json_body(
            fixture
                .call("GET", "/api/games/42", None, Value::Null)
                .await,
            StatusCode::OK
        )
        .await,
        stale
    );

    // An explicit import is the separate operation that does wait for a
    // refresh, and it still falls back to the stored row when that fails.
    let imported = fixture
        .value(
            "POST",
            "/api/catalog/import/42",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(imported["name"], "Stored title");

    // A game that was never stored has nothing to attach a record to, so it
    // still reports the outage rather than inventing a local row.
    for (method, route, body) in [
        ("PUT", "/api/me/library/7", json!({"status":"backlog"})),
        ("POST", "/api/me/playthroughs", json!({"igdb_id":7})),
    ] {
        let error = fixture
            .value(method, route, &token, body, StatusCode::BAD_GATEWAY)
            .await;
        assert_eq!(error["error"]["code"], "igdb_unavailable");
    }
    assert_eq!(
        provider_lookups(&fixture).await - before,
        3,
        "only the import and the two unknown games reached the provider"
    );
    let (games, entries): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM games), (SELECT count(*) FROM library_entries)",
    )
    .fetch_one(&fixture.database.app)
    .await
    .unwrap();
    assert_eq!((games, entries), (1, 1), "a failed save stores nothing");
    fixture.finish().await;
}

/// The refresh a save no longer waits for still happens, just out of band, and
/// on a row the save's own records stay attached to.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn a_save_queues_the_refresh_it_no_longer_waits_for() {
    let fixture = Fixture::refreshing().await;
    let (_, token) = fixture.user("refresh").await;
    let imported = fixture
        .value(
            "POST",
            "/api/catalog/import/42",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let local_id = imported["id"].clone();
    sqlx::query("UPDATE games SET name='Stored title', refreshed_at=now()-interval '2 days'")
        .execute(&fixture.database.app)
        .await
        .unwrap();

    // Every upstream answer from here on is slow. The delay is what separates
    // a refresh that happens later from one the save is still waiting for.
    let provider_delay = Duration::from_secs(5);
    fixture.server.reset().await;
    Mock::given(path("/games"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(provider_delay)
                .set_body_json(json!([{
                    "id":42,"name":"Refreshed title","slug":"test-game","summary":"A fixture game",
                    "url":"https://www.igdb.com/games/test-game"
                }])),
        )
        .mount(&fixture.server)
        .await;

    let started = Instant::now();
    let entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &token,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(entry["status"], "playing");
    assert!(
        started.elapsed() < provider_delay,
        "the save answered only after the provider would have"
    );
    // It answered from the stale row, too: the refresh has not landed yet.
    let (pending,): (String,) = sqlx::query_as("SELECT name FROM games WHERE igdb_id=42")
        .fetch_one(&fixture.database.app)
        .await
        .unwrap();
    assert_eq!(pending, "Stored title");

    // The refresh runs on its own schedule, so wait for its effect rather than
    // for a fixed interval.
    let refreshed = timeout(provider_delay * 4, async {
        loop {
            let (name, fresh): (String, bool) = sqlx::query_as(
                "SELECT name, refreshed_at > now()-interval '1 hour' FROM games WHERE igdb_id=42",
            )
            .fetch_one(&fixture.database.app)
            .await
            .unwrap();
            if fresh {
                return name;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a stale save must queue a background refresh");
    assert_eq!(refreshed, "Refreshed title");

    // The refresh updated the row in place, so the entry saved a moment before
    // it still points at the same game.
    let game = json_body(
        fixture
            .call("GET", "/api/games/42", None, Value::Null)
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(game["id"], local_id);
    assert_eq!(
        fixture
            .call("GET", "/api/me/library/42", Some(&token), Value::Null)
            .await
            .status(),
        200
    );

    // The game is current now, so a further save queues nothing at all: the
    // count below can no longer grow.
    fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &token,
            json!({"status":"completed"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        provider_lookups(&fixture).await,
        1,
        "one refresh, and only for the stale save"
    );
    fixture.finish().await;
}

/// Application routes reject with Doorman's codes, not the application's.
///
/// This is the contract the SPA's sign-out branch is written against, and until
/// now nothing pinned it: the existing tests assert `401` and stop, so the
/// published `ErrorCode` union could — and did — omit every code these routes
/// actually return when a session is missing. `contract/errors.ts` names them in
/// `AuthCode`, and `tests/contract.rs` holds that union to Doorman's own
/// `classify`; this test is the other half, fixing which of those codes an
/// `/api` route really produces.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn application_routes_reject_with_doorman_authentication_codes() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.user("codes").await;

    // Every authenticated application route, with no credentials at all.
    for (method, path) in [
        ("GET", "/api/me"),
        ("PUT", "/api/me"),
        ("GET", "/api/me/library"),
        ("PUT", "/api/me/library/42"),
        ("GET", "/api/me/playthroughs"),
        ("POST", "/api/me/playthroughs"),
        ("POST", "/api/catalog/import/42"),
    ] {
        let body = json_body(
            fixture.call(method, path, None, json!({})).await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert_eq!(
            body["error"]["code"], "session_not_found",
            "{method} {path} should reject with Doorman's session code"
        );
        // The auth layers share the error envelope but never populate `fields`;
        // a client branching on its presence must not see one here.
        assert!(
            body["error"].get("fields").is_none(),
            "{method} {path} should carry no field errors"
        );
    }

    // A token that never existed is not distinguishable from an expired one.
    let unknown = json_body(
        fixture
            .call("GET", "/api/me", Some("not-a-real-token"), Value::Null)
            .await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(unknown["error"]["code"], "session_not_found");

    // A revoked session reads the same way. This is why `isSignedOut` in the
    // contract accepts `session_not_found` as well as `session_expired`: the
    // session layer attaches no context for a session it could not validate, so
    // the `AuthUser` extractor cannot tell "expired" from "never existed", and
    // an SPA branching only on `session_expired` would never fire.
    assert_eq!(
        fixture
            .call("POST", "/auth/logout", Some(&token), json!({}))
            .await
            .status(),
        204
    );
    let revoked = json_body(
        fixture
            .call("GET", "/api/me", Some(&token), Value::Null)
            .await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(
        revoked["error"]["code"], "session_not_found",
        "a revoked session must not report itself as expired"
    );

    // Public routes stay public: the session layer must not turn a missing
    // cookie into a rejection on a route that never required one.
    assert_eq!(
        fixture
            .call("GET", "/api/games", None, Value::Null)
            .await
            .status(),
        200
    );
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn presentation_dates_control_diary_and_rating_without_recording_time_fallbacks() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.user("historical-dates").await;
    let collection = "/api/me/playthroughs";
    let library = "/api/me/library/42";
    // Review-only and rating-only entries need no invented play dates. A chosen
    // day can also describe a summary with neither a rating nor a review.
    let mut rows = Vec::new();
    for fields in [
        json!({"igdb_id":42,"review":"Written now about last year","presentation_on":"2025-09-09"}),
        json!({"igdb_id":42,"rating":8,"presentation_on":"2026-01-01"}),
        json!({"igdb_id":42,"rating":9,"presentation_on":"2026-01-01"}),
        json!({"igdb_id":42,"rating":10,"finished_on":"2026-09-09"}),
        json!({"igdb_id":42,"rating":3}),
        json!({"igdb_id":42,"presentation_on":"2027-01-01","started_on":"2020-01-01","finished_on":"2020-02-01"}),
    ] {
        rows.push(
            fixture
                .value("POST", collection, &token, fields, StatusCode::CREATED)
                .await,
        );
    }
    // Deliberately make recording timestamps disagree with UUID tie ordering.
    // This catches a reintroduced timestamp tie-breaker regardless of random IDs.
    for pair in [[1, 2], [3, 4]] {
        let mut indices = pair;
        indices.sort_by(|a, b| rows[*a]["id"].as_str().cmp(&rows[*b]["id"].as_str()));
        for (index, recorded_at) in [
            (indices[0], "2099-01-01T00:00:00Z"),
            (indices[1], "1900-01-01T00:00:00Z"),
        ] {
            sqlx::query("UPDATE playthroughs SET created_at=$2::text::timestamptz WHERE id=$1")
                .bind(Uuid::parse_str(rows[index]["id"].as_str().unwrap()).unwrap())
                .bind(recorded_at)
                .execute(&fixture.database.app)
                .await
                .unwrap();
            rows[index] = fixture
                .value(
                    "GET",
                    &format!("{collection}/{}", rows[index]["id"].as_str().unwrap()),
                    &token,
                    Value::Null,
                    StatusCode::OK,
                )
                .await;
        }
    }
    // Same-day and unknown-date ties depend only on UUID, never insertion order.
    let mut dated = [rows[1].clone(), rows[2].clone()];
    dated.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    let mut undated = [rows[3].clone(), rows[4].clone()];
    undated.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    let expected = vec![
        rows[5]["id"].clone(),
        dated[0]["id"].clone(),
        dated[1]["id"].clone(),
        rows[0]["id"].clone(),
        undated[0]["id"].clone(),
        undated[1]["id"].clone(),
    ];
    let ids = |page: &Value| {
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].clone())
            .collect::<Vec<_>>()
    };
    for query in ["", "?igdb_id=42"] {
        let page = fixture
            .value(
                "GET",
                &format!("{collection}{query}"),
                &token,
                Value::Null,
                StatusCode::OK,
            )
            .await;
        assert_eq!(ids(&page), expected);
    }
    let page = fixture
        .value(
            "GET",
            &format!("{collection}?limit=2&offset=2"),
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(ids(&page), expected[2..4]);
    assert_eq!(page["has_more"], true);
    let entry = fixture
        .value(
            "PUT",
            library,
            &token,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(entry["rating_playthrough_id"], dated[0]["id"]);
    assert_eq!(entry["rating"], dated[0]["rating"]);
    let listed = fixture
        .value(
            "GET",
            "/api/me/library",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed["data"][0]["entry"], entry);

    let original = &rows[0];
    let path = format!("{collection}/{}", original["id"].as_str().unwrap());
    let edited = fixture
        .edit(
            "PATCH",
            &path,
            &token,
            &tag(original),
            json!({"review":"Corrected text"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(edited["presentation_on"], "2025-09-09");
    assert_eq!(edited["created_at"], original["created_at"]);
    assert_ne!(edited["updated_at"], original["updated_at"]);
    assert_eq!(edited["version"], 2);
    let page = fixture
        .value("GET", collection, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(ids(&page), expected);
    let moved = fixture
        .edit(
            "PATCH",
            &path,
            &token,
            &tag(&edited),
            json!({"presentation_on":"2028-02-29","rating":6}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(moved["presentation_on"], "2028-02-29");
    assert!(moved["started_on"].is_null());
    assert!(moved["finished_on"].is_null());
    let page = fixture
        .value("GET", collection, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(page["data"][0]["id"], original["id"]);
    let entry = fixture
        .value("GET", library, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(entry["rating"], 6);
    assert_eq!(entry["rating_playthrough_id"], original["id"]);
    let unchanged = fixture
        .edit(
            "PATCH",
            &path,
            &token,
            &tag(&moved),
            json!({"presentation_on":"2028-02-29"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(unchanged, moved, "re-saving the date is not a revision");

    // Date corrections participate in exactly the same ownership and locking
    // rules as text: concurrent edits against one revision have one winner.
    let (_, other) = fixture.user("historical-other").await;
    fixture
        .edit(
            "PATCH",
            &path,
            &other,
            &tag(&moved),
            json!({"presentation_on":null}),
            StatusCode::NOT_FOUND,
        )
        .await;
    fixture
        .edit(
            "PATCH",
            &path,
            &token,
            &tag(original),
            json!({"presentation_on":null}),
            StatusCode::PRECONDITION_FAILED,
        )
        .await;
    let revision = tag(&moved);
    let headers = [("If-Match", revision.as_str())];
    let (a, b) = tokio::join!(
        fixture.send(
            "PATCH",
            &path,
            Some(&token),
            &headers,
            json!({"presentation_on":"2000-01-01"})
        ),
        fixture.send(
            "PATCH",
            &path,
            Some(&token),
            &headers,
            json!({"review":"Concurrent correction"})
        ),
    );
    assert!(matches!(
        (a.status(), b.status()),
        (StatusCode::OK, StatusCode::PRECONDITION_FAILED)
            | (StatusCode::PRECONDITION_FAILED, StatusCode::OK)
    ));
    let current = fixture
        .value("GET", &path, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["version"], 4);
    let cleared = fixture
        .edit(
            "PATCH",
            &path,
            &token,
            &tag(&current),
            json!({"presentation_on":null}),
            StatusCode::OK,
        )
        .await;
    assert!(cleared["presentation_on"].is_null());
    assert_eq!(cleared["review"], current["review"]);
    // PUT can set a day, and omission in a subsequent replacement clears it.
    let replaced = fixture
        .edit(
            "PUT",
            &path,
            &token,
            &tag(&cleared),
            json!({"presentation_on":"1999-12-31"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(replaced["presentation_on"], "1999-12-31");
    let replaced = fixture
        .edit(
            "PUT",
            &path,
            &token,
            &tag(&replaced),
            json!({"review":"Unknown date"}),
            StatusCode::OK,
        )
        .await;
    assert!(replaced["presentation_on"].is_null());
    // Once all dated ratings are removed, the deterministic unknown-date
    // representative remains available, without claiming it is the newest.
    for row in &dated {
        let path = format!("{collection}/{}", row["id"].as_str().unwrap());
        fixture
            .edit(
                "PATCH",
                &path,
                &token,
                &tag(row),
                json!({"rating":null}),
                StatusCode::OK,
            )
            .await;
    }
    let entry = fixture
        .value("GET", library, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(entry["rating_playthrough_id"], undated[0]["id"]);
    assert_eq!(entry["rating"], undated[0]["rating"]);
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn presentation_date_migration_preserves_existing_dates_and_revisions() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.user("date-migration").await;
    let collection = "/api/me/playthroughs";
    let row = fixture.value("POST", collection, &token,
        json!({"igdb_id":42,"rating":7,"review":"Existing review","started_on":"2020-01-01","finished_on":"2020-02-01"}), StatusCode::CREATED).await;
    let path = format!("{collection}/{}", row["id"].as_str().unwrap());
    let row = fixture
        .edit(
            "PATCH",
            &path,
            &token,
            &tag(&row),
            json!({"platform":"PC"}),
            StatusCode::OK,
        )
        .await;
    // Reconstruct the prior schema and ordering in this disposable database.
    sqlx::raw_sql(
        "DROP VIEW library_entry_details;
        DROP INDEX playthroughs_game_recent_idx; DROP INDEX playthroughs_diary_recent_idx;
        ALTER TABLE playthroughs DROP COLUMN presentation_on, DROP COLUMN state, DROP COLUMN is_current;
        DELETE FROM _sqlx_migrations WHERE version IN (202609090001,202609090002);",
    )
    .execute(&fixture.database.app)
    .await
    .unwrap();
    let prior_ordering = include_str!("../migrations/202609080002_playthroughs.sql")
        .split_once("CREATE INDEX playthroughs_game_recent_idx")
        .unwrap()
        .1;
    // This string is exclusively repository-owned migration SQL, with no user input.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE INDEX playthroughs_game_recent_idx{prior_ordering}"
    )))
    .execute(&fixture.database.app)
    .await
    .unwrap();
    let migrator = sqlx::migrate!("./migrations");
    migrator.run(&fixture.database.app).await.unwrap();
    migrator.run(&fixture.database.app).await.unwrap();
    let migrated = fixture
        .value("GET", &path, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(
        migrated, row,
        "migration invents no presentation day and changes no private bookkeeping or content"
    );
    assert!(migrated["presentation_on"].is_null());
    fixture.finish().await;
}
