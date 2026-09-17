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
    // Owner game bookkeeping stays available, but related public catalog reads
    // must use only the descriptive projection.
    assert_eq!(listed["data"][0]["game"], imported);
    let mut description = imported.clone();
    for field in ["id", "imported_at", "refreshed_at"] {
        assert!(found["data"][0].get(field).is_none());
        description.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(description, found["data"][0]);
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
    undo_visibility(&fixture.database.app).await;
    // Restore the exact old library shape in this disposable database, then
    // exercise the real migrator, including its migration history and restart.
    sqlx::raw_sql("DROP VIEW diary_note_details; DROP TABLE diary_notes; DROP FUNCTION diary_note_revision();
        DROP VIEW library_entry_details; DROP TABLE playthroughs;
        DROP FUNCTION playthrough_revision();
        ALTER TABLE library_entries ADD COLUMN rating SMALLINT CHECK (rating BETWEEN 1 AND 10),
        ADD COLUMN review TEXT CHECK (char_length(review) <= 10000);
        DELETE FROM _sqlx_migrations WHERE version IN (202609080002,202609080003,202609090001,202609090002,202609090003);
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
    assert!(game.get("id").is_none());
    let stored_id: Uuid = sqlx::query_scalar("SELECT id FROM games WHERE igdb_id=42")
        .fetch_one(&fixture.database.app)
        .await
        .unwrap();
    assert_eq!(json!(stored_id), local_id);
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
    undo_visibility(&fixture.database.app).await;
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

impl Fixture {
    async fn note(&self, token: &str, parent: &Value, text: &str, day: Option<&str>) -> Value {
        self.value(
            "POST",
            &format!(
                "/api/me/playthroughs/{}/notes",
                parent["id"].as_str().unwrap()
            ),
            token,
            json!({"text":text,"presentation_on":day}),
            StatusCode::CREATED,
        )
        .await
    }
}

fn note_path(note: &Value) -> String {
    format!("/api/me/notes/{}", note["id"].as_str().unwrap())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn diary_notes_keep_independent_history_and_local_game_context() {
    let f = Fixture::new().await;
    let (_, token) = f.user("notes-history").await;
    let parent = f.value("POST", "/api/me/playthroughs", &token,
        json!({"igdb_id":42,"review":"Overall review","rating":8,"presentation_on":"2025-01-01"}), StatusCode::CREATED).await;
    let old = f
        .note(
            &token,
            &parent,
            "First session\n  With formatting",
            Some("2020-01-01"),
        )
        .await;
    let recent = f
        .note(&token, &parent, "Last session", Some("2021-01-01"))
        .await;
    let unknown = f.note(&token, &parent, "Unknown day", None).await;
    let tied = f.note(&token, &parent, "Another unknown", None).await;
    // Opposing recording times cannot influence ordering or historical dates.
    sqlx::query("UPDATE diary_notes SET created_at=CASE WHEN id=$1 THEN '2099-01-01'::timestamptz ELSE '1900-01-01'::timestamptz END")
        .bind(Uuid::parse_str(old["id"].as_str().unwrap()).unwrap()).execute(&f.database.app).await.unwrap();
    let old = f
        .value("GET", &note_path(&old), &token, Value::Null, StatusCode::OK)
        .await;
    // No note route may require IGDB, even if all stored metadata is stale.
    sqlx::query("UPDATE games SET refreshed_at='2000-01-01'::timestamptz")
        .execute(&f.database.app)
        .await
        .unwrap();
    let lookups = provider_lookups(&f).await;
    let edited = f
        .edit(
            "PATCH",
            &note_path(&old),
            &token,
            &tag(&old),
            json!({"text":"Revised old session"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(edited["presentation_on"], "2020-01-01");
    assert_eq!(edited["created_at"], old["created_at"]);
    assert_eq!(
        edited["version"].as_i64().unwrap(),
        old["version"].as_i64().unwrap() + 1
    );
    assert_ne!(edited["updated_at"], old["updated_at"]);
    let noop = f
        .edit(
            "PATCH",
            &note_path(&edited),
            &token,
            &tag(&edited),
            json!({}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(noop, edited);
    let nested = format!(
        "/api/me/playthroughs/{}/notes",
        parent["id"].as_str().unwrap()
    );
    let page = f
        .value(
            "GET",
            "/api/me/diary?limit=2",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(page["has_more"], true);
    assert_eq!(page["data"][0]["id"], recent["id"]);
    assert_eq!(page["data"][1], edited);
    let last = f
        .value(
            "GET",
            "/api/me/diary?limit=2&offset=2",
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let mut ids = [
        unknown["id"].as_str().unwrap(),
        tied["id"].as_str().unwrap(),
    ];
    ids.sort_by(|a, b| b.cmp(a));
    assert_eq!(last["has_more"], false);
    assert_eq!(last["data"][0]["id"], ids[0]);
    assert_eq!(last["data"][1]["id"], ids[1]);
    let all = f
        .value("GET", &nested, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(all["data"].as_array().unwrap().len(), 4);
    for row in all["data"].as_array().unwrap() {
        assert_eq!(row["igdb_id"], 42);
        assert_eq!(row["game_name"], "Test Game");
        assert_eq!(row["game_slug"], "test-game");
        assert!(row.get("cover_url").is_some());
        assert_eq!(row["playthrough_id"], parent["id"]);
    }
    let backdated = f
        .edit(
            "PATCH",
            &note_path(&recent),
            &token,
            "*",
            json!({"presentation_on":"2019-12-31"}),
            StatusCode::OK,
        )
        .await;
    let reordered = f
        .value("GET", "/api/me/diary", &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(reordered["data"][0], edited);
    assert_eq!(reordered["data"][1], backdated);
    let replaced = f
        .edit(
            "PUT",
            &note_path(&backdated),
            &token,
            &tag(&backdated),
            json!({"text":"Replacement"}),
            StatusCode::OK,
        )
        .await;
    assert!(replaced["presentation_on"].is_null());
    let dated = f
        .edit(
            "PATCH",
            &note_path(&replaced),
            &token,
            &tag(&replaced),
            json!({"presentation_on":"2030-02-01"}),
            StatusCode::OK,
        )
        .await;
    let cleared = f
        .edit(
            "PATCH",
            &note_path(&dated),
            &token,
            &tag(&dated),
            json!({"presentation_on":null}),
            StatusCode::OK,
        )
        .await;
    assert!(cleared["presentation_on"].is_null());
    let unchanged = f
        .value(
            "GET",
            &format!("/api/me/playthroughs/{}", parent["id"].as_str().unwrap()),
            &token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        unchanged, parent,
        "notes never replace reviews or revise their parent"
    );
    assert_eq!(provider_lookups(&f).await, lookups);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn diary_notes_enforce_ownership_validation_and_revision_preconditions() {
    let f = Fixture::new().await;
    let (alice_id, alice) = f.user("note-owner").await;
    let (_, bob) = f.user("note-other").await;
    let parent = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &alice,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    let nested = format!(
        "/api/me/playthroughs/{}/notes",
        parent["id"].as_str().unwrap()
    );
    let note = f
        .note(&alice, &parent, "Private note", Some("2020-01-01"))
        .await;
    let path = note_path(&note);
    let read = f.call("GET", &path, Some(&alice), Value::Null).await;
    assert_eq!(read.headers()["etag"], tag(&note));
    for (method, url, body) in [
        ("GET", path.as_str(), Value::Null),
        ("GET", nested.as_str(), Value::Null),
        ("GET", "/api/me/diary", Value::Null),
        ("POST", nested.as_str(), json!({"text":"Unauthorized"})),
        ("PUT", path.as_str(), json!({"text":"Unauthorized"})),
        ("PATCH", path.as_str(), json!({"text":"Unauthorized"})),
        ("DELETE", path.as_str(), Value::Null),
    ] {
        assert_eq!(
            f.send(method, url, None, &[("If-Match", "\"1\"")], body.clone())
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        if url == "/api/me/diary" {
            continue;
        }
        let rejected = json_body(
            f.send(method, url, Some(&bob), &[("If-Match", "\"999\"")], body)
                .await,
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_eq!(rejected["error"]["code"], "not_found");
    }
    let bob_diary = f
        .value(
            "GET",
            "/api/me/diary?limit=1",
            &bob,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(bob_diary["data"], json!([]));
    assert_eq!(bob_diary["has_more"], false);
    for method in ["PUT", "PATCH", "DELETE"] {
        f.value(
            method,
            &path,
            &alice,
            json!({"text":"Missing revision"}),
            StatusCode::PRECONDITION_REQUIRED,
        )
        .await;
        for (etag, status) in [
            ("1", StatusCode::BAD_REQUEST),
            ("W/\"1\"", StatusCode::PRECONDITION_FAILED),
            ("\"999\"", StatusCode::PRECONDITION_FAILED),
        ] {
            f.edit(method, &path, &alice, etag, json!({"text":"Stale"}), status)
                .await;
        }
    }
    for payload in [
        json!({}),
        json!({"text":null}),
        json!({"text":12}),
        json!({"text":"a","presentation_on":"2020-02-30"}),
        json!({"text":"a","user_id":alice_id.0.to_string()}),
        json!({"text":"a","playthrough_id":parent["id"]}),
        json!({"text":"a","visibility":"public"}),
    ] {
        f.value(
            "POST",
            &nested,
            &alice,
            payload,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for text in [
        "".to_owned(),
        " \n\t\u{2003}".to_owned(),
        "x".repeat(10_001),
        "a\0b".to_owned(),
    ] {
        for method in ["POST", "PUT", "PATCH"] {
            let url = if method == "POST" { &nested } else { &path };
            let error = f
                .edit(
                    method,
                    url,
                    &alice,
                    &tag(&note),
                    json!({"text":text}),
                    StatusCode::BAD_REQUEST,
                )
                .await;
            assert_eq!(error["error"]["fields"][0]["field"], "text");
        }
    }
    f.edit(
        "PATCH",
        &path,
        &alice,
        &tag(&note),
        json!({"text":null}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let unicode = "🎮".repeat(10_000);
    let saved = f
        .edit(
            "PUT",
            &path,
            &alice,
            &tag(&note),
            json!({"text":unicode}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(saved["text"], unicode);
    for query in [
        "limit=0",
        "limit=51",
        "offset=10001",
        "offset=-1",
        "unknown=test",
    ] {
        for url in [
            format!("/api/me/diary?{query}"),
            format!("{nested}?{query}"),
        ] {
            f.value("GET", &url, &alice, Value::Null, StatusCode::BAD_REQUEST)
                .await;
        }
    }
    f.edit(
        "DELETE",
        &path,
        &alice,
        &tag(&note),
        Value::Null,
        StatusCode::PRECONDITION_FAILED,
    )
    .await;
    f.edit(
        "DELETE",
        &path,
        &alice,
        &tag(&saved),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    f.edit(
        "DELETE",
        &path,
        &alice,
        &tag(&saved),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    f.value("GET", &path, &alice, Value::Null, StatusCode::NOT_FOUND)
        .await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn diary_notes_survive_membership_removal_and_replay_but_follow_parent_and_account_deletion()
{
    let f = Fixture::new().await;
    let (id, token) = f.user("note-retention").await;
    let run = f
        .value(
            "POST",
            "/api/me/playthroughs/start",
            &token,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    let note = f.note(&token, &run, "First run", Some("2020-01-01")).await;
    f.lifecycle(&token, &run, json!({"action":"complete"}), StatusCode::OK)
        .await;
    let replay = f
        .value(
            "POST",
            "/api/me/playthroughs/start",
            &token,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    let replay_note = f.note(&token, &replay, "Replay note", None).await;
    f.value(
        "DELETE",
        "/api/me/library/42",
        &token,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    for expected in [&note, &replay_note] {
        let stored = f
            .value(
                "GET",
                &note_path(expected),
                &token,
                Value::Null,
                StatusCode::OK,
            )
            .await;
        assert_eq!(stored, *expected);
    }
    f.value(
        "DELETE",
        &format!("/api/me/playthroughs/{}", run["id"].as_str().unwrap()),
        &token,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    f.value(
        "GET",
        &note_path(&note),
        &token,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let diary = f
        .value("GET", "/api/me/diary", &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(diary["data"], json!([replay_note]));
    let gone = format!("/api/me/playthroughs/{}/notes", run["id"].as_str().unwrap());
    f.value("GET", &gone, &token, Value::Null, StatusCode::NOT_FOUND)
        .await;
    f.value(
        "POST",
        &gone,
        &token,
        json!({"text":"Too late"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    f.auth
        .state()
        .operator()
        .delete_account(
            id,
            OperatorActor::new("integration-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM diary_notes")
        .fetch_one(&f.database.app)
        .await
        .unwrap();
    assert_eq!(count, 0);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn concurrent_note_writes_and_parent_deletion_are_consistent() {
    let f = Fixture::new().await;
    let (_, token) = f.user("note-races").await;
    let parent = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &token,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    let note = f.note(&token, &parent, "Original", None).await;
    let path = note_path(&note);
    let headers = [("If-Match", "\"1\"")];
    let (a, b) = tokio::join!(
        f.send(
            "PATCH",
            &path,
            Some(&token),
            &headers,
            json!({"text":"First editor"})
        ),
        f.send(
            "PATCH",
            &path,
            Some(&token),
            &headers,
            json!({"text":"Second editor"})
        ),
    );
    let mut statuses = [a.status().as_u16(), b.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 412]);
    // Wildcard opts into merging against the latest version, under the lock.
    let headers = [("If-Match", "*")];
    let (a, b) = tokio::join!(
        f.send(
            "PATCH",
            &path,
            Some(&token),
            &headers,
            json!({"text":"Merged editor"})
        ),
        f.send(
            "PATCH",
            &path,
            Some(&token),
            &headers,
            json!({"presentation_on":"2022-01-01"})
        ),
    );
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(b.status(), StatusCode::OK);
    let current = f
        .value("GET", &path, &token, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(current["text"], "Merged editor");
    assert_eq!(current["presentation_on"], "2022-01-01");
    assert_eq!(current["version"], 4);
    let etag = tag(&current);
    let headers = [("If-Match", etag.as_str())];
    let (edit, delete) = tokio::join!(
        f.send(
            "PATCH",
            &path,
            Some(&token),
            &headers,
            json!({"text":"Racing deletion"})
        ),
        f.send("DELETE", &path, Some(&token), &headers, Value::Null),
    );
    assert!(matches!(
        (edit.status().as_u16(), delete.status().as_u16()),
        (200, 412) | (404, 204)
    ));
    // Race create/edit against cascading parent deletion: no FK error, orphan,
    // or deadlock is an acceptable result, whichever request wins.
    let note = f.note(&token, &parent, "Before parent removal", None).await;
    let note_url = note_path(&note);
    let parent_url = format!("/api/me/playthroughs/{}", parent["id"].as_str().unwrap());
    let nested = format!("{parent_url}/notes");
    let headers = [("If-Match", "*")];
    let (create, edit, delete) = timeout(Duration::from_secs(10), async {
        tokio::join!(
            f.call(
                "POST",
                &nested,
                Some(&token),
                json!({"text":"Racing parent removal"})
            ),
            f.send(
                "PATCH",
                &note_url,
                Some(&token),
                &headers,
                json!({"text":"Racing parent removal"})
            ),
            f.call("DELETE", &parent_url, Some(&token), Value::Null),
        )
    })
    .await
    .expect("note/parent locking must not deadlock");
    assert!(matches!(create.status().as_u16(), 201 | 404));
    assert!(matches!(edit.status().as_u16(), 200 | 404));
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM diary_notes")
        .fetch_one(&f.database.app)
        .await
        .unwrap();
    assert_eq!(count, 0);
    f.finish().await;
}

// Reconstruct the schema before feature 4 in disposable migration fixtures.
async fn undo_visibility(pool: &PgPool) {
    undo_public_profiles(pool).await;
    sqlx::raw_sql("DROP VIEW public_library_entries,public_reviews,public_ratings,public_playthroughs,public_notes;
        DROP VIEW library_entry_details,diary_note_details;
        DROP TRIGGER profiles_revision ON profiles;
        DROP TRIGGER library_entries_revision ON library_entries;
        DROP TRIGGER playthroughs_clear_publication ON playthroughs;
        DROP FUNCTION clear_missing_publication();
        ALTER TABLE profiles DROP COLUMN public_enabled,DROP COLUMN version;
        ALTER TABLE library_entries DROP COLUMN public_id,DROP COLUMN visibility,DROP COLUMN version;
        ALTER TABLE playthroughs DROP COLUMN summary_visibility,DROP COLUMN rating_visibility,DROP COLUMN review_visibility;
        ALTER TABLE diary_notes DROP COLUMN visibility;
        DELETE FROM _sqlx_migrations WHERE version=202609100001;")
        .execute(pool).await.unwrap();
    let library = include_str!("../migrations/202609090002_playthrough_lifecycle.sql")
        .split_once("CREATE OR REPLACE VIEW library_entry_details")
        .unwrap()
        .1;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE VIEW library_entry_details{library}"
    )))
    .execute(pool)
    .await
    .unwrap();
    let notes = include_str!("../migrations/202609090003_diary_notes.sql")
        .split_once("CREATE VIEW diary_note_details")
        .unwrap()
        .1;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE VIEW diary_note_details{notes}"
    )))
    .execute(pool)
    .await
    .unwrap();
}

fn private_parts() -> Value {
    json!({
        "summary_visibility":"private","rating_visibility":"private","review_visibility":"private"
    })
}

async fn publication(fixture: &Fixture, token: &str, enabled: bool) -> Value {
    let me = fixture
        .value("GET", "/api/me", token, Value::Null, StatusCode::OK)
        .await;
    fixture
        .edit(
            "PUT",
            "/api/me/publication",
            token,
            &tag(&me["profile"]),
            json!({"public_enabled":enabled}),
            StatusCode::OK,
        )
        .await
}

async fn share_run(fixture: &Fixture, token: &str, run: &Value, parts: Value) -> Value {
    fixture
        .edit(
            "PUT",
            &format!("{}/visibility", run_path(run)),
            token,
            &tag(run),
            parts,
            StatusCode::OK,
        )
        .await
}

async fn public_body(
    fixture: &Fixture,
    path: &str,
    token: Option<&str>,
    status: StatusCode,
) -> Value {
    let response = fixture
        .send(
            "GET",
            path,
            token,
            &[
                ("If-None-Match", "*"),
                ("If-Modified-Since", "Wed, 09 Sep 2099 00:00:00 GMT"),
            ],
            Value::Null,
        )
        .await;
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(!response.headers().contains_key("etag"));
    assert!(!response.headers().contains_key("last-modified"));
    let body = json_body(response, status).await;
    fn safe(value: &Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(
                        ![
                            "created_at",
                            "updated_at",
                            "version",
                            "imported_at",
                            "refreshed_at",
                            "user_id",
                            "email",
                            "is_current",
                            "current_playthrough_id",
                            "playthrough_id",
                            "rating_playthrough_id",
                            "visibility",
                            "summary_visibility",
                            "rating_visibility",
                            "review_visibility"
                        ]
                        .contains(&key.as_str()),
                        "leaked {key}"
                    );
                    safe(value);
                }
            }
            Value::Array(items) => {
                for item in items {
                    safe(item);
                }
            }
            _ => (),
        }
    }
    safe(&body);
    body
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn visibility_is_independent_and_public_projections_do_not_expand_private_relations() {
    let fixture = Fixture::new().await;
    let (_, owner) = fixture.user("sharing-owner").await;
    let (_, other) = fixture.user("sharing-other").await;
    fixture
        .value(
            "PUT",
            "/api/me",
            &owner,
            json!({"display_name":"Author","bio":"Private bio"}),
            StatusCode::OK,
        )
        .await;
    let entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"dropped"}),
            StatusCode::OK,
        )
        .await;
    let run = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,
        "rating":9,"review":"Public review","presentation_on":"2020-02-02","state":"abandoned",
        "started_on":"2019-01-01","finished_on":"2020-01-01","platform":"Secret platform"}),
            StatusCode::CREATED,
        )
        .await;
    let note = fixture
        .note(&owner, &run, "Secret note", Some("2019-08-01"))
        .await;
    let run_id = run["id"].as_str().unwrap();
    let note_id = note["id"].as_str().unwrap();
    let urls = [
        format!(
            "/api/public/library/{}",
            entry["public_id"].as_str().unwrap()
        ),
        format!("/api/public/playthroughs/{run_id}"),
        format!("/api/public/ratings/{run_id}"),
        format!("/api/public/reviews/{run_id}"),
        format!("/api/public/notes/{note_id}"),
    ];
    assert_eq!(entry["visibility"], "private");
    assert_eq!(note["visibility"], "private");
    for field in [
        "summary_visibility",
        "rating_visibility",
        "review_visibility",
    ] {
        assert_eq!(run[field], "private");
    }
    for token in [None, Some(owner.as_str()), Some(other.as_str())] {
        for url in &urls {
            public_body(&fixture, url, token, StatusCode::NOT_FOUND).await;
        }
    }
    // A public review does not grant access to the owner's full record or children.
    let run = share_run(
        &fixture,
        &owner,
        &run,
        json!({"summary_visibility":"private",
        "rating_visibility":"private","review_visibility":"public"}),
    )
    .await;
    public_body(&fixture, &urls[3], None, StatusCode::NOT_FOUND).await;
    let profile = publication(&fixture, &owner, true).await;
    let expected = json!({"id":run["id"],"author_name":"Author","author_id":profile["public_id"],"game":{
        "igdb_id":42,"game_name":"Test Game","game_slug":"test-game","cover_url":null},
        "presentation_on":"2020-02-02","review":"Public review"});
    for token in [None, Some(owner.as_str()), Some(other.as_str())] {
        for (i, url) in urls.iter().enumerate() {
            let body = public_body(
                &fixture,
                url,
                token,
                if i == 3 {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                },
            )
            .await;
            if i == 3 {
                assert_eq!(body, expected);
            }
        }
    }
    for path in [
        run_path(&run),
        format!("{}/notes", run_path(&run)),
        format!("/api/me/notes/{note_id}"),
        "/api/me/library/42".into(),
    ] {
        fixture
            .value("GET", &path, &owner, Value::Null, StatusCode::OK)
            .await;
        fixture
            .value("GET", &path, &other, Value::Null, StatusCode::NOT_FOUND)
            .await;
        assert_eq!(
            fixture.call("GET", &path, None, Value::Null).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    // Publish a note with every parent aspect private, then test each other part alone.
    let mut run = share_run(&fixture, &owner, &run, private_parts()).await;
    let note = fixture
        .edit(
            "PUT",
            &format!("/api/me/notes/{note_id}/visibility"),
            &owner,
            &tag(&note),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    for token in [None, Some(owner.as_str()), Some(other.as_str())] {
        let body = public_body(&fixture, &urls[4], token, StatusCode::OK).await;
        assert_eq!(
            body,
            json!({"id":note["id"],"author_name":"Author","author_id":profile["public_id"],"game":expected["game"],
            "presentation_on":"2019-08-01","text":"Secret note"})
        );
        public_body(&fixture, &urls[1], token, StatusCode::NOT_FOUND).await;
    }
    fixture
        .edit(
            "PUT",
            &format!("/api/me/notes/{note_id}/visibility"),
            &owner,
            &tag(&note),
            json!({"visibility":"private"}),
            StatusCode::OK,
        )
        .await;
    for (part, index) in [("summary_visibility", 1), ("rating_visibility", 2)] {
        let mut choices = private_parts();
        choices[part] = json!("public");
        run = share_run(&fixture, &owner, &run, choices).await;
        for token in [None, Some(owner.as_str()), Some(other.as_str())] {
            for (i, url) in urls.iter().enumerate() {
                let body = public_body(
                    &fixture,
                    url,
                    token,
                    if i == index {
                        StatusCode::OK
                    } else {
                        StatusCode::NOT_FOUND
                    },
                )
                .await;
                if i == 1 && i == index {
                    assert_eq!(
                        body,
                        json!({"id":run["id"],"author_name":"Author","author_id":profile["public_id"],"game":expected["game"],
                        "presentation_on":"2020-02-02","state":"abandoned","started_on":"2019-01-01",
                        "finished_on":"2020-01-01","platform":"Secret platform"})
                    );
                } else if i == 2 && i == index {
                    assert_eq!(
                        body,
                        json!({"id":run["id"],"author_name":"Author","author_id":profile["public_id"],"game":expected["game"],
                        "presentation_on":"2020-02-02","rating":9})
                    );
                }
            }
        }
    }
    share_run(&fixture, &owner, &run, private_parts()).await;
    fixture
        .edit(
            "PUT",
            "/api/me/library/42/visibility",
            &owner,
            &tag(&entry),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    for token in [None, Some(owner.as_str()), Some(other.as_str())] {
        let body = public_body(&fixture, &urls[0], token, StatusCode::OK).await;
        assert_eq!(
            body,
            json!({"id":entry["public_id"],"author_name":"Author","author_id":profile["public_id"],"game":expected["game"],
            "status":"dropped","rating":null,"rating_id":null})
        );
    }
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn profile_revocation_deletion_and_new_content_obey_the_same_public_boundary() {
    let fixture = Fixture::new().await;
    let (_, owner) = fixture.user("revocation-owner").await;
    let (_, other) = fixture.user("revocation-other").await;
    publication(&fixture, &owner, true).await;
    let entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"backlog"}),
            StatusCode::OK,
        )
        .await;
    fixture
        .edit(
            "PUT",
            "/api/me/library/42/visibility",
            &owner,
            &tag(&entry),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    let run = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"rating":7,"review":"Shared"}),
            StatusCode::CREATED,
        )
        .await;
    let run = share_run(&fixture,&owner,&run,json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await;
    let note = fixture.note(&owner, &run, "Shared note", None).await;
    let note_path = format!("/api/me/notes/{}", note["id"].as_str().unwrap());
    let note = fixture
        .edit(
            "PUT",
            &format!("{note_path}/visibility"),
            &owner,
            &tag(&note),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    let new_note = fixture
        .note(&owner, &run, "Later private addition", None)
        .await;
    assert_eq!(new_note["visibility"], "private");
    let entry = fixture
        .value(
            "GET",
            "/api/me/library/42",
            &owner,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let urls = [
        format!(
            "/api/public/library/{}",
            entry["public_id"].as_str().unwrap()
        ),
        format!("/api/public/playthroughs/{}", run["id"].as_str().unwrap()),
        format!("/api/public/ratings/{}", run["id"].as_str().unwrap()),
        format!("/api/public/reviews/{}", run["id"].as_str().unwrap()),
        format!("/api/public/notes/{}", note["id"].as_str().unwrap()),
    ];
    let mut before = Vec::new();
    for url in &urls {
        before.push(public_body(&fixture, url, None, StatusCode::OK).await);
    }
    let disabled = publication(&fixture, &owner, false).await;
    for token in [None, Some(owner.as_str()), Some(other.as_str())] {
        for url in &urls {
            public_body(&fixture, url, token, StatusCode::NOT_FOUND).await;
        }
    }
    assert_eq!(
        fixture
            .value("GET", &run_path(&run), &owner, Value::Null, StatusCode::OK)
            .await,
        run
    );
    assert_eq!(
        fixture
            .value("GET", &note_path, &owner, Value::Null, StatusCode::OK)
            .await,
        note
    );
    assert_eq!(
        fixture
            .value(
                "GET",
                "/api/me/library/42",
                &owner,
                Value::Null,
                StatusCode::OK
            )
            .await,
        entry
    );
    let unchanged = fixture
        .edit(
            "PUT",
            "/api/me/publication",
            &owner,
            &tag(&disabled),
            json!({"public_enabled":false}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(unchanged, disabled);
    publication(&fixture, &owner, true).await;
    for (url, expected) in urls.iter().zip(&before) {
        assert_eq!(
            &public_body(&fixture, url, None, StatusCode::OK).await,
            expected
        );
    }
    let head = fixture.call("HEAD", &urls[3], None, Value::Null).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["cache-control"], "no-store");
    assert!(!head.headers().contains_key("etag"));
    assert!(!head.headers().contains_key("last-modified"));
    // Starts keep existing library permission, but new runs never inherit it.
    let replay = fixture
        .value(
            "POST",
            START_RUN,
            &owner,
            json!({"igdb_id":42}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(replay["summary_visibility"], "private");
    public_body(
        &fixture,
        &format!(
            "/api/public/playthroughs/{}",
            replay["id"].as_str().unwrap()
        ),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        fixture
            .call("DELETE", "/api/me/library/42", Some(&owner), Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    public_body(&fixture, &urls[0], None, StatusCode::NOT_FOUND).await;
    public_body(&fixture, &urls[4], None, StatusCode::OK).await;
    let replaced_entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"backlog"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(replaced_entry["visibility"], "private");
    assert_ne!(replaced_entry["public_id"], entry["public_id"]);
    fixture
        .edit(
            "DELETE",
            &run_path(&run),
            &owner,
            &tag(&run),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    for url in &urls {
        public_body(&fixture, url, None, StatusCode::NOT_FOUND).await;
    }
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn publication_writes_enforce_ownership_preconditions_validation_and_concurrency() {
    let fixture = Fixture::new().await;
    let (_, owner) = fixture.user("permission-owner").await;
    let (_, other) = fixture.user("permission-other").await;
    let profile = fixture
        .value("GET", "/api/me", &owner, Value::Null, StatusCode::OK)
        .await["profile"]
        .clone();
    let entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"backlog"}),
            StatusCode::OK,
        )
        .await;
    let run = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"review":"Review"}),
            StatusCode::CREATED,
        )
        .await;
    let note = fixture.note(&owner, &run, "Note", None).await;
    let rows = [
        (
            "/api/me/publication".into(),
            profile,
            json!({"public_enabled":true}),
        ),
        (
            "/api/me/library/42/visibility".into(),
            entry,
            json!({"visibility":"public"}),
        ),
        (
            format!("{}/visibility", run_path(&run)),
            run.clone(),
            json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"}),
        ),
        (
            format!("/api/me/notes/{}/visibility", note["id"].as_str().unwrap()),
            note.clone(),
            json!({"visibility":"public"}),
        ),
    ];
    for (url, current, input) in &rows {
        assert_eq!(
            fixture.call("PUT", url, None, input.clone()).await.status(),
            StatusCode::UNAUTHORIZED
        );
        fixture
            .edit(
                "PUT",
                url,
                &other,
                &tag(current),
                input.clone(),
                StatusCode::NOT_FOUND,
            )
            .await;
        fixture
            .value(
                "PUT",
                url,
                &owner,
                input.clone(),
                StatusCode::PRECONDITION_REQUIRED,
            )
            .await;
        for invalid in [
            json!({}),
            json!({"visibility":"friends"}),
            json!({"visibility":{"public":null}}),
            json!({"visibility":null}),
            json!({"public_enabled":null}),
            json!({"unknown":true}),
        ] {
            let response = fixture
                .send(
                    "PUT",
                    url,
                    Some(&owner),
                    &[("If-Match", tag(current).as_str())],
                    invalid.clone(),
                )
                .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{url}: {invalid}"
            );
            json_body(response, StatusCode::UNPROCESSABLE_ENTITY).await;
        }
        let etag = tag(current);
        let headers = [("If-Match", etag.as_str())];
        let (a, b) = tokio::join!(
            fixture.send("PUT", url, Some(&owner), &headers, input.clone()),
            fixture.send("PUT", url, Some(&owner), &headers, input.clone())
        );
        let mut statuses = [a.status().as_u16(), b.status().as_u16()];
        statuses.sort();
        assert_eq!(statuses, [200, 412], "{url}");
        let saved = json_body(
            if a.status() == StatusCode::OK { a } else { b },
            StatusCode::OK,
        )
        .await;
        // Publication is an independent edit; no-op saves keep the same revision.
        let noop = fixture
            .edit(
                "PUT",
                url,
                &owner,
                &tag(&saved),
                input.clone(),
                StatusCode::OK,
            )
            .await;
        assert_eq!(noop, saved);
        fixture
            .edit(
                "PUT",
                url,
                &owner,
                &etag,
                input.clone(),
                StatusCode::PRECONDITION_FAILED,
            )
            .await;
    }
    let mut current = fixture
        .value("GET", &run_path(&run), &owner, Value::Null, StatusCode::OK)
        .await;
    let invalid = json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"});
    fixture
        .edit(
            "PUT",
            &rows[2].0,
            &owner,
            &tag(&current),
            invalid,
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(
        fixture
            .value("GET", &run_path(&run), &owner, Value::Null, StatusCode::OK)
            .await,
        current
    );
    // Publication versus content edits share a revision and cannot both commit.
    let etag = tag(&current);
    let url = run_path(&current);
    let visibility_url = format!("{url}/visibility");
    let headers = [("If-Match", etag.as_str())];
    let (a, b) = tokio::join!(
        fixture.send(
            "PATCH",
            &url,
            Some(&owner),
            &headers,
            json!({"review":"Revised"})
        ),
        fixture.send(
            "PUT",
            &visibility_url,
            Some(&owner),
            &headers,
            private_parts()
        )
    );
    let mut statuses = [a.status().as_u16(), b.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 412]);
    current = fixture
        .value("GET", &url, &owner, Value::Null, StatusCode::OK)
        .await;
    let current = share_run(&fixture,&owner,&current,json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"})).await;
    let current = fixture
        .edit(
            "PATCH",
            &url,
            &owner,
            &tag(&current),
            json!({"review":null,"rating":8}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(current["review_visibility"], "private");
    assert_eq!(current["rating_visibility"], "private");
    let current = share_run(&fixture,&owner,&current,json!({"summary_visibility":"private","rating_visibility":"public","review_visibility":"private"})).await;
    let current = fixture
        .edit(
            "PATCH",
            &url,
            &owner,
            &tag(&current),
            json!({"rating":null,"review":"Replacement review"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(current["rating_visibility"], "private");
    assert_eq!(current["review_visibility"], "private");
    let current = fixture
        .edit(
            "PATCH",
            &url,
            &owner,
            &tag(&current),
            json!({"rating":6}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(current["rating_visibility"], "private");
    public_body(
        &fixture,
        &format!("/api/public/reviews/{}", current["id"].as_str().unwrap()),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    public_body(
        &fixture,
        &format!("/api/public/ratings/{}", current["id"].as_str().unwrap()),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    // A content editor with the note's pre-publication revision is stale too.
    fixture
        .edit(
            "PATCH",
            &format!("/api/me/notes/{}", note["id"].as_str().unwrap()),
            &owner,
            &tag(&note),
            json!({"text":"Stale edit"}),
            StatusCode::PRECONDITION_FAILED,
        )
        .await;
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn public_rating_ordering_and_aggregates_ignore_private_history_and_bookkeeping() {
    let fixture = Fixture::new().await;
    let (owner_id, owner) = fixture.user("public-order").await;
    publication(&fixture, &owner, true).await;
    let entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"backlog"}),
            StatusCode::OK,
        )
        .await;
    let entry = fixture
        .edit(
            "PUT",
            "/api/me/library/42/visibility",
            &owner,
            &tag(&entry),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    let public_url = format!(
        "/api/public/library/{}",
        entry["public_id"].as_str().unwrap()
    );
    let mut public_runs = Vec::new();
    for (rating, day) in [
        (7, Some("2020-01-01")),
        (8, Some("2020-01-01")),
        (2, None),
        (3, None),
    ] {
        let run = fixture
            .value(
                "POST",
                "/api/me/playthroughs",
                &owner,
                json!({"igdb_id":42,"rating":rating,
            "presentation_on":day,"review":"A dated or unknown review"}),
                StatusCode::CREATED,
            )
            .await;
        public_runs.push(share_run(&fixture,&owner,&run,json!({"summary_visibility":"private","rating_visibility":"public","review_visibility":"public"})).await);
    }
    public_runs.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    public_runs.sort_by_key(|r| r["presentation_on"].is_null());
    let latest = &public_runs[0];
    let private = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"rating":10,
        "presentation_on":"2030-01-01","review":"Private newest"}),
            StatusCode::CREATED,
        )
        .await;
    let before = public_body(&fixture, &public_url, None, StatusCode::OK).await;
    assert_eq!(before["rating"], latest["rating"]);
    assert_eq!(before["rating_id"], latest["id"]);
    assert_eq!(
        fixture
            .value(
                "GET",
                "/api/me/library/42",
                &owner,
                Value::Null,
                StatusCode::OK
            )
            .await["rating"],
        10
    );
    // The same filtered views power future sections, pagination and aggregates.
    let rows: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM public_reviews WHERE user_id=$1 ORDER BY presentation_on DESC NULLS LAST,id DESC")
        .bind(owner_id.0.as_bytes().as_slice()).fetch_all(&fixture.database.app).await.unwrap();
    assert_eq!(
        json!(rows),
        Value::Array(public_runs.iter().map(|r| r["id"].clone()).collect())
    );
    let page: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM public_reviews WHERE user_id=$1 ORDER BY presentation_on DESC NULLS LAST,id DESC LIMIT 1 OFFSET 1")
        .bind(owner_id.0.as_bytes().as_slice()).fetch_all(&fixture.database.app).await.unwrap();
    assert_eq!(json!(page), json!([public_runs[1]["id"]]));
    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM public_reviews),
        (SELECT count(*) FROM public_ratings),(SELECT count(*) FROM public_playthroughs),
        (SELECT count(*) FROM public_notes),(SELECT count(*) FROM public_library_entries)",
    )
    .fetch_one(&fixture.database.app)
    .await
    .unwrap();
    assert_eq!(counts, (4, 4, 0, 0, 1));
    fixture
        .edit(
            "PATCH",
            &run_path(&private),
            &owner,
            &tag(&private),
            json!({"rating":1,"presentation_on":"1900-01-01"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        public_body(&fixture, &public_url, None, StatusCode::OK).await,
        before
    );
    let review_url = format!("/api/public/reviews/{}", latest["id"].as_str().unwrap());
    let review = public_body(&fixture, &review_url, None, StatusCode::OK).await;
    let edited = fixture
        .edit(
            "PATCH",
            &run_path(latest),
            &owner,
            &tag(latest),
            json!({"review":"Edited old text"}),
            StatusCode::OK,
        )
        .await;
    let mut expected = review.clone();
    expected["review"] = json!("Edited old text");
    assert_eq!(
        public_body(&fixture, &review_url, None, StatusCode::OK).await,
        expected
    );
    assert_eq!(
        public_body(&fixture, &public_url, None, StatusCode::OK).await,
        before
    );
    // Actual timestamps and revisions never contribute to a public projection.
    sqlx::query(
        "UPDATE playthroughs SET created_at='2090-01-01',updated_at='2090-01-02' WHERE user_id=$1",
    )
    .bind(owner_id.0.as_bytes().as_slice())
    .execute(&fixture.database.app)
    .await
    .unwrap();
    assert_eq!(
        public_body(&fixture, &review_url, None, StatusCode::OK).await,
        expected
    );
    assert_eq!(
        public_body(&fixture, &public_url, None, StatusCode::OK).await,
        before
    );
    let edited = fixture
        .value(
            "GET",
            &run_path(&edited),
            &owner,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let edited = fixture
        .edit(
            "PATCH",
            &run_path(&edited),
            &owner,
            &tag(&edited),
            json!({"presentation_on":"1990-01-01"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        public_body(&fixture, &public_url, None, StatusCode::OK).await["rating_id"],
        public_runs[1]["id"]
    );
    share_run(&fixture, &owner, &edited, private_parts()).await;
    public_body(&fixture, &review_url, None, StatusCode::NOT_FOUND).await;
    publication(&fixture, &owner, false).await;
    let remaining: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM public_reviews)+(SELECT count(*) FROM public_ratings)+
        (SELECT count(*) FROM public_playthroughs)+(SELECT count(*) FROM public_notes)+(SELECT count(*) FROM public_library_entries)")
        .fetch_one(&fixture.database.app).await.unwrap();
    assert_eq!(remaining, 0);
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; run cargo test --test backend -- --ignored"]
async fn visibility_migration_keeps_existing_content_private_without_changing_history() {
    let fixture = Fixture::new().await;
    let (_, owner) = fixture.user("visibility-migration").await;
    let profile = fixture
        .value(
            "PUT",
            "/api/me",
            &owner,
            json!({"display_name":"Legacy","bio":"Original bio"}),
            StatusCode::OK,
        )
        .await;
    let _entry = fixture
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"completed"}),
            StatusCode::OK,
        )
        .await;
    let run = fixture
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"rating":9,
        "review":"Legacy review","presentation_on":"2001-02-03","state":"completed"}),
            StatusCode::CREATED,
        )
        .await;
    let run = fixture
        .edit(
            "PATCH",
            &run_path(&run),
            &owner,
            &tag(&run),
            json!({"platform":"PC"}),
            StatusCode::OK,
        )
        .await;
    let note = fixture.note(&owner, &run, "Legacy note", None).await;
    let entry = fixture
        .value(
            "GET",
            "/api/me/library/42",
            &owner,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    undo_visibility(&fixture.database.app).await;
    sqlx::migrate!("./migrations")
        .run(&fixture.database.app)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&fixture.database.app)
        .await
        .unwrap();
    let mut migrated_profile = fixture
        .value("GET", "/api/me", &owner, Value::Null, StatusCode::OK)
        .await["profile"]
        .clone();
    assert!(Uuid::parse_str(migrated_profile["public_id"].as_str().unwrap()).is_ok());
    migrated_profile["public_id"] = profile["public_id"].clone();
    assert_eq!(migrated_profile, profile);
    assert_eq!(
        fixture
            .value("GET", &run_path(&run), &owner, Value::Null, StatusCode::OK)
            .await,
        run
    );
    assert_eq!(
        fixture
            .value(
                "GET",
                &format!("/api/me/notes/{}", note["id"].as_str().unwrap()),
                &owner,
                Value::Null,
                StatusCode::OK
            )
            .await,
        note
    );
    let mut migrated = fixture
        .value(
            "GET",
            "/api/me/library/42",
            &owner,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    migrated["public_id"] = entry["public_id"].clone(); // New identity has no historical meaning.
    assert_eq!(migrated, entry);
    let count: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM public_reviews)+(SELECT count(*) FROM public_ratings)+
        (SELECT count(*) FROM public_playthroughs)+(SELECT count(*) FROM public_notes)+(SELECT count(*) FROM public_library_entries)")
        .fetch_one(&fixture.database.app).await.unwrap();
    assert_eq!(count, 0);
    fixture.finish().await;
}

// Undo only feature 5 in disposable fixtures; restore feature 4's gated views.
async fn undo_public_profiles(pool: &PgPool) {
    undo_lists(pool).await;
    sqlx::raw_sql("DROP VIEW public_library_entries,public_reviews,public_ratings,public_playthroughs,public_notes;
        DROP VIEW selected_library_entries;
        DROP VIEW selected_reviews,selected_ratings,selected_playthroughs,selected_notes;
        DROP TABLE profile_handle_reservations;
        ALTER TABLE profiles DROP COLUMN public_id,DROP COLUMN handle,DROP COLUMN bio_visibility;
        DELETE FROM _sqlx_migrations WHERE version=202609160001;")
        .execute(pool).await.unwrap();
    let original = include_str!("../migrations/202609100001_visibility.sql");
    let views = &original[original.find("CREATE VIEW public_reviews").unwrap()
        ..original
            .find("CREATE INDEX public_reviews_owner_date_idx")
            .unwrap()];
    sqlx::raw_sql(sqlx::AssertSqlSafe(views))
        .execute(pool)
        .await
        .unwrap();
}

const PROFILE_SECTIONS: [&str; 6] = [
    "current_games",
    "library",
    "playthroughs",
    "ratings",
    "reviews",
    "notes",
];

async fn owner_profile(f: &Fixture, token: &str) -> Value {
    f.value("GET", "/api/me", token, Value::Null, StatusCode::OK)
        .await["profile"]
        .clone()
}

async fn share_note(f: &Fixture, token: &str, note: &Value) -> Value {
    f.edit(
        "PUT",
        &format!("{}/visibility", note_path(note)),
        token,
        &tag(note),
        json!({"visibility":"public"}),
        StatusCode::OK,
    )
    .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn public_profiles_and_owner_preview_share_projection_counts_and_permissions() {
    let f = Fixture::new().await;
    let (owner_id, owner) = f.user("profile-owner").await;
    let (_, other) = f.user("profile-other").await;
    let profile = f
        .value(
            "PUT",
            "/api/me",
            &owner,
            json!({"display_name":"Public author","bio":"Private biography"}),
            StatusCode::OK,
        )
        .await;
    let id = profile["public_id"].as_str().unwrap();
    assert_ne!(id, owner_id.0.to_string());
    let base = format!("/api/public/profiles/{id}");
    let run = f
        .value(
            "POST",
            START_RUN,
            &owner,
            json!({"igdb_id":42,"platform":"PC"}),
            StatusCode::CREATED,
        )
        .await;
    let run = f
        .edit(
            "PATCH",
            &run_path(&run),
            &owner,
            &tag(&run),
            json!({"rating":8,"review":"Selected review","presentation_on":"2004-02-29"}),
            StatusCode::OK,
        )
        .await;
    let run = share_run(
        &f,
        &owner,
        &run,
        json!({"summary_visibility":"public",
        "rating_visibility":"public","review_visibility":"public"}),
    )
    .await;
    let note = f
        .note(&owner, &run, "Selected note", Some("2003-01-01"))
        .await;
    let note = share_note(&f, &owner, &note).await;
    f.note(&owner, &run, "Private child", Some("2026-01-01"))
        .await;
    f.value(
        "POST",
        "/api/me/playthroughs",
        &owner,
        json!({"igdb_id":42,"rating":1,
        "review":"Private newer run","presentation_on":"2026-01-01"}),
        StatusCode::CREATED,
    )
    .await;
    let entry = f.current_entry(&owner).await;
    let entry = f
        .edit(
            "PUT",
            "/api/me/library/42/visibility",
            &owner,
            &tag(&entry),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    let preview = public_body(&f, "/api/me/public-preview", Some(&owner), StatusCode::OK).await;
    assert_eq!(
        preview,
        json!({"id":id,"handle":null,"display_name":"Public author","bio":null,
        "counts":{"current_games":1,"library":1,"playthroughs":1,"ratings":1,"reviews":1,"notes":1,"lists":0}})
    );
    // Preview has no target user input. Another owner sees only their own selection.
    let other_profile = owner_profile(&f, &other).await;
    let other_preview =
        public_body(&f, "/api/me/public-preview", Some(&other), StatusCode::OK).await;
    assert_eq!(other_preview["id"], other_profile["public_id"]);
    assert_eq!(other_preview["counts"]["reviews"], 0);
    for section in std::iter::once("").chain(PROFILE_SECTIONS) {
        let suffix = if section.is_empty() {
            String::new()
        } else {
            format!("/{section}")
        };
        public_body(
            &f,
            &format!("/api/me/public-preview{suffix}"),
            None,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        for token in [None, Some(owner.as_str()), Some(other.as_str())] {
            public_body(&f, &format!("{base}{suffix}"), token, StatusCode::NOT_FOUND).await;
        }
    }
    let mut previews = Vec::new();
    for section in PROFILE_SECTIONS {
        previews.push(
            public_body(
                &f,
                &format!("/api/me/public-preview/{section}"),
                Some(&owner),
                StatusCode::OK,
            )
            .await,
        );
    }
    assert_eq!(previews[0]["page"]["data"][0]["rating"], 8);
    assert_eq!(previews[0]["page"]["data"][0]["rating_id"], run["id"]);
    let profile = publication(&f, &owner, true).await;
    assert_eq!(public_body(&f, &base, None, StatusCode::OK).await, preview);
    let directs = [
        format!(
            "/api/public/library/{}",
            entry["public_id"].as_str().unwrap()
        ),
        format!(
            "/api/public/library/{}",
            entry["public_id"].as_str().unwrap()
        ),
        format!("/api/public/playthroughs/{}", run["id"].as_str().unwrap()),
        format!("/api/public/ratings/{}", run["id"].as_str().unwrap()),
        format!("/api/public/reviews/{}", run["id"].as_str().unwrap()),
        format!("/api/public/notes/{}", note["id"].as_str().unwrap()),
    ];
    f.server.reset().await; // All reads below must use local game context.
    for (i, section) in PROFILE_SECTIONS.iter().enumerate() {
        for token in [None, Some(owner.as_str()), Some(other.as_str())] {
            let section_body =
                public_body(&f, &format!("{base}/{section}"), token, StatusCode::OK).await;
            assert_eq!(section_body, previews[i]);
            let direct = public_body(&f, &directs[i], token, StatusCode::OK).await;
            assert_eq!(section_body["page"]["data"][0], direct);
            assert_eq!(direct["author_id"], id);
            assert_eq!(direct["game"]["game_name"], "Test Game");
        }
        assert_eq!(
            public_body(
                &f,
                &format!("/api/me/public-preview/{section}"),
                Some(&owner),
                StatusCode::OK
            )
            .await,
            previews[i]
        );
    }
    assert!(f.server.received_requests().await.unwrap().is_empty());
    for route in [&base, "/api/me/public-preview"] {
        let head = f.call("HEAD", route, Some(&owner), Value::Null).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()["cache-control"], "no-store");
        assert!(!head.headers().contains_key("etag"));
        assert!(!head.headers().contains_key("last-modified"));
    }
    let profile = f
        .edit(
            "PUT",
            "/api/me/publication",
            &owner,
            &tag(&profile),
            json!({"public_enabled":true,"bio_visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        public_body(&f, &base, None, StatusCode::OK).await["bio"],
        "Private biography"
    );
    assert_eq!(
        public_body(&f, &base, None, StatusCode::OK).await,
        public_body(&f, "/api/me/public-preview", Some(&owner), StatusCode::OK).await
    );
    f.edit(
        "PUT",
        "/api/me/publication",
        &owner,
        &tag(&profile),
        json!({"public_enabled":false,"bio_visibility":"private"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        public_body(&f, "/api/me/public-preview", Some(&owner), StatusCode::OK).await,
        preview
    );
    for (i, section) in PROFILE_SECTIONS.iter().enumerate() {
        public_body(
            &f,
            &format!("{base}/{section}"),
            Some(&owner),
            StatusCode::NOT_FOUND,
        )
        .await;
        public_body(&f, &directs[i], None, StatusCode::NOT_FOUND).await;
        assert_eq!(
            public_body(
                &f,
                &format!("/api/me/public-preview/{section}"),
                Some(&owner),
                StatusCode::OK
            )
            .await,
            previews[i]
        );
    }
    // Disabling the profile did not change owner content or per-item permissions.
    assert_eq!(
        f.value("GET", &run_path(&run), &owner, Value::Null, StatusCode::OK)
            .await,
        run
    );
    // Independent revocation must affect selected counts and pages even while disabled.
    share_run(&f, &owner, &run, private_parts()).await;
    let counts =
        public_body(&f, "/api/me/public-preview", Some(&owner), StatusCode::OK).await["counts"]
            .clone();
    assert_eq!(
        counts,
        json!({"current_games":1,"library":1,"playthroughs":0,"ratings":0,"reviews":0,"notes":1,"lists":0})
    );
    let library = public_body(
        &f,
        "/api/me/public-preview/library",
        Some(&owner),
        StatusCode::OK,
    )
    .await;
    assert!(library["page"]["data"][0]["rating"].is_null());
    // Public note with private parent still works when the profile is enabled.
    publication(&f, &owner, true).await;
    public_body(&f, &directs[5], None, StatusCode::OK).await;
    f.call("DELETE", &run_path(&run), Some(&owner), Value::Null)
        .await;
    public_body(&f, &directs[5], None, StatusCode::NOT_FOUND).await;
    assert_eq!(
        public_body(&f, &base, None, StatusCode::OK).await["counts"]["notes"],
        0
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn profile_chronology_pagination_and_independent_review_links_use_selected_dates() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("profile-chronology").await;
    let profile = publication(&f, &owner, true).await;
    let base = format!(
        "/api/public/profiles/{}",
        profile["public_id"].as_str().unwrap()
    );
    let mut runs = Vec::new();
    for date in [Some("2000-01-01"), Some("2000-01-01"), None, None] {
        let run = f.value("POST", "/api/me/playthroughs", &owner, json!({"igdb_id":42,
            "rating":10,"review":"Backfilled review","presentation_on":date,"platform":"Private console",
            "started_on":"1999-01-01","finished_on":"2000-01-01"}), StatusCode::CREATED).await;
        runs.push(
            share_run(
                &f,
                &owner,
                &run,
                json!({"summary_visibility":"private",
            "rating_visibility":"private","review_visibility":"public"}),
            )
            .await,
        );
    }
    let private = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,
        "review":"Newer private review","presentation_on":"2026-01-01"}),
            StatusCode::CREATED,
        )
        .await;
    let mut expected = runs
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    expected[..2].sort_by(|a, b| b.cmp(a));
    expected[2..].sort_by(|a, b| b.cmp(a));
    let all = public_body(&f, &format!("{base}/reviews"), None, StatusCode::OK).await;
    assert_eq!(
        all["page"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        public_body(&f, &base, None, StatusCode::OK).await["counts"],
        json!({"current_games":0,"library":0,"playthroughs":0,"ratings":0,"reviews":4,"notes":0,"lists":0})
    );
    for (offset, id) in expected.iter().enumerate() {
        let route = format!("{base}/reviews?limit=1&offset={offset}");
        let page = public_body(&f, &route, None, StatusCode::OK).await;
        assert_eq!(page["page"]["data"][0]["id"], *id);
        assert_eq!(page["page"]["has_more"], offset < 3);
        assert_eq!(
            page,
            public_body(
                &f,
                &format!("/api/me/public-preview/reviews?limit=1&offset={offset}"),
                Some(&owner),
                StatusCode::OK
            )
            .await
        );
        let direct = public_body(
            &f,
            &format!("/api/public/reviews/{id}"),
            None,
            StatusCode::OK,
        )
        .await;
        assert_eq!(page["page"]["data"][0], direct);
        for key in ["rating", "state", "platform", "started_on", "finished_on"] {
            assert!(direct.get(key).is_none());
        }
    }
    for params in [
        "limit=0",
        "limit=51",
        "offset=10001",
        "offset=-1",
        "limit=foo",
        "preview=true",
        "user_id=other",
        "unknown=foo",
    ] {
        for root in [&base, &"/api/me/public-preview".to_owned()] {
            public_body(
                &f,
                &format!("{root}/reviews?{params}"),
                Some(&owner),
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
    }
    let run = f
        .edit(
            "PATCH",
            &run_path(&runs[0]),
            &owner,
            &tag(&runs[0]),
            json!({"review":"Edited yesterday's text"}),
            StatusCode::OK,
        )
        .await;
    let after = public_body(&f, &format!("{base}/reviews"), None, StatusCode::OK).await;
    assert_eq!(
        after["page"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        public_body(
            &f,
            &format!("/api/public/reviews/{}", run["id"].as_str().unwrap()),
            None,
            StatusCode::OK
        )
        .await["presentation_on"],
        "2000-01-01"
    );
    f.edit(
        "PATCH",
        &run_path(&run),
        &owner,
        &tag(&run),
        json!({"presentation_on":"2001-01-01"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        public_body(&f, &format!("{base}/reviews?limit=1"), None, StatusCode::OK).await["page"]["data"]
            [0]["id"],
        run["id"]
    );
    public_body(
        &f,
        &format!("/api/public/reviews/{}", private["id"].as_str().unwrap()),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    // Backdated note edits obey the same policy with independent parent visibility.
    let note = f
        .note(&owner, &private, "Dated note", Some("1980-01-01"))
        .await;
    let note = share_note(&f, &owner, &note).await;
    let undated = f.note(&owner, &private, "Unknown day", None).await;
    share_note(&f, &owner, &undated).await;
    f.edit(
        "PATCH",
        &note_path(&note),
        &owner,
        &tag(&note),
        json!({"text":"Corrected note"}),
        StatusCode::OK,
    )
    .await;
    let notes = public_body(&f, &format!("{base}/notes"), None, StatusCode::OK).await;
    assert_eq!(notes["page"]["data"][0]["id"], note["id"]);
    assert_eq!(notes["page"]["data"][0]["presentation_on"], "1980-01-01");
    assert_eq!(notes["page"]["data"][1]["presentation_on"], Value::Null);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn public_handles_are_unique_revision_protected_and_safe_across_renames() {
    let f = Fixture::new().await;
    let (alice_id, alice) = f.user("handle-alice").await;
    let (_, bob) = f.user("handle-bob").await;
    let alice_profile = publication(&f, &alice, true).await;
    let bob_profile = publication(&f, &bob, true).await;
    for invalid in ["ab", "1abc", "a-b", " abc", "a/b", "åbc", &"x".repeat(31)] {
        let error = f
            .edit(
                "PUT",
                "/api/me/handle",
                &alice,
                &tag(&alice_profile),
                json!({"handle":invalid}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(error["error"]["fields"][0]["field"], "handle");
    }
    for invalid in [
        json!({}),
        json!({"handle":null}),
        json!({"handle":4}),
        json!({"handle":"okay","owner":"bob"}),
    ] {
        f.edit(
            "PUT",
            "/api/me/handle",
            &alice,
            &tag(&alice_profile),
            invalid,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    assert_eq!(
        f.call(
            "PUT",
            "/api/me/handle",
            Some(&alice),
            json!({"handle":"Valid"})
        )
        .await
        .status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        f.call("PUT", "/api/me/handle", None, json!({"handle":"Valid"}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let atag = tag(&alice_profile);
    let btag = tag(&bob_profile);
    let aheaders = [("If-Match", atag.as_str())];
    let bheaders = [("If-Match", btag.as_str())];
    let (a, b) = tokio::join!(
        f.send(
            "PUT",
            "/api/me/handle",
            Some(&alice),
            &aheaders,
            json!({"handle":"Same_Name"})
        ),
        f.send(
            "PUT",
            "/api/me/handle",
            Some(&bob),
            &bheaders,
            json!({"handle":"same_name"})
        ),
    );
    assert!(matches!(
        (a.status().as_u16(), b.status().as_u16()),
        (200, 409) | (409, 200)
    ));
    let (winner, loser, won, lost) = if a.status() == StatusCode::OK {
        (&alice, &bob, a, b)
    } else {
        (&bob, &alice, b, a)
    };
    let saved = json_body(won, StatusCode::OK).await;
    assert_eq!(
        json_body(lost, StatusCode::CONFLICT).await["error"]["code"],
        "handle_unavailable"
    );
    assert_eq!(saved["handle"], "same_name");
    let same = f
        .edit(
            "PUT",
            "/api/me/handle",
            winner,
            &tag(&saved),
            json!({"handle":"SAME_NAME"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(same, saved); // Canonical no-op preserves revision and timestamps.
    let canonical = format!(
        "/api/public/profiles/{}",
        saved["public_id"].as_str().unwrap()
    );
    let resolved = public_body(&f, "/api/public/handles/SAME_NAME", None, StatusCode::OK).await;
    assert_eq!(
        resolved,
        public_body(&f, &canonical, None, StatusCode::OK).await
    );
    let run = f
        .value(
            "POST",
            "/api/me/playthroughs",
            winner,
            json!({"igdb_id":42,"review":"Stable link"}),
            StatusCode::CREATED,
        )
        .await;
    let run = share_run(&f,winner,&run,json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"})).await;
    let direct = format!("/api/public/reviews/{}", run["id"].as_str().unwrap());
    let before = public_body(&f, &direct, None, StatusCode::OK).await;
    let renamed = f
        .edit(
            "PUT",
            "/api/me/handle",
            winner,
            &tag(&saved),
            json!({"handle":"new_name"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(renamed["public_id"], saved["public_id"]);
    assert_eq!(public_body(&f, &direct, None, StatusCode::OK).await, before);
    public_body(
        &f,
        "/api/public/handles/same_name",
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        public_body(&f, &canonical, None, StatusCode::OK).await["handle"],
        "new_name"
    );
    let loser_profile = owner_profile(&f, loser).await;
    f.edit(
        "PUT",
        "/api/me/handle",
        loser,
        &tag(&loser_profile),
        json!({"handle":"same_name"}),
        StatusCode::CONFLICT,
    )
    .await;
    f.edit(
        "PUT",
        "/api/me/handle",
        winner,
        &tag(&saved),
        json!({"handle":"stale_name"}),
        StatusCode::PRECONDITION_FAILED,
    )
    .await;
    let restored = f
        .edit(
            "PUT",
            "/api/me/handle",
            winner,
            &tag(&renamed),
            json!({"handle":"same_name"}),
            StatusCode::OK,
        )
        .await;
    // Two writes against one profile revision cannot silently discard a choice.
    let revision = tag(&restored);
    let headers = [("If-Match", revision.as_str())];
    let (rename, publish) = tokio::join!(
        f.send(
            "PUT",
            "/api/me/handle",
            Some(winner),
            &headers,
            json!({"handle":"race_name"})
        ),
        f.send(
            "PUT",
            "/api/me/publication",
            Some(winner),
            &headers,
            json!({"public_enabled":false})
        ),
    );
    assert!(matches!(
        (rename.status().as_u16(), publish.status().as_u16()),
        (200, 412) | (412, 200)
    ));
    publication(&f, winner, false).await;
    for token in [None, Some(winner.as_str()), Some(loser.as_str())] {
        public_body(&f, &canonical, token, StatusCode::NOT_FOUND).await;
        public_body(
            &f,
            "/api/public/handles/same_name",
            token,
            StatusCode::NOT_FOUND,
        )
        .await;
        public_body(
            &f,
            "/api/public/handles/race_name",
            token,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    assert_eq!(
        public_body(&f, "/api/me/public-preview", Some(winner), StatusCode::OK).await["id"],
        saved["public_id"]
    );
    // Exercise the actual account deletion cascade (not a profile-only deletion).
    let winner_id = if winner == &alice {
        alice_id
    } else {
        let me = f
            .value("GET", "/api/me", winner, Value::Null, StatusCode::OK)
            .await;
        UserId(Uuid::parse_str(me["id"].as_str().unwrap()).unwrap())
    };
    f.auth
        .state()
        .operator()
        .delete_account(
            winner_id,
            OperatorActor::new("profile-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    public_body(&f, &canonical, None, StatusCode::NOT_FOUND).await;
    public_body(&f, &direct, None, StatusCode::NOT_FOUND).await;
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profile_handle_reservations WHERE profile_id=$1")
            .bind(Uuid::parse_str(saved["public_id"].as_str().unwrap()).unwrap())
            .fetch_one(&f.database.app)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
    for retired in ["same_name", "new_name"] {
        f.edit(
            "PUT",
            "/api/me/handle",
            loser,
            &tag(&loser_profile),
            json!({"handle":retired}),
            StatusCode::CONFLICT,
        )
        .await;
        public_body(
            &f,
            &format!("/api/public/handles/{retired}"),
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn public_profile_migration_preserves_choices_and_private_biographies() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("profile-migration").await;
    f.value(
        "PUT",
        "/api/me",
        &owner,
        json!({"display_name":"Existing name","bio":"Never published"}),
        StatusCode::OK,
    )
    .await;
    let profile = publication(&f, &owner, true).await;
    let run = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"review":"Existing public review",
        "presentation_on":"1990-12-31","rating":7}),
            StatusCode::CREATED,
        )
        .await;
    let run = share_run(&f,&owner,&run,json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"})).await;
    let note = f.note(&owner, &run, "Existing private note", None).await;
    undo_public_profiles(&f.database.app).await;
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    let migrated = owner_profile(&f, &owner).await;
    for field in [
        "public_enabled",
        "version",
        "display_name",
        "bio",
        "created_at",
        "updated_at",
    ] {
        assert_eq!(migrated[field], profile[field], "migration changed {field}");
    }
    assert_eq!(migrated["handle"], Value::Null);
    assert_eq!(migrated["bio_visibility"], "private");
    assert!(Uuid::parse_str(migrated["public_id"].as_str().unwrap()).is_ok());
    assert_eq!(
        f.value("GET", &run_path(&run), &owner, Value::Null, StatusCode::OK)
            .await,
        run
    );
    assert_eq!(
        f.value(
            "GET",
            &note_path(&note),
            &owner,
            Value::Null,
            StatusCode::OK
        )
        .await,
        note
    );
    let base = format!(
        "/api/public/profiles/{}",
        migrated["public_id"].as_str().unwrap()
    );
    let public = public_body(&f, &base, None, StatusCode::OK).await;
    assert!(public["bio"].is_null());
    assert_eq!(public["counts"]["reviews"], 1);
    assert_eq!(public["counts"]["notes"], 0);
    assert_eq!(
        public,
        public_body(&f, "/api/me/public-preview", Some(&owner), StatusCode::OK).await
    );
    let review = public_body(
        &f,
        &format!("/api/public/reviews/{}", run["id"].as_str().unwrap()),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(review["presentation_on"], "1990-12-31");
    assert_eq!(review["author_id"], migrated["public_id"]);
    f.finish().await;
}

async fn browse(f: &Fixture, owner: &str, path: &str) -> Value {
    f.value("GET", path, owner, Value::Null, StatusCode::OK)
        .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn browsing_diary_filters_and_cursors_preserve_chosen_history() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("browse-owner").await;
    let (_, other) = f.user("browse-other").await;
    let mut runs = Vec::new();
    for (day, state, rating, review) in [
        (Some("2020-12-31"), "completed", Some(8), Some("Finished")),
        (Some("2020-02-29"), "paused", None, Some("Paused review")),
        (Some("2020-02-29"), "completed", Some(6), None),
        (Some("2019-12-31"), "abandoned", None, None),
        (None, "paused", Some(5), Some("Unknown")),
        (None, "completed", None, None),
    ] {
        runs.push(f.value("POST", "/api/me/playthroughs", &owner,
            json!({"igdb_id":42,"presentation_on":day,"state":state,"rating":rating,"review":review}), StatusCode::CREATED).await);
    }
    for (query, count) in [
        ("year=2020", 3),
        ("year=2020&has_review=true", 2),
        ("year=2020&has_rating=false", 1),
        ("state=paused", 2),
        ("state=completed&has_review=false", 2),
        ("undated=true", 2),
        ("undated=false", 4),
        ("from_on=2020-02-29&to_on=2020-02-29", 2),
        ("q=TeSt&igdb_id=42", 6),
        ("q=%25", 0),
        ("igdb_id=43", 0),
    ] {
        let page = browse(&f, &owner, &format!("/api/me/playthroughs?{query}")).await;
        assert_eq!(page["total"], count, "{query}");
        assert_eq!(
            page["data"].as_array().unwrap().len(),
            count as usize,
            "{query}"
        );
        assert_eq!(page["has_more"], false);
    }
    assert_eq!(
        browse(&f, &other, "/api/me/playthroughs?year=2020").await["total"],
        0
    );
    let first = browse(&f, &owner, "/api/me/playthroughs?limit=1").await;
    assert_eq!(first["data"][0]["id"], runs[0]["id"]);
    let cursor = first["next_cursor"].as_str().unwrap();
    // Editing the anchor, even deleting it, does not invalidate its value cursor.
    f.edit(
        "PATCH",
        &run_path(&runs[0]),
        &owner,
        &tag(&runs[0]),
        json!({"review":"Corrected"}),
        StatusCode::OK,
    )
    .await;
    f.value(
        "DELETE",
        &run_path(&runs[0]),
        &owner,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    // A new row ahead of the cursor cannot duplicate a previously read item.
    f.value(
        "POST",
        "/api/me/playthroughs",
        &owner,
        json!({"igdb_id":42,"presentation_on":"2021-01-01"}),
        StatusCode::CREATED,
    )
    .await;
    let backfill = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"presentation_on":"2020-06-01"}),
            StatusCode::CREATED,
        )
        .await;
    let mut after = cursor.to_owned();
    let mut seen = Vec::new();
    loop {
        let page = browse(
            &f,
            &owner,
            &format!("/api/me/playthroughs?limit=1&after={after}"),
        )
        .await;
        assert_eq!(page["total"], 7);
        seen.push(page["data"][0]["id"].clone());
        if page["next_cursor"].is_null() {
            assert_eq!(page["has_more"], false);
            break;
        }
        after = page["next_cursor"].as_str().unwrap().to_owned();
    }
    let mut expected = runs[1..].to_vec();
    expected.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    expected.sort_by(|a, b| {
        b["presentation_on"]
            .as_str()
            .cmp(&a["presentation_on"].as_str())
    });
    let mut expected_ids = vec![backfill["id"].clone()];
    expected_ids.extend(expected.iter().map(|row| row["id"].clone()));
    assert_eq!(seen, expected_ids);
    let note = f
        .note(
            &owner,
            &runs[1],
            "Reached 50%_ of the game",
            Some("2020-02-29"),
        )
        .await;
    f.note(&owner, &runs[2], "Other note", Some("2020-02-29"))
        .await;
    f.note(&owner, &runs[1], "Unknown day", None).await;
    let notes = browse(
        &f,
        &owner,
        "/api/me/diary?year=2020&state=paused&has_review=true&has_rating=false&q=TEST&text=50%25_",
    )
    .await;
    assert_eq!(notes["total"], 1);
    assert_eq!(notes["data"][0]["id"], note["id"]);
    let nested = format!("{}/notes?undated=true", run_path(&runs[1]));
    assert_eq!(browse(&f, &owner, &nested).await["total"], 1);
    let page = browse(&f, &owner, "/api/me/diary?limit=1").await;
    let next = browse(
        &f,
        &owner,
        &format!(
            "/api/me/diary?limit=1&after={}",
            page["next_cursor"].as_str().unwrap()
        ),
    )
    .await;
    assert_ne!(page["data"][0]["id"], next["data"][0]["id"]);
    assert_eq!(next["total"], 3);
    for query in [
        "year=0",
        "year=10000",
        "year=2020&from_on=2020-01-01",
        "undated=true&year=2020",
        "from_on=2021-01-01&to_on=2020-01-01",
        "year=abc",
        "has_rating=1",
        "state=bad",
        "after=bad",
        "text=%00",
        "q=%0A",
        "unknown=x",
    ] {
        f.value(
            "GET",
            &format!("/api/me/diary?{query}"),
            &owner,
            Value::Null,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    f.value(
        "GET",
        &format!("/api/me/diary?offset=1&after={cursor}"),
        &owner,
        Value::Null,
        StatusCode::BAD_REQUEST,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn browsing_library_sorts_totals_and_play_activity_are_independent_of_edits() {
    let f = Fixture::new().await;
    let (owner_id, owner) = f.user("browse-library").await;
    let (_, other) = f.user("browse-library-other").await;
    for (id, name) in [(43, "alpha"), (44, "Alpha"), (45, "Zulu"), (46, "Undated")] {
        sqlx::query("INSERT INTO games(id,igdb_id,name,slug,source_url) VALUES ($1,$2,$3,$3,'https://example.test')")
            .bind(Uuid::new_v4()).bind(id as i64).bind(name).execute(&f.database.app).await.unwrap();
    }
    for (id, status) in [
        (42, "backlog"),
        (43, "playing"),
        (44, "on_hold"),
        (45, "completed"),
        (46, "dropped"),
    ] {
        f.value(
            "PUT",
            &format!("/api/me/library/{id}"),
            &owner,
            json!({"status":status}),
            StatusCode::OK,
        )
        .await;
    }
    // Equal recording dates exercise deterministic title/IGDB ties.
    sqlx::query("UPDATE library_entries SET created_at='2020-01-01' WHERE user_id=$1")
        .bind(owner_id.0.as_bytes().as_slice())
        .execute(&f.database.app)
        .await
        .unwrap();
    sqlx::query("UPDATE library_entries SET created_at='2022-01-01' WHERE user_id=$1 AND game_id=(SELECT id FROM games WHERE igdb_id=45)")
        .bind(owner_id.0.as_bytes().as_slice()).execute(&f.database.app).await.unwrap();
    let mut runs = Vec::new();
    for (id, rating, start, end) in [
        (42, Some(9), Some("2010-01-01"), Some("2010-12-31")),
        (43, Some(8), Some("2021-01-01"), None),
        (44, Some(8), None, Some("2020-06-01")),
        (45, None, None, None),
    ] {
        runs.push(f.value("POST", "/api/me/playthroughs", &owner, json!({"igdb_id":id,"rating":rating,"review":"History","started_on":start,"finished_on":end}), StatusCode::CREATED).await);
    }
    for (sort, expected) in [
        ("title", vec![43, 44, 42, 46, 45]),
        ("rating", vec![42, 43, 44, 46, 45]),
        ("date_added", vec![45, 43, 44, 42, 46]),
        ("recently_played", vec![43, 44, 42, 46, 45]),
    ] {
        let page = browse(&f, &owner, &format!("/api/me/library?sort={sort}")).await;
        assert_eq!(page["total"], 5);
        assert_eq!(
            page["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["game"]["igdb_id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            page["status_counts"],
            json!({"backlog":1,"playing":1,"on_hold":1,"completed":1,"dropped":1})
        );
    }
    let filtered = browse(&f, &owner, "/api/me/library?q=alpha&status=playing&limit=1").await;
    assert_eq!(filtered["total"], 1);
    assert_eq!(
        filtered["status_counts"],
        json!({"backlog":0,"playing":1,"on_hold":1,"completed":0,"dropped":0})
    );
    let empty = browse(&f, &owner, "/api/me/library?has_rating=true&offset=20").await;
    assert_eq!(empty["total"], 3);
    assert_eq!(empty["data"], json!([]));
    assert_eq!(empty["has_more"], false);
    assert_eq!(browse(&f, &other, "/api/me/library").await["total"], 0);
    let before = browse(&f, &owner, "/api/me/library?sort=recently_played&limit=2").await;
    let corrected = f
        .edit(
            "PATCH",
            &run_path(&runs[0]),
            &owner,
            &tag(&runs[0]),
            json!({"review":"Corrected today","presentation_on":"2030-01-01"}),
            StatusCode::OK,
        )
        .await;
    share_run(&f,&owner,&corrected,json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await;
    publication(&f, &owner, true).await;
    f.value(
        "PUT",
        "/api/me",
        &owner,
        json!({"display_name":"Changed","bio":"Changed"}),
        StatusCode::OK,
    )
    .await;
    let after = browse(&f, &owner, "/api/me/library?sort=recently_played&limit=2").await;
    assert_eq!(before, after);
    assert_eq!(after["data"][0]["last_played_on"], "2021-01-01");
    assert_eq!(after["has_more"], true);
    let last = browse(&f, &owner, "/api/me/library?sort=recently_played&offset=2").await;
    assert_eq!(last["data"][0]["game"]["igdb_id"], 42);
    assert!(last["data"][1]["last_played_on"].is_null());
    // A deliberate play-date correction, unlike review edits, changes this sort.
    f.edit(
        "PATCH",
        &run_path(&runs[1]),
        &owner,
        &tag(&runs[1]),
        json!({"started_on":"2000-01-01"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        browse(&f, &owner, "/api/me/library?sort=recently_played").await["data"][0]["game"]["igdb_id"],
        44
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn browsing_public_filters_counts_and_cursors_use_selected_components_only() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("browse-public").await;
    let (_, other) = f.user("browse-public-other").await;
    let profile = publication(&f, &owner, true).await;
    let base = format!(
        "/api/public/profiles/{}",
        profile["public_id"].as_str().unwrap()
    );
    let entry = f
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    f.edit(
        "PUT",
        "/api/me/library/42/visibility",
        &owner,
        &tag(&entry),
        json!({"visibility":"public"}),
        StatusCode::OK,
    )
    .await;
    let mut runs = Vec::new();
    for (day, selected) in [
        ("2020-12-31", true),
        ("2020-01-01", true),
        ("2030-01-01", false),
    ] {
        let run = f.value("POST","/api/me/playthroughs",&owner,json!({"igdb_id":42,"state":"completed","rating":10,"review":"Secret","presentation_on":day,"finished_on":day}),StatusCode::CREATED).await;
        runs.push(if selected {share_run(&f,&owner,&run,json!({"summary_visibility":"public","rating_visibility":"private","review_visibility":"private"})).await} else {run});
    }
    let run = share_run(&f,&owner,&runs[1],json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await;
    runs[1] = run;
    for (section, query, total) in [
        (
            "playthroughs",
            "year=2020&state=completed&has_rating=false&has_review=false",
            1,
        ),
        ("playthroughs", "has_rating=true&has_review=true", 1),
        ("ratings", "year=2030", 0),
        ("reviews", "text=Secret&year=2020", 1),
        ("library", "has_rating=true&status=playing&sort=rating", 1),
        ("library", "sort=recently_played", 1),
        ("current_games", "q=TEST", 1),
    ] {
        let url = format!("{base}/{section}?{query}");
        let anonymous = public_body(&f, &url, None, StatusCode::OK).await;
        assert_eq!(anonymous["page"]["total"], total, "{url}");
        for token in [&owner, &other] {
            assert_eq!(
                public_body(&f, &url, Some(token), StatusCode::OK).await,
                anonymous
            );
        }
        let preview = public_body(
            &f,
            &format!("/api/me/public-preview/{section}?{query}"),
            Some(&owner),
            StatusCode::OK,
        )
        .await;
        assert_eq!(preview, anonymous);
    }
    let note = f
        .note(&owner, &runs[2], "Public note", Some("1999-12-31"))
        .await;
    share_note(&f, &owner, &note).await;
    f.note(&owner, &runs[0], "Private note", Some("1999-12-31"))
        .await;
    assert_eq!(
        public_body(
            &f,
            &format!("{base}/notes?year=1999&text=note"),
            None,
            StatusCode::OK
        )
        .await["page"]["total"],
        1
    );
    for (section, query) in [
        ("notes", "state=completed"),
        ("reviews", "has_rating=true"),
        ("ratings", "state=completed"),
        ("library", "sort=date_added"),
        ("library", "year=2020"),
        ("playthroughs", "status=playing"),
    ] {
        public_body(
            &f,
            &format!("{base}/{section}?{query}"),
            None,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let first = public_body(
        &f,
        &format!("{base}/playthroughs?year=2020&limit=1"),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first["page"]["total"], 2);
    let cursor = first["page"]["next_cursor"].as_str().unwrap();
    assert!(!cursor.contains("2030"));
    let next_url = format!("{base}/playthroughs?year=2020&limit=1&after={cursor}");
    let next = public_body(&f, &next_url, None, StatusCode::OK).await;
    assert_eq!(next["page"]["data"][0]["id"], runs[1]["id"]);
    // Another public game establishes an ordering that the newer private run
    // must not change. Private membership must not enter totals or status facets.
    for id in [43_i64, 44] {
        sqlx::query("INSERT INTO games(id,igdb_id,name,slug,source_url) VALUES ($1,$2,'Other','other','https://example.test')")
            .bind(Uuid::new_v4()).bind(id).execute(&f.database.app).await.unwrap();
        let entry = f
            .value(
                "PUT",
                &format!("/api/me/library/{id}"),
                &owner,
                json!({"status":"backlog"}),
                StatusCode::OK,
            )
            .await;
        if id == 43 {
            f.edit(
                "PUT",
                &format!("/api/me/library/{id}/visibility"),
                &owner,
                &tag(&entry),
                json!({"visibility":"public"}),
                StatusCode::OK,
            )
            .await;
            let run = f
                .value(
                    "POST",
                    "/api/me/playthroughs",
                    &owner,
                    json!({"igdb_id":43,"started_on":"2025-01-01"}),
                    StatusCode::CREATED,
                )
                .await;
            share_run(&f,&owner,&run,json!({"summary_visibility":"public","rating_visibility":"private","review_visibility":"private"})).await;
        }
    }
    // Private rating/play dates and text changes do not perturb public metadata.
    let before = public_body(
        &f,
        &format!("{base}/library?sort=recently_played"),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(before["page"]["total"], 2);
    assert_eq!(before["page"]["status_counts"]["backlog"], 1);
    assert_eq!(before["page"]["data"][0]["game"]["igdb_id"], 43);
    f.edit(
        "PATCH",
        &run_path(&runs[2]),
        &owner,
        &tag(&runs[2]),
        json!({"finished_on":"2099-01-01","rating":1,"review":"Changed"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        public_body(
            &f,
            &format!("{base}/library?sort=recently_played"),
            None,
            StatusCode::OK
        )
        .await,
        before
    );
    share_run(&f, &owner, &runs[1], private_parts()).await;
    let revoked = public_body(&f, &next_url, None, StatusCode::OK).await;
    assert_eq!(revoked["page"]["total"], 1);
    assert_eq!(revoked["page"]["data"], json!([]));
    assert_eq!(revoked["page"]["has_more"], false);
    publication(&f, &owner, false).await;
    public_body(&f, &next_url, None, StatusCode::NOT_FOUND).await;
    let preview = public_body(
        &f,
        "/api/me/public-preview/playthroughs?year=2020",
        Some(&owner),
        StatusCode::OK,
    )
    .await;
    assert_eq!(preview["page"]["total"], 1);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn browsing_pages_and_totals_share_a_snapshot_during_concurrent_writes() {
    let f = Fixture::new().await;
    let (owner_id, owner) = f.user("browse-snapshot").await;
    let entry = f
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    f.edit(
        "PUT",
        "/api/me/library/42/visibility",
        &owner,
        &tag(&entry),
        json!({"visibility":"public"}),
        StatusCode::OK,
    )
    .await;
    let profile = publication(&f, &owner, true).await;
    let public = format!(
        "/api/public/profiles/{}/library?status=playing",
        profile["public_id"].as_str().unwrap()
    );
    let ((), ()) = tokio::join!(
        async {
            for _ in 0..40 {
                sqlx::query("UPDATE library_entries SET status=CASE WHEN status='playing' THEN 'backlog' ELSE 'playing' END WHERE user_id=$1")
                    .bind(owner_id.0.as_bytes().as_slice()).execute(&f.database.app).await.unwrap();
                tokio::task::yield_now().await;
            }
        },
        async {
            for _ in 0..20 {
                let private = browse(&f, &owner, "/api/me/library?status=playing").await;
                let public = public_body(&f, &public, None, StatusCode::OK).await;
                for page in [&private, &public["page"]] {
                    assert_eq!(
                        page["total"].as_u64().unwrap(),
                        page["data"].as_array().unwrap().len() as u64
                    );
                    assert_eq!(page["total"], page["status_counts"]["playing"]);
                    assert_eq!(
                        page["status_counts"]["playing"].as_i64().unwrap()
                            + page["status_counts"]["backlog"].as_i64().unwrap(),
                        1
                    );
                    assert_eq!(page["has_more"], false);
                }
            }
        }
    );
    f.finish().await;
}

// Remove feature 7 before reconstructing older schemas in disposable fixtures.
async fn undo_lists(pool: &PgPool) {
    undo_statistics(pool).await;
    sqlx::raw_sql(
        "DROP VIEW public_lists,selected_lists,game_list_details;
        DROP TABLE game_list_members,game_lists;
        DROP FUNCTION game_list_revision();
        DELETE FROM _sqlx_migrations WHERE version=202609160002;",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn list_games_fixture(f: &Fixture) {
    for id in [42_i64, 43, 44] {
        sqlx::query("INSERT INTO games(id,igdb_id,name,slug,source_url) VALUES ($1,$2,$3,$3,'https://example.test')")
            .bind(Uuid::new_v4()).bind(id).bind(format!("Game {id}"))
            .execute(&f.database.app).await.unwrap();
    }
}

fn list_path(list: &Value) -> String {
    format!("/api/me/lists/{}", list["list"]["id"].as_str().unwrap())
}
fn member_ids(list: &Value) -> Vec<i64> {
    list["games"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["igdb_id"].as_i64().unwrap())
        .collect()
}
async fn edit_list(f: &Fixture, owner: &str, list: &Value, suffix: &str, input: Value) -> Value {
    f.edit(
        "PUT",
        &format!("{}{suffix}", list_path(list)),
        owner,
        &tag(&list["list"]),
        input,
        StatusCode::OK,
    )
    .await
}
async fn new_list(f: &Fixture, owner: &str, title: &str) -> Value {
    f.value(
        "POST",
        "/api/me/lists",
        owner,
        json!({"title":title}),
        StatusCode::CREATED,
    )
    .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn ordered_lists_validate_edits_and_preserve_independent_history() {
    let f = Fixture::new().await;
    let (uid, owner) = f.user("lists-owner").await;
    let (_, other) = f.user("lists-other").await;
    list_games_fixture(&f).await;
    let entry = f
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"backlog"}),
            StatusCode::OK,
        )
        .await;
    let run = f.value("POST", "/api/me/playthroughs", &owner,
        json!({"igdb_id":42,"rating":8,"review":"Private history","presentation_on":"2001-01-01"}), StatusCode::CREATED).await;
    let list = new_list(&f, &owner, "Play next").await;
    assert_eq!(list["list"]["kind"], "custom");
    assert_eq!(list["list"]["visibility"], "private");
    assert_eq!(list["list"]["version"], 1);
    assert_eq!(list["games"], json!([]));
    let uri = list_path(&list);
    for method in ["GET", "PUT", "DELETE"] {
        let response = f
            .send(
                method,
                &uri,
                Some(&other),
                &[("If-Match", "\"1\"")],
                json!({"title":"Takeover"}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(
        f.call("GET", &uri, None, Value::Null).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for suffix in ["/games", "/visibility"] {
        let input = if suffix == "/games" {
            json!({"igdb_ids":[42]})
        } else {
            json!({"visibility":"public"})
        };
        f.edit(
            "PUT",
            &format!("{uri}{suffix}"),
            &other,
            "\"1\"",
            input,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    for input in [
        json!({"title":"  "}),
        json!({"title":"a".repeat(121)}),
        json!({"title":"ok","description":"x".repeat(5001)}),
        json!({"title":"x\u{0}"}),
    ] {
        f.value(
            "POST",
            "/api/me/lists",
            &owner,
            input.clone(),
            StatusCode::BAD_REQUEST,
        )
        .await;
        f.edit("PUT", &uri, &owner, "\"1\"", input, StatusCode::BAD_REQUEST)
            .await;
    }
    for input in [
        json!({"title":"x","kind":"wishlist"}),
        json!({"title":"x","visibility":"public"}),
        json!({"title":null}),
    ] {
        f.value(
            "POST",
            "/api/me/lists",
            &owner,
            input,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for invalid_tag in ["*", "1", "W/\"1\""] {
        let expected = if invalid_tag.starts_with("W/") {
            StatusCode::PRECONDITION_FAILED
        } else {
            StatusCode::BAD_REQUEST
        };
        f.edit(
            "PUT",
            &format!("{uri}/games"),
            &owner,
            invalid_tag,
            json!({"igdb_ids":[42]}),
            expected,
        )
        .await;
    }
    for input in [
        json!([42, 42]),
        json!([0]),
        json!([-1]),
        json!([42, 999]),
        json!((1..=501).collect::<Vec<_>>()),
    ] {
        f.edit(
            "PUT",
            &format!("{uri}/games"),
            &owner,
            "\"1\"",
            json!({"igdb_ids":input}),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(browse(&f, &owner, &uri).await, list);
    }
    for (method, suffix, input) in [
        ("PUT", "", json!({"title":"x"})),
        ("PUT", "/games", json!({"igdb_ids":[42]})),
        ("PUT", "/visibility", json!({"visibility":"public"})),
        ("DELETE", "", Value::Null),
    ] {
        f.value(
            method,
            &format!("{uri}{suffix}"),
            &owner,
            input,
            StatusCode::PRECONDITION_REQUIRED,
        )
        .await;
    }
    let list = edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[43,42,44]})).await;
    assert_eq!(member_ids(&list), [43, 42, 44]);
    assert_eq!(list["list"]["version"], 2);
    assert_eq!(list["list"]["game_count"], 3);
    let noop = edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[43,42,44]})).await;
    assert_eq!(noop, list);
    let list = edit_list(
        &f,
        &owner,
        &list,
        "",
        json!({"title":"  My next games  ","description":"Notes\nabout games"}),
    )
    .await;
    assert_eq!(list["list"]["title"], "  My next games  ");
    let list = edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[44,43]})).await;
    assert_eq!(member_ids(&browse(&f, &owner, &uri).await), [44, 43]);
    let current = f.current_entry(&owner).await;
    assert_eq!(current["status"], entry["status"]);
    assert_eq!(current["version"], entry["version"]);
    assert_eq!(browse(&f, &owner, &run_path(&run)).await, run);
    assert_eq!(browse(&f, &owner, "/api/me/library").await["total"], 1);
    assert_eq!(browse(&f, &owner, "/api/me/playthroughs").await["total"], 1);
    f.value(
        "DELETE",
        "/api/me/library/42",
        &owner,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(browse(&f, &owner, &uri).await, list);
    assert_eq!(browse(&f, &other, "/api/me/lists").await["total"], 0);
    let empty = edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[]})).await;
    assert_eq!(empty["list"]["game_count"], 0);
    let list = edit_list(&f, &owner, &empty, "/games", json!({"igdb_ids":[42]})).await;
    f.edit(
        "DELETE",
        &uri,
        &owner,
        &tag(&empty["list"]),
        Value::Null,
        StatusCode::PRECONDITION_FAILED,
    )
    .await;
    f.edit(
        "DELETE",
        &uri,
        &owner,
        &tag(&list["list"]),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM game_list_members")
            .fetch_one(&f.database.app)
            .await
            .unwrap(),
        0
    );
    assert_eq!(browse(&f, &owner, &run_path(&run)).await, run);
    // Account deletion removes lists/members without deleting shared catalog games.
    let list = new_list(&f, &owner, "Deleted account").await;
    edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[42]})).await;
    sqlx::query("DELETE FROM gameboxd_auth.users WHERE id=$1")
        .bind(uid.0.as_bytes().as_slice())
        .execute(&f.database.app)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM game_lists")
            .fetch_one(&f.database.app)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM game_list_members")
            .fetch_one(&f.database.app)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM games")
            .fetch_one(&f.database.app)
            .await
            .unwrap(),
        3
    );
    assert!(f.server.received_requests().await.unwrap().is_empty());
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn public_lists_share_only_selected_membership_and_revoke_every_access_path() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("public-lists-owner").await;
    let (_, other) = f.user("public-lists-other").await;
    list_games_fixture(&f).await;
    let profile = owner_profile(&f, &owner).await;
    let profile_uri = format!(
        "/api/public/profiles/{}",
        profile["public_id"].as_str().unwrap()
    );
    let list = new_list(&f, &owner, "Play next").await;
    let list = edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[43,42]})).await;
    let id = list["list"]["id"].as_str().unwrap().to_owned();
    let public_uri = format!("/api/public/lists/{id}");
    let preview_uri = format!("/api/me/public-preview/lists/{id}");
    public_body(&f, &preview_uri, Some(&owner), StatusCode::NOT_FOUND).await;
    let list = edit_list(
        &f,
        &owner,
        &list,
        "/visibility",
        json!({"visibility":"public"}),
    )
    .await;
    let preview = public_body(&f, &preview_uri, Some(&owner), StatusCode::OK).await;
    for visitor in [None, Some(owner.as_str()), Some(other.as_str())] {
        public_body(&f, &public_uri, visitor, StatusCode::NOT_FOUND).await;
    }
    public_body(&f, &preview_uri, Some(&other), StatusCode::NOT_FOUND).await;
    public_body(&f, &preview_uri, None, StatusCode::UNAUTHORIZED).await;
    let selected = public_body(
        &f,
        "/api/me/public-preview/lists",
        Some(&owner),
        StatusCode::OK,
    )
    .await;
    assert_eq!(selected["page"]["data"], json!([preview["list"]]));
    publication(&f, &owner, true).await;
    for visitor in [None, Some(owner.as_str()), Some(other.as_str())] {
        assert_eq!(
            public_body(&f, &public_uri, visitor, StatusCode::OK).await,
            preview
        );
        assert_eq!(
            public_body(&f, &format!("{profile_uri}/lists"), visitor, StatusCode::OK).await,
            selected
        );
    }
    assert_eq!(
        public_body(&f, &profile_uri, None, StatusCode::OK).await["counts"]["lists"],
        1
    );
    let mut keys: Vec<_> = preview["list"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "author_id",
            "author_name",
            "description",
            "game_count",
            "id",
            "kind",
            "title"
        ]
    );
    for game in preview["games"].as_array().unwrap() {
        let mut keys: Vec<_> = game
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(keys, ["cover_url", "game_name", "game_slug", "igdb_id"]);
    }
    // Private history, membership, ratings and other lists cannot change public rows or counts.
    f.value(
        "PUT",
        "/api/me/library/42",
        &owner,
        json!({"status":"completed"}),
        StatusCode::OK,
    )
    .await;
    f.value(
        "POST",
        "/api/me/playthroughs",
        &owner,
        json!({"igdb_id":42,"rating":9,"review":"Secret","platform":"Secret"}),
        StatusCode::CREATED,
    )
    .await;
    new_list(&f, &owner, "AAA private list").await;
    assert_eq!(
        public_body(&f, &public_uri, None, StatusCode::OK).await,
        preview
    );
    assert_eq!(
        public_body(&f, &format!("{profile_uri}/lists"), None, StatusCode::OK).await,
        selected
    );
    let list = edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[44,43]})).await;
    assert_eq!(
        member_ids(&public_body(&f, &public_uri, None, StatusCode::OK).await),
        [44, 43]
    );
    let list = edit_list(&f, &owner, &list, "", json!({"title":"Play next 2"})).await;
    assert_eq!(
        public_body(&f, &public_uri, None, StatusCode::OK).await["list"]["title"],
        "Play next 2"
    );
    for enabled in [false, true] {
        publication(&f, &owner, enabled).await;
        public_body(
            &f,
            &public_uri,
            None,
            if enabled {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
        )
        .await;
        public_body(
            &f,
            &format!("{profile_uri}/lists"),
            None,
            if enabled {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
        )
        .await;
        assert_eq!(
            public_body(&f, &preview_uri, Some(&owner), StatusCode::OK).await["list"]["id"],
            id
        );
    }
    let list = edit_list(
        &f,
        &owner,
        &list,
        "/visibility",
        json!({"visibility":"private"}),
    )
    .await;
    for visitor in [None, Some(owner.as_str()), Some(other.as_str())] {
        public_body(&f, &public_uri, visitor, StatusCode::NOT_FOUND).await;
    }
    public_body(&f, &preview_uri, Some(&owner), StatusCode::NOT_FOUND).await;
    assert_eq!(
        public_body(&f, &format!("{profile_uri}/lists"), None, StatusCode::OK).await["page"]["total"],
        0
    );
    assert_eq!(
        public_body(&f, &profile_uri, None, StatusCode::OK).await["counts"]["lists"],
        0
    );
    let list = edit_list(
        &f,
        &owner,
        &list,
        "/visibility",
        json!({"visibility":"public"}),
    )
    .await;
    f.edit(
        "DELETE",
        &list_path(&list),
        &owner,
        &tag(&list["list"]),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    public_body(&f, &public_uri, None, StatusCode::NOT_FOUND).await;
    public_body(&f, &preview_uri, Some(&owner), StatusCode::NOT_FOUND).await;
    let head = f.call("HEAD", &public_uri, None, Value::Null).await;
    assert_eq!(head.status(), StatusCode::NOT_FOUND);
    assert_eq!(head.headers()["cache-control"], "no-store");
    assert!(!head.headers().contains_key("etag"));
    assert!(f.server.received_requests().await.unwrap().is_empty());
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn list_concurrency_preserves_membership_and_arbitrates_favorites() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("concurrent-lists").await;
    list_games_fixture(&f).await;
    let (a, b) = tokio::join!(
        f.call(
            "POST",
            "/api/me/lists",
            Some(&owner),
            json!({"kind":"favorites","title":"Favorites"})
        ),
        f.call(
            "POST",
            "/api/me/lists",
            Some(&owner),
            json!({"kind":"favorites","title":"Favorites"})
        )
    );
    let (winner, loser) = if a.status() == StatusCode::CREATED {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(
        json_body(loser, StatusCode::CONFLICT).await["error"]["code"],
        "favorites_exists"
    );
    let list = json_body(winner, StatusCode::CREATED).await;
    assert_eq!(list["list"]["kind"], "favorites");
    let mut list = edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[42,43]})).await;
    let uri = list_path(&list);
    // Addition and reorder, removal and reorder, then publication and reorder.
    for (suffix, input) in [
        ("/games", json!({"igdb_ids":[42,43,44]})),
        ("/games", json!({"igdb_ids":[42]})),
        ("/visibility", json!({"visibility":"public"})),
        ("", json!({"title":"Renamed favorites"})),
    ] {
        let revision = tag(&list["list"]);
        let first_uri = format!("{uri}{suffix}");
        let reorder_uri = format!("{uri}/games");
        let mut reordered = member_ids(&list);
        reordered.reverse();
        // Always make a real membership change, including the single-member case.
        if reordered == member_ids(&list) {
            reordered.push(43);
        }
        let headers = [("If-Match", revision.as_str())];
        let (a, b) = tokio::join!(
            f.send("PUT", &first_uri, Some(&owner), &headers, input),
            f.send(
                "PUT",
                &reorder_uri,
                Some(&owner),
                &headers,
                json!({"igdb_ids":reordered})
            )
        );
        let (winner, loser) = if a.status() == StatusCode::OK {
            (a, b)
        } else {
            (b, a)
        };
        json_body(loser, StatusCode::PRECONDITION_FAILED).await;
        let next = json_body(winner, StatusCode::OK).await;
        assert_eq!(
            next["list"]["version"].as_i64().unwrap(),
            list["list"]["version"].as_i64().unwrap() + 1
        );
        assert_eq!(browse(&f, &owner, &uri).await, next);
        list = next;
    }
    // Deletion and a member edit serialize against the same aggregate lock.
    let revision = tag(&list["list"]);
    let headers = [("If-Match", revision.as_str())];
    let games_uri = format!("{uri}/games");
    let (delete, edit) = tokio::join!(
        f.send("DELETE", &uri, Some(&owner), &headers, Value::Null),
        f.send(
            "PUT",
            &games_uri,
            Some(&owner),
            &headers,
            json!({"igdb_ids":[]})
        )
    );
    if delete.status() == StatusCode::NO_CONTENT {
        assert_eq!(edit.status(), StatusCode::NOT_FOUND);
    } else {
        assert_eq!(delete.status(), StatusCode::PRECONDITION_FAILED);
        let list = json_body(edit, StatusCode::OK).await;
        f.edit(
            "DELETE",
            &uri,
            &owner,
            &tag(&list["list"]),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    }
    let recreated = f
        .value(
            "POST",
            "/api/me/lists",
            &owner,
            json!({"kind":"favorites","title":"Favorites"}),
            StatusCode::CREATED,
        )
        .await;
    assert_ne!(recreated["list"]["id"], list["list"]["id"]);
    assert_eq!(recreated["list"]["visibility"], "private");
    assert_eq!(member_ids(&recreated), Vec::<i64>::new());
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn list_browsing_and_migration_keep_private_data_out_of_public_counts() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("list-migration").await;
    list_games_fixture(&f).await;
    let profile = publication(&f, &owner, true).await;
    let run = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"rating":7,"review":"Old private history"}),
            StatusCode::CREATED,
        )
        .await;
    undo_lists(&f.database.app).await;
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    assert_eq!(owner_profile(&f, &owner).await, profile);
    assert_eq!(browse(&f, &owner, &run_path(&run)).await, run);
    assert_eq!(browse(&f, &owner, "/api/me/lists").await["total"], 0);
    let public_uri = format!(
        "/api/public/profiles/{}/lists",
        profile["public_id"].as_str().unwrap()
    );
    assert_eq!(
        public_body(&f, &public_uri, None, StatusCode::OK).await["page"]["total"],
        0
    );
    let mut public_ids = Vec::new();
    for title in ["z last", "Alpha", "Alpha", "50%_ fun"] {
        let list = new_list(&f, &owner, title).await;
        let list = edit_list(
            &f,
            &owner,
            &list,
            "/visibility",
            json!({"visibility":"public"}),
        )
        .await;
        public_ids.push(list["list"]["id"].as_str().unwrap().to_owned());
    }
    new_list(&f, &owner, "AAA private").await;
    let owner_page = browse(&f, &owner, "/api/me/lists?limit=1").await;
    assert_eq!(owner_page["data"][0]["title"], "50%_ fun");
    assert_eq!(owner_page["total"], 5);
    let first = public_body(&f, &format!("{public_uri}?limit=1"), None, StatusCode::OK).await;
    assert_eq!(first["page"]["total"], 4);
    assert_eq!(first["page"]["has_more"], true);
    assert_eq!(first["page"]["next_cursor"], Value::Null);
    let ties = public_body(
        &f,
        &format!("{public_uri}?limit=2&offset=1"),
        None,
        StatusCode::OK,
    )
    .await;
    let mut expected = [public_ids[1].clone(), public_ids[2].clone()];
    expected.sort();
    assert_eq!(ties["page"]["data"][0]["id"], expected[0]);
    assert_eq!(ties["page"]["data"][1]["id"], expected[1]);
    let literal = public_body(&f, &format!("{public_uri}?q=%25_"), None, StatusCode::OK).await;
    assert_eq!(literal["page"]["total"], 1);
    let empty = public_body(&f, &format!("{public_uri}?offset=99"), None, StatusCode::OK).await;
    assert_eq!(empty["page"]["total"], 4);
    assert_eq!(empty["page"]["data"], json!([]));
    assert_eq!(empty["page"]["has_more"], false);
    for query in [
        "limit=0",
        "offset=10001",
        "status=backlog",
        "sort=title",
        "igdb_id=42",
        "year=2020",
        "has_rating=true",
        "text=private",
        "state=paused",
        "undated=true",
        "unknown=x",
    ] {
        public_body(
            &f,
            &format!("{public_uri}?{query}"),
            None,
            StatusCode::BAD_REQUEST,
        )
        .await;
        f.value(
            "GET",
            &format!("/api/me/lists?{query}"),
            &owner,
            Value::Null,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    f.finish().await;
}

// Reconstruct the pre-summary schema without changing existing profile values.
async fn undo_statistics(pool: &PgPool) {
    undo_following(pool).await;
    sqlx::raw_sql(
        "ALTER TABLE profiles DROP COLUMN statistics_visibility;
        DELETE FROM _sqlx_migrations WHERE version=202609160003;",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn statistics_choice(f: &Fixture, owner: &str, visibility: &str) -> Value {
    let profile = owner_profile(f, owner).await;
    f.edit(
        "PUT",
        "/api/me/publication",
        owner,
        &tag(&profile),
        json!({"public_enabled":profile["public_enabled"],"statistics_visibility":visibility}),
        StatusCode::OK,
    )
    .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn statistics_define_runs_completions_replays_dates_and_live_corrections() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("stats-owner").await;
    let (_, other) = f.user("stats-other").await;
    list_games_fixture(&f).await;
    let empty = browse(&f, &owner, "/api/me/statistics").await;
    assert_eq!(
        empty["playthroughs"],
        json!({"played_playthroughs":0,"unique_games_played":0,
        "completed_playthroughs":0,"unique_completed_games":0,"replay_count":0,
        "replayed_games":0,"undated_playthroughs":0})
    );
    assert_eq!(empty["rating_distribution"].as_array().unwrap().len(), 10);
    for (index, bucket) in empty["rating_distribution"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(bucket, &json!({"rating":index+1,"count":0}));
    }
    let mut runs = Vec::new();
    for input in [
        json!({"igdb_id":42,"state":"completed","presentation_on":"2020-02-29","rating":8}),
        json!({"igdb_id":42,"state":"completed","presentation_on":"2020-12-31","rating":8}),
        json!({"igdb_id":43,"state":"abandoned","presentation_on":"2020-03-01","finished_on":"2020-03-01","rating":2}),
        json!({"igdb_id":44,"state":"completed","rating":10}),
        // An end date is evidence of play, never evidence of completion.
        json!({"igdb_id":43,"finished_on":"2019-01-01","presentation_on":"2019-01-01"}),
        // A review or rating by itself does not establish a run.
        json!({"igdb_id":44,"rating":6,"review":"Only an assessment","presentation_on":"2020-01-01"}),
    ] {
        runs.push(
            f.value(
                "POST",
                "/api/me/playthroughs",
                &owner,
                input,
                StatusCode::CREATED,
            )
            .await,
        );
    }
    f.value(
        "PUT",
        "/api/me/library/42",
        &owner,
        json!({"status":"backlog"}),
        StatusCode::OK,
    )
    .await;
    f.value(
        "PUT",
        "/api/me/library/43",
        &owner,
        json!({"status":"playing"}),
        StatusCode::OK,
    )
    .await;
    let list = new_list(&f, &owner, "Not activity").await;
    edit_list(&f, &owner, &list, "/games", json!({"igdb_ids":[44]})).await;
    let all = browse(&f, &owner, "/api/me/statistics").await;
    assert_eq!(
        all["playthroughs"],
        json!({"played_playthroughs":5,"unique_games_played":3,
        "completed_playthroughs":3,"unique_completed_games":2,"replay_count":2,
        "replayed_games":2,"undated_playthroughs":1})
    );
    assert_eq!(
        all["current_status_counts"],
        json!({"backlog":1,"playing":1,"completed":0,"on_hold":0,"dropped":0})
    );
    assert_eq!(all["rating_distribution"][7]["count"], 2);
    assert_eq!(all["rating_distribution"][9]["count"], 1);
    assert_eq!(browse(&f, &other, "/api/me/statistics").await, empty);
    let year = browse(&f, &owner, "/api/me/statistics?year=2020").await;
    assert_eq!(
        year["playthroughs"],
        json!({"played_playthroughs":3,"unique_games_played":2,
        "completed_playthroughs":2,"unique_completed_games":1,"replay_count":1,
        "replayed_games":1,"undated_playthroughs":0})
    );
    assert_eq!(year["current_status_counts"], all["current_status_counts"]);
    assert_eq!(year["rating_distribution"][5]["count"], 1);
    assert_eq!(year["rating_distribution"][9]["count"], 0);
    assert_eq!(
        year,
        browse(
            &f,
            &owner,
            "/api/me/statistics?from_on=2020-01-01&to_on=2020-12-31"
        )
        .await
    );
    let leap = browse(
        &f,
        &owner,
        "/api/me/statistics?from_on=2020-02-29&to_on=2020-02-29",
    )
    .await;
    assert_eq!(leap["playthroughs"]["completed_playthroughs"], 1);
    assert_eq!(leap["playthroughs"]["replay_count"], 0);
    let unknown = browse(&f, &owner, "/api/me/statistics?undated=true").await;
    assert_eq!(unknown["playthroughs"]["completed_playthroughs"], 1);
    assert_eq!(unknown["rating_distribution"][9]["count"], 1);
    assert_eq!(
        browse(&f, &owner, "/api/me/statistics?undated=false").await["playthroughs"]["played_playthroughs"],
        4
    );
    for query in [
        "year=0",
        "year=10000",
        "year=2020&from_on=2020-01-01",
        "undated=true&year=2020",
        "from_on=2021-01-01&to_on=2020-01-01",
        "from_on=2020-02-30",
        "undated=yes",
        "limit=20",
        "user_id=x",
        "year=2020&year=2021",
    ] {
        f.value(
            "GET",
            &format!("/api/me/statistics?{query}"),
            &owner,
            Value::Null,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    json_body(
        f.send("GET", "/api/me/statistics", None, &[], Value::Null)
            .await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    // Actual text editing has no date effect; chosen date, outcome and rating corrections do.
    let edited = f
        .edit(
            "PATCH",
            &run_path(&runs[0]),
            &owner,
            &tag(&runs[0]),
            json!({"review":"Corrected text"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        browse(&f, &owner, "/api/me/statistics?year=2020").await,
        year
    );
    f.edit(
        "PATCH",
        &run_path(&edited),
        &owner,
        &tag(&edited),
        json!({"presentation_on":"2018-01-01","state":"abandoned","rating":1}),
        StatusCode::OK,
    )
    .await;
    let corrected = browse(&f, &owner, "/api/me/statistics?year=2020").await;
    assert_eq!(corrected["playthroughs"]["completed_playthroughs"], 1);
    assert_eq!(corrected["playthroughs"]["replay_count"], 0);
    assert_eq!(corrected["rating_distribution"][7]["count"], 1);
    f.edit(
        "DELETE",
        &run_path(&runs[1]),
        &owner,
        &tag(&runs[1]),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        browse(&f, &owner, "/api/me/statistics?year=2020").await["playthroughs"]["completed_playthroughs"],
        0
    );
    f.value(
        "DELETE",
        "/api/me/library/42",
        &owner,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let removed = browse(&f, &owner, "/api/me/statistics").await;
    assert_eq!(removed["playthroughs"]["unique_games_played"], 3);
    assert_eq!(removed["current_status_counts"]["backlog"], 0);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn statistics_public_summary_uses_only_selected_components_and_explicit_permission() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("stats-public").await;
    let (_, other) = f.user("stats-visitor").await;
    list_games_fixture(&f).await;
    let profile = publication(&f, &owner, true).await;
    assert_eq!(profile["statistics_visibility"], "private");
    let uri = format!(
        "/api/public/profiles/{}/statistics",
        profile["public_id"].as_str().unwrap()
    );
    let preview = "/api/me/public-preview/statistics";
    for token in [None, Some(owner.as_str()), Some(other.as_str())] {
        public_body(&f, &uri, token, StatusCode::NOT_FOUND).await;
    }
    public_body(&f, preview, Some(&owner), StatusCode::NOT_FOUND).await;
    statistics_choice(&f, &owner, "public").await;
    let baseline = public_body(&f, &uri, None, StatusCode::OK).await;
    // Private runs of the same game cannot inflate public replays or ratings.
    let private = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"state":"completed","presentation_on":"2020-01-01","rating":10}),
            StatusCode::CREATED,
        )
        .await;
    let run = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"state":"completed","presentation_on":"2020-02-01","rating":2}),
            StatusCode::CREATED,
        )
        .await;
    let rating_only = f.value("POST","/api/me/playthroughs",&owner,
        json!({"igdb_id":43,"state":"completed","presentation_on":"2020-03-01","rating":7,"review":"Selected review"}),StatusCode::CREATED).await;
    let entry = f
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"playing"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(public_body(&f, &uri, None, StatusCode::OK).await, baseline);
    let run = share_run(&f,&owner,&run,json!({"summary_visibility":"public","rating_visibility":"private","review_visibility":"private"})).await;
    let rating_only = share_run(&f,&owner,&rating_only,json!({"summary_visibility":"private","rating_visibility":"public","review_visibility":"public"})).await;
    f.edit(
        "PUT",
        "/api/me/library/42/visibility",
        &owner,
        &tag(&entry),
        json!({"visibility":"public"}),
        StatusCode::OK,
    )
    .await;
    let shared = public_body(&f, &uri, None, StatusCode::OK).await;
    assert_eq!(
        shared["playthroughs"],
        json!({"played_playthroughs":1,"unique_games_played":1,
        "completed_playthroughs":1,"unique_completed_games":1,"replay_count":0,"replayed_games":0,"undated_playthroughs":0})
    );
    assert_eq!(shared["rating_distribution"][6]["count"], 1);
    assert_eq!(shared["rating_distribution"][1]["count"], 0);
    assert_eq!(shared["rating_distribution"][9]["count"], 0);
    assert_eq!(shared["current_status_counts"]["playing"], 1);
    for token in [Some(owner.as_str()), Some(other.as_str())] {
        assert_eq!(public_body(&f, &uri, token, StatusCode::OK).await, shared);
    }
    assert_eq!(
        public_body(&f, preview, Some(&owner), StatusCode::OK).await,
        shared
    );
    assert_eq!(
        public_body(&f, &format!("{uri}?year=2020"), None, StatusCode::OK).await,
        shared
    );
    assert_eq!(
        public_body(&f, &format!("{uri}?undated=true"), None, StatusCode::OK).await["playthroughs"]
            ["played_playthroughs"],
        0
    );
    for query in [
        "state=completed",
        "year=0",
        "year=2020&undated=true",
        "sort=date_added",
    ] {
        public_body(&f, &format!("{uri}?{query}"), None, StatusCode::BAD_REQUEST).await;
        public_body(
            &f,
            &format!("{preview}?{query}"),
            Some(&owner),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    // Private edits, dates, deletion and ratings have no public effect.
    let private = f
        .edit(
            "PATCH",
            &run_path(&private),
            &owner,
            &tag(&private),
            json!({"presentation_on":"2020-12-31","rating":1}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(public_body(&f, &uri, None, StatusCode::OK).await, shared);
    f.edit(
        "DELETE",
        &run_path(&private),
        &owner,
        &tag(&private),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(public_body(&f, &uri, None, StatusCode::OK).await, shared);
    // Disable/re-enable retains the summary choice; only the owner can preview.
    publication(&f, &owner, false).await;
    for token in [None, Some(owner.as_str()), Some(other.as_str())] {
        public_body(&f, &uri, token, StatusCode::NOT_FOUND).await;
    }
    assert_eq!(
        public_body(&f, preview, Some(&owner), StatusCode::OK).await,
        shared
    );
    public_body(&f, preview, Some(&other), StatusCode::NOT_FOUND).await;
    public_body(&f, preview, None, StatusCode::UNAUTHORIZED).await;
    publication(&f, &owner, true).await;
    // Summary revocation retains independently public content and owner totals.
    let own = browse(&f, &owner, "/api/me/statistics").await;
    statistics_choice(&f, &owner, "private").await;
    public_body(&f, &uri, None, StatusCode::NOT_FOUND).await;
    public_body(&f, preview, Some(&owner), StatusCode::NOT_FOUND).await;
    assert_eq!(browse(&f, &owner, "/api/me/statistics").await, own);
    public_body(
        &f,
        &format!("/api/public/playthroughs/{}", run["id"].as_str().unwrap()),
        None,
        StatusCode::OK,
    )
    .await;
    statistics_choice(&f, &owner, "public").await;
    share_run(&f,&owner,&run,json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"private"})).await;
    let revoked = public_body(&f, &uri, None, StatusCode::OK).await;
    assert_eq!(revoked["playthroughs"]["played_playthroughs"], 0);
    assert_eq!(revoked["rating_distribution"][6]["count"], 1);
    f.edit(
        "DELETE",
        &run_path(&rating_only),
        &owner,
        &tag(&rating_only),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        public_body(&f, &uri, None, StatusCode::OK).await["rating_distribution"],
        baseline["rating_distribution"]
    );
    let head = f.send("HEAD", &uri, None, &[], Value::Null).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["cache-control"], "no-store");
    assert!(!head.headers().contains_key("etag"));
    assert!(!head.headers().contains_key("last-modified"));
    public_body(
        &f,
        &format!("/api/public/profiles/{}/statistics", Uuid::new_v4()),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn statistics_migration_and_publication_preserve_data_and_reject_stale_choices() {
    let f = Fixture::new().await;
    let (_, owner) = f.user("stats-migration").await;
    let profile = publication(&f, &owner, true).await;
    let run = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"state":"completed","rating":9}),
            StatusCode::CREATED,
        )
        .await;
    undo_statistics(&f.database.app).await;
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    assert_eq!(owner_profile(&f, &owner).await, profile);
    assert_eq!(browse(&f, &owner, &run_path(&run)).await, run);
    let saved = statistics_choice(&f, &owner, "public").await;
    assert_eq!(
        saved["version"].as_i64().unwrap(),
        profile["version"].as_i64().unwrap() + 1
    );
    assert_eq!(statistics_choice(&f, &owner, "public").await, saved);
    assert_eq!(publication(&f, &owner, true).await, saved); // omission preserves
    let null = f
        .edit(
            "PUT",
            "/api/me/publication",
            &owner,
            &tag(&saved),
            json!({"public_enabled":true,"statistics_visibility":null}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(null, saved);
    f.edit(
        "PUT",
        "/api/me/publication",
        &owner,
        &tag(&profile),
        json!({"public_enabled":true,"statistics_visibility":"private"}),
        StatusCode::PRECONDITION_FAILED,
    )
    .await;
    f.edit(
        "PUT",
        "/api/me/publication",
        &owner,
        &tag(&saved),
        json!({"public_enabled":true,"statistics_visibility":"friends"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let tag = tag(&saved);
    let headers = [("If-Match", tag.as_str())];
    let (a, b) = tokio::join!(
        f.send(
            "PUT",
            "/api/me/publication",
            Some(&owner),
            &headers,
            json!({"public_enabled":true,"statistics_visibility":"private"})
        ),
        f.send(
            "PUT",
            "/api/me/publication",
            Some(&owner),
            &headers,
            json!({"public_enabled":false,"statistics_visibility":"public"})
        )
    );
    let mut codes = [a.status().as_u16(), b.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 412]);
    let uri = format!(
        "/api/public/profiles/{}/statistics",
        profile["public_id"].as_str().unwrap()
    );
    public_body(&f, &uri, None, StatusCode::NOT_FOUND).await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn statistics_snapshots_keep_metrics_consistent_during_concurrent_history_edits() {
    let f = Fixture::new().await;
    let (uid, owner) = f.user("stats-snapshot").await;
    let profile = publication(&f, &owner, true).await;
    statistics_choice(&f, &owner, "public").await;
    let run = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &owner,
            json!({"igdb_id":42,"state":"completed","rating":8,"presentation_on":"2020-01-01"}),
            StatusCode::CREATED,
        )
        .await;
    share_run(&f, &owner, &run, json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"private"})).await;
    let entry = f
        .value(
            "PUT",
            "/api/me/library/42",
            &owner,
            json!({"status":"completed"}),
            StatusCode::OK,
        )
        .await;
    f.edit(
        "PUT",
        "/api/me/library/42/visibility",
        &owner,
        &tag(&entry),
        json!({"visibility":"public"}),
        StatusCode::OK,
    )
    .await;
    let uri = format!(
        "/api/public/profiles/{}/statistics",
        profile["public_id"].as_str().unwrap()
    );
    // Atomically alternate completion/rating/status/date. A response must see
    // one committed combination even though its component queries are separate.
    let ((), ()) = tokio::join!(
        async {
            for n in 0..40 {
                let mut tx = f.database.app.begin().await.unwrap();
                let completed = n % 2 == 1;
                sqlx::query("UPDATE playthroughs SET state=$2,rating=$3,presentation_on=$4::text::date WHERE user_id=$1")
                    .bind(uid.0.as_bytes().as_slice())
                    .bind(if completed {"completed"} else {"abandoned"})
                    .bind(if completed {8_i16} else {2_i16})
                    .bind(if completed {"2020-01-01"} else {"2019-01-01"})
                    .execute(&mut *tx).await.unwrap();
                sqlx::query("UPDATE library_entries SET status=$2 WHERE user_id=$1")
                    .bind(uid.0.as_bytes().as_slice())
                    .bind(if completed { "completed" } else { "dropped" })
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                tokio::task::yield_now().await;
            }
        },
        async {
            for _ in 0..20 {
                let public = public_body(&f, &uri, None, StatusCode::OK).await;
                let personal = browse(&f, &owner, "/api/me/statistics").await;
                let filtered =
                    public_body(&f, &format!("{uri}?year=2020"), None, StatusCode::OK).await;
                for value in [&public, &personal, &filtered] {
                    assert_eq!(
                        value["playthroughs"]["completed_playthroughs"],
                        value["rating_distribution"][7]["count"]
                    );
                    assert_eq!(
                        value["playthroughs"]["completed_playthroughs"],
                        value["current_status_counts"]["completed"]
                    );
                    assert_eq!(value["playthroughs"]["replay_count"], 0);
                }
                assert_eq!(
                    filtered["playthroughs"]["played_playthroughs"],
                    filtered["playthroughs"]["completed_playthroughs"]
                );
                for value in [&public, &personal] {
                    assert_eq!(value["playthroughs"]["played_playthroughs"], 1);
                    assert_eq!(
                        value["rating_distribution"][1]["count"],
                        value["current_status_counts"]["dropped"]
                    );
                }
            }
        }
    );
    f.finish().await;
}

async fn undo_following(pool: &PgPool) {
    undo_moderation(pool).await;
    sqlx::raw_sql("DROP TABLE follows; DELETE FROM _sqlx_migrations WHERE version=202609170001;")
        .execute(pool)
        .await
        .unwrap();
}

fn following_path(profile: &Value) -> String {
    format!(
        "/api/me/following/{}",
        profile["public_id"].as_str().unwrap()
    )
}

async fn follow_request(f: &Fixture, token: &str, profile: &Value, method: &str) {
    let response = f
        .call(method, &following_path(profile), Some(token), Value::Null)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(!response.headers().contains_key("etag"));
    assert!(!response.headers().contains_key("last-modified"));
}

async fn feed_body(f: &Fixture, token: &str) -> Value {
    public_body(f, "/api/me/feed", Some(token), StatusCode::OK).await
}

async fn feed_run(f: &Fixture, owner: &str, date: Option<&str>, state: Option<&str>) -> Value {
    f.value("POST", "/api/me/playthroughs", owner,
        json!({"igdb_id":42,"presentation_on":date,"state":state,"review":"Chosen review","rating":9,
            "platform":"Private platform","started_on":"2010-01-01"}), StatusCode::CREATED).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn following_is_private_idempotent_and_cascades_after_migration() {
    let f = Fixture::new().await;
    let (reader_id, reader) = f.user("follow-reader").await;
    let (author_id, author) = f.user("follow-author").await;
    let (_, other) = f.user("follow-other").await;
    let profile = publication(&f, &author, true).await;
    let other_profile = publication(&f, &other, true).await;
    // Migration changes no profiles or publication choices and creates no follows.
    undo_following(&f.database.app).await;
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    assert_eq!(browse(&f, &author, "/api/me").await["profile"], profile);
    assert_eq!(
        browse(&f, &reader, "/api/me/following").await["data"],
        json!([])
    );
    for (method, uri) in [
        ("GET", "/api/me/following"),
        ("GET", "/api/me/feed"),
        ("PUT", following_path(&profile).as_str()),
        ("DELETE", following_path(&profile).as_str()),
    ] {
        assert_eq!(
            f.call(method, uri, None, Value::Null).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        f.call("PUT", &following_path(&profile), Some(&author), Value::Null)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    // An unpublished follower can follow; repeated and concurrent PUTs add one edge.
    let ((), ()) = tokio::join!(
        follow_request(&f, &reader, &profile, "PUT"),
        follow_request(&f, &reader, &profile, "PUT")
    );
    let followed = public_body(&f, "/api/me/following", Some(&reader), StatusCode::OK).await;
    assert_eq!(followed["data"].as_array().unwrap().len(), 1);
    assert_eq!(followed["data"][0]["public_id"], profile["public_id"]);
    assert_eq!(
        browse(&f, &author, "/api/me/following").await["data"],
        json!([])
    );
    assert_eq!(
        browse(&f, &other, "/api/me/following").await["data"],
        json!([])
    );
    for suffix in ["followers", "following"] {
        public_body(
            &f,
            &format!(
                "/api/public/profiles/{}/{suffix}",
                profile["public_id"].as_str().unwrap()
            ),
            None,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    follow_request(&f, &reader, &other_profile, "PUT").await;
    let page = browse(&f, &reader, "/api/me/following?limit=1").await;
    let next = browse(
        &f,
        &reader,
        &format!(
            "/api/me/following?limit=1&after={}",
            page["next_cursor"].as_str().unwrap()
        ),
    )
    .await;
    assert_ne!(page["data"][0]["public_id"], next["data"][0]["public_id"]);
    assert_eq!(next["has_more"], false);
    // Disabled sources vanish from lists and cannot be newly followed. Existing
    // edges survive toggling, but DELETE works even while the target is hidden.
    publication(&f, &author, false).await;
    assert_eq!(
        browse(&f, &reader, "/api/me/following").await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.call("PUT", &following_path(&profile), Some(&reader), Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    follow_request(&f, &reader, &profile, "DELETE").await;
    follow_request(&f, &reader, &profile, "DELETE").await;
    publication(&f, &author, true).await;
    assert_eq!(
        browse(&f, &reader, "/api/me/following").await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // All operations lock the same target, including when no edge yet exists.
    for _ in 0..8 {
        let ((), ()) = timeout(Duration::from_secs(10), async {
            tokio::join!(
                follow_request(&f, &reader, &profile, "PUT"),
                follow_request(&f, &reader, &profile, "DELETE")
            )
        })
        .await
        .unwrap();
        follow_request(&f, &reader, &profile, "PUT").await;
    }
    // Publication and follow requests cannot deadlock or return storage failures.
    let hidden = publication(&f, &author, false).await;
    let target_path = following_path(&profile);
    let hidden_tag = tag(&hidden);
    let headers = [("If-Match", hidden_tag.as_str())];
    let (follow, publish) = tokio::join!(
        f.call("PUT", &target_path, Some(&reader), Value::Null),
        f.send(
            "PUT",
            "/api/me/publication",
            Some(&author),
            &headers,
            json!({"public_enabled":true})
        )
    );
    assert!(matches!(follow.status().as_u16(), 204 | 404));
    assert_eq!(publish.status(), StatusCode::OK);
    f.auth
        .state()
        .operator()
        .delete_account(
            author_id,
            OperatorActor::new("follow-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    follow_request(&f, &reader, &profile, "DELETE").await;
    assert_eq!(
        f.call("PUT", &following_path(&profile), Some(&reader), Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let targets: i64 = sqlx::query_scalar("SELECT count(*) FROM follows WHERE followed_id=$1")
        .bind(author_id.0.as_bytes().as_slice())
        .fetch_one(&f.database.app)
        .await
        .unwrap();
    assert_eq!(targets, 0);
    f.auth
        .state()
        .operator()
        .delete_account(
            reader_id,
            OperatorActor::new("follow-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM follows")
        .fetch_one(&f.database.app)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn feed_projects_independent_components_and_revokes_every_source_live() {
    let f = Fixture::new().await;
    let (_, reader) = f.user("feed-reader").await;
    let (author_id, author) = f.user("feed-author").await;
    let (_, other) = f.user("feed-other").await;
    list_games_fixture(&f).await;
    let profile = publication(&f, &author, true).await;
    publication(&f, &other, true).await;
    let run = feed_run(&f, &author, Some("2020-02-02"), Some("completed")).await;
    let run = share_run(&f, &author, &run, json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"})).await;
    let note = f
        .note(&author, &run, "Shared note", Some("2020-02-01"))
        .await;
    let note = f
        .edit(
            "PUT",
            &format!("{}/visibility", note_path(&note)),
            &author,
            &tag(&note),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    let other_run = feed_run(&f, &other, Some("2030-01-01"), Some("active")).await;
    share_run(&f, &other, &other_run, json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await;
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    follow_request(&f, &reader, &profile, "PUT").await;
    let selected = feed_body(&f, &reader).await;
    assert_eq!(selected["data"].as_array().unwrap().len(), 2);
    assert_eq!(selected["data"][0]["kind"], "review");
    assert_eq!(
        selected["data"][0]["item"],
        public_body(
            &f,
            &format!("/api/public/reviews/{}", run["id"].as_str().unwrap()),
            None,
            StatusCode::OK
        )
        .await
    );
    assert_eq!(selected["data"][1]["kind"], "note");
    assert_eq!(
        selected["data"][1]["item"],
        public_body(
            &f,
            &format!("/api/public/notes/{}", note["id"].as_str().unwrap()),
            None,
            StatusCode::OK
        )
        .await
    );
    // Private state/platform/rating and parent identity cannot leak via reviews or notes.
    for row in selected["data"].as_array().unwrap() {
        for key in [
            "platform",
            "state",
            "rating",
            "started_on",
            "finished_on",
            "playthrough_id",
        ] {
            assert!(row["item"].get(key).is_none(), "leaked {key}");
        }
    }
    assert_eq!(feed_body(&f, &other).await["data"], json!([]));
    let run = share_run(&f, &author, &run, json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await;
    let all = feed_body(&f, &reader).await;
    assert_eq!(all["data"].as_array().unwrap().len(), 3);
    assert_eq!(all["data"][0]["kind"], "milestone");
    assert!(all["data"][0]["item"].get("review").is_none());
    assert!(all["data"][0]["item"].get("rating").is_none());
    assert_eq!(
        all["data"][0]["item"],
        public_body(
            &f,
            &format!("/api/public/playthroughs/{}", run["id"].as_str().unwrap()),
            None,
            StatusCode::OK
        )
        .await
    );
    // Shared summaries lacking an explicit state and rating-only shares are not events.
    let assessment = f
        .value(
            "POST",
            "/api/me/playthroughs",
            &author,
            json!({"igdb_id":42,"rating":1,"presentation_on":"2040-01-01"}),
            StatusCode::CREATED,
        )
        .await;
    share_run(&f, &author, &assessment, json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"private"})).await;
    let private = feed_run(&f, &author, Some("2050-01-01"), Some("completed")).await;
    assert_eq!(feed_body(&f, &reader).await, all);
    publication(&f, &author, false).await;
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    publication(&f, &author, true).await;
    assert_eq!(feed_body(&f, &reader).await, all);
    let run = share_run(&f, &author, &run, json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"private"})).await;
    assert_eq!(
        feed_body(&f, &reader).await["data"],
        json!([selected["data"][1]])
    );
    let note = f
        .edit(
            "PUT",
            &format!("{}/visibility", note_path(&note)),
            &author,
            &tag(&note),
            json!({"visibility":"private"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    f.edit(
        "PUT",
        &format!("{}/visibility", note_path(&note)),
        &author,
        &tag(&note),
        json!({"visibility":"public"}),
        StatusCode::OK,
    )
    .await;
    follow_request(&f, &reader, &profile, "DELETE").await;
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    follow_request(&f, &reader, &profile, "PUT").await;
    assert_eq!(
        feed_body(&f, &reader).await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.call("DELETE", &run_path(&run), Some(&author), Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    share_run(&f, &author, &private, json!({"summary_visibility":"public","rating_visibility":"private","review_visibility":"public"})).await;
    assert_eq!(
        feed_body(&f, &reader).await["data"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    f.auth
        .state()
        .operator()
        .delete_account(
            author_id,
            OperatorActor::new("feed-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    // All context was local; no IGDB request was made by these operations.
    assert!(f.server.received_requests().await.unwrap().is_empty());
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn feed_cursor_orders_chosen_dates_components_unknowns_and_backfills() {
    let f = Fixture::new().await;
    let (_, reader) = f.user("feed-pages-reader").await;
    let (_, author) = f.user("feed-pages-author").await;
    list_games_fixture(&f).await;
    let profile = publication(&f, &author, true).await;
    follow_request(&f, &reader, &profile, "PUT").await;
    let mut runs = Vec::new();
    for date in [
        Some("2020-01-01"),
        Some("2020-01-01"),
        Some("2019-01-01"),
        None,
        None,
    ] {
        let run = feed_run(&f, &author, date, Some("completed")).await;
        runs.push(share_run(&f, &author, &run, json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await);
    }
    let baseline = feed_body(&f, &reader).await;
    assert_eq!(baseline["data"].as_array().unwrap().len(), 10);
    let mut seen = Vec::new();
    let mut uri = "/api/me/feed?limit=1".to_string();
    let mut cursors = Vec::new();
    loop {
        let page = public_body(&f, &uri, Some(&reader), StatusCode::OK).await;
        seen.extend(page["data"].as_array().unwrap().iter().cloned());
        if page["has_more"] == false {
            assert!(page["next_cursor"].is_null());
            break;
        }
        let cursor = page["next_cursor"].as_str().unwrap().to_string();
        cursors.push(cursor.clone());
        uri = format!("/api/me/feed?limit=1&after={cursor}");
        assert!(seen.len() <= 10);
    }
    assert_eq!(seen, *baseline["data"].as_array().unwrap());
    for pair in seen.chunks(2) {
        assert_eq!(pair[0]["kind"], "milestone");
        assert_eq!(pair[1]["kind"], "review");
        assert_eq!(pair[0]["item"]["id"], pair[1]["item"]["id"]);
    }
    assert!(seen[6]["item"]["presentation_on"].is_null());
    // Editing text keeps exactly the same position and cursors, with live text.
    let edited = f
        .edit(
            "PATCH",
            &run_path(&runs[2]),
            &author,
            &tag(&runs[2]),
            json!({"review":"Revised old text"}),
            StatusCode::OK,
        )
        .await;
    let revised = feed_body(&f, &reader).await;
    assert_eq!(revised["data"][5]["item"]["review"], "Revised old text");
    assert_eq!(revised["data"][5]["item"]["presentation_on"], "2019-01-01");
    // Publishing a backfill today inserts at its chosen old day, without an event date.
    let backfill = feed_run(&f, &author, Some("2018-01-01"), Some("paused")).await;
    share_run(&f, &author, &backfill, json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"})).await;
    let continued = browse(&f, &reader, &format!("/api/me/feed?after={}", cursors[3])).await;
    assert_eq!(continued["data"][2]["item"]["id"], backfill["id"]);
    // A newly inserted newer item cannot shift continuation; removing the cursor
    // anchor also cannot invalidate the value boundary.
    let newer = feed_run(&f, &author, Some("2025-01-01"), Some("active")).await;
    share_run(&f, &author, &newer, json!({"summary_visibility":"public","rating_visibility":"private","review_visibility":"public"})).await;
    let anchor_id = baseline["data"][3]["item"]["id"].as_str().unwrap();
    assert_eq!(
        f.call(
            "DELETE",
            &format!("/api/me/playthroughs/{anchor_id}"),
            Some(&author),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        browse(&f, &reader, &format!("/api/me/feed?after={}", cursors[3])).await,
        continued
    );
    // Changing the chosen date deliberately changes its position.
    f.edit(
        "PATCH",
        &run_path(&edited),
        &author,
        &tag(&edited),
        json!({"presentation_on":"2026-01-01"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        feed_body(&f, &reader).await["data"][0]["item"]["id"],
        edited["id"]
    );
    for query in [
        "limit=0",
        "limit=51",
        "limit=-1",
        "offset=1",
        "after=garbage",
        "year=2020",
        "after=f1:unknown:00000000-0000-0000-0000-000000000000:rating",
    ] {
        public_body(
            &f,
            &format!("/api/me/feed?{query}"),
            Some(&reader),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for query in [
        "limit=0",
        "limit=51",
        "after=bad",
        "user_id=someone",
        "offset=1",
    ] {
        public_body(
            &f,
            &format!("/api/me/following?{query}"),
            Some(&reader),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for uri in ["/api/me/feed", "/api/me/following"] {
        let response = f.call("HEAD", uri, Some(&reader), Value::Null).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.headers().contains_key("etag"));
        assert!(!response.headers().contains_key("last-modified"));
    }
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn feed_snapshot_never_mixes_following_visibility_and_component_edits() {
    let f = Fixture::new().await;
    let (reader_id, reader) = f.user("feed-snapshot-reader").await;
    let (author_id, author) = f.user("feed-snapshot-author").await;
    list_games_fixture(&f).await;
    let profile = publication(&f, &author, true).await;
    follow_request(&f, &reader, &profile, "PUT").await;
    let run = feed_run(&f, &author, Some("2020-01-01"), Some("completed")).await;
    share_run(&f, &author, &run, json!({"summary_visibility":"public","rating_visibility":"private","review_visibility":"public"})).await;
    let ((), ()) = tokio::join!(
        async {
            for n in 0..40 {
                let mut tx = f.database.app.begin().await.unwrap();
                let visible = n % 2 == 1;
                sqlx::query("UPDATE profiles SET public_enabled=$2 WHERE user_id=$1")
                    .bind(author_id.0.as_bytes().as_slice())
                    .bind(visible)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::query("UPDATE playthroughs SET review=$2,summary_visibility=$3,review_visibility=$3 WHERE user_id=$1")
                .bind(author_id.0.as_bytes().as_slice()).bind(if visible {"Chosen review"} else {"Never visible"})
                .bind(if visible {"public"} else {"private"}).execute(&mut *tx).await.unwrap();
                if visible {
                    sqlx::query("INSERT INTO follows(follower_id,followed_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
                    .bind(reader_id.0.as_bytes().as_slice()).bind(author_id.0.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
                } else {
                    sqlx::query("DELETE FROM follows WHERE follower_id=$1")
                        .bind(reader_id.0.as_bytes().as_slice())
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                }
                tx.commit().await.unwrap();
                tokio::task::yield_now().await;
            }
        },
        async {
            for _ in 0..30 {
                let feed = feed_body(&f, &reader).await;
                let rows = feed["data"].as_array().unwrap();
                assert!(rows.is_empty() || rows.len() == 2);
                if rows.len() == 2 {
                    assert_eq!(rows[0]["kind"], "milestone");
                    assert_eq!(rows[1]["item"]["review"], "Chosen review");
                    assert_eq!(rows[0]["item"]["id"], rows[1]["item"]["id"]);
                }
            }
        }
    );
    f.finish().await;
}

async fn undo_moderation(pool: &PgPool) {
    let profile_sql = include_str!("../migrations/202609160001_public_profiles.sql");
    let views = profile_sql
        .split("CREATE VIEW selected_reviews")
        .nth(1)
        .unwrap();
    let views = format!("CREATE VIEW selected_reviews{views}")
        .replace("CREATE VIEW", "CREATE OR REPLACE VIEW");
    sqlx::raw_sql(sqlx::AssertSqlSafe(views))
        .execute(pool)
        .await
        .unwrap();
    let list_sql = include_str!("../migrations/202609160002_game_lists.sql");
    let views = list_sql.split("CREATE VIEW selected_lists").nth(1).unwrap();
    let views = format!("CREATE VIEW selected_lists{views}")
        .replace("CREATE VIEW", "CREATE OR REPLACE VIEW");
    sqlx::raw_sql(sqlx::AssertSqlSafe(views))
        .execute(pool)
        .await
        .unwrap();
    sqlx::raw_sql("DROP TRIGGER gameboxd_moderation_delete ON gameboxd_auth.users;
        DROP TRIGGER moderation_publication ON profiles;
        DROP TRIGGER moderation_publication ON playthroughs;
        DROP TRIGGER moderation_publication ON diary_notes;
        DROP TRIGGER moderation_publication ON library_entries;
        DROP TRIGGER moderation_publication ON game_lists;
        DROP FUNCTION moderation_delete_evidence(),moderation_publication_guard(),moderation_hidden(text,uuid),social_allowed(bytea,bytea),public_view_allowed(bytea,bytea);
        DROP TABLE account_blocks,moderation_operators,moderation_restrictions,moderation_reports,moderation_audit;
        DELETE FROM _sqlx_migrations WHERE version=202609170002;")
        .execute(pool).await.unwrap();
}

async fn safety_operator(f: &Fixture) -> (UserId, String) {
    let (id, token) = f.user("safety-operator").await;
    sqlx::query("INSERT INTO moderation_operators(user_id) VALUES($1)")
        .bind(id.0.as_bytes().as_slice())
        .execute(&f.database.app)
        .await
        .unwrap();
    (id, token)
}
async fn restriction(
    f: &Fixture,
    operator: &str,
    kind: &str,
    id: &Value,
    active: bool,
    version: i32,
) -> Value {
    f.value(
        "PUT",
        &format!("/api/operator/restrictions/{kind}/{}", id.as_str().unwrap()),
        operator,
        json!({"active":active,"expected_version":version,"reason":"Reviewed public content"}),
        StatusCode::OK,
    )
    .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn safety_blocks_remove_both_follows_hide_authenticated_reads_and_serialize_races() {
    let f = Fixture::new().await;
    let (_, reader) = f.user("block-reader").await;
    let (_, author) = f.user("block-author").await;
    let (_, other) = f.user("block-other").await;
    list_games_fixture(&f).await;
    let reader_profile = publication(&f, &reader, true).await;
    let author_profile = publication(&f, &author, true).await;
    statistics_choice(&f, &author, "public").await;
    let run = feed_run(&f, &author, Some("2001-01-01"), Some("completed")).await;
    share_run(&f,&author,&run,json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await;
    let note = f.note(&author, &run, "Blockable note", None).await;
    let note = share_note(&f, &author, &note).await;
    let list = new_list(&f, &author, "Blockable list").await;
    let list = edit_list(
        &f,
        &author,
        &list,
        "/visibility",
        json!({"visibility":"public"}),
    )
    .await;
    follow_request(&f, &reader, &author_profile, "PUT").await;
    follow_request(&f, &author, &reader_profile, "PUT").await;
    let block_url = format!(
        "/api/me/blocks/{}",
        author_profile["public_id"].as_str().unwrap()
    );
    assert_eq!(
        f.call("PUT", &block_url, None, Value::Null).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for _ in 0..2 {
        assert_eq!(
            f.call("PUT", &block_url, Some(&reader), Value::Null)
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    assert_eq!(
        browse(&f, &reader, "/api/me/following").await["data"],
        json!([])
    );
    assert_eq!(
        browse(&f, &author, "/api/me/following").await["data"],
        json!([])
    );
    assert_eq!(
        browse(&f, &reader, "/api/me/blocks").await,
        json!([{"public_id":author_profile["public_id"]}])
    );
    assert_eq!(browse(&f, &author, "/api/me/blocks").await, json!([]));
    for (token, target) in [(&reader, &author_profile), (&author, &reader_profile)] {
        assert_eq!(
            f.call("PUT", &following_path(target), Some(token), Value::Null)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let id = author_profile["public_id"].as_str().unwrap();
    let urls = vec![
        format!("/api/public/profiles/{id}"),
        format!("/api/public/profiles/{id}/reviews"),
        format!("/api/public/profiles/{id}/statistics"),
        format!("/api/public/reviews/{}", run["id"].as_str().unwrap()),
        format!("/api/public/ratings/{}", run["id"].as_str().unwrap()),
        format!("/api/public/playthroughs/{}", run["id"].as_str().unwrap()),
        format!("/api/public/notes/{}", note["id"].as_str().unwrap()),
        format!("/api/public/lists/{}", list["list"]["id"].as_str().unwrap()),
    ];
    for url in &urls {
        public_body(&f, url, Some(&reader), StatusCode::NOT_FOUND).await;
        public_body(&f, url, None, StatusCode::OK).await;
        public_body(&f, url, Some(&other), StatusCode::OK).await;
    }
    public_body(&f, "/api/me/public-preview", Some(&author), StatusCode::OK).await;
    assert_eq!(
        f.call("DELETE", &block_url, Some(&reader), Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(feed_body(&f, &reader).await["data"], json!([])); // Unblock never restores follows.
    for _ in 0..8 {
        let follow_url = following_path(&author_profile);
        let reverse = following_path(&reader_profile);
        let (a, b, c) = tokio::join!(
            f.call("PUT", &follow_url, Some(&reader), Value::Null),
            f.call("PUT", &reverse, Some(&author), Value::Null),
            f.call("PUT", &block_url, Some(&reader), Value::Null)
        );
        assert!([StatusCode::NO_CONTENT, StatusCode::NOT_FOUND].contains(&a.status()));
        assert!([StatusCode::NO_CONTENT, StatusCode::NOT_FOUND].contains(&b.status()));
        assert_eq!(c.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM follows")
                .fetch_one(&f.database.app)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            f.call("DELETE", &block_url, Some(&reader), Value::Null)
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
    }
    // Reciprocal blocks are independent, including while a profile is disabled.
    let reverse_block = format!(
        "/api/me/blocks/{}",
        reader_profile["public_id"].as_str().unwrap()
    );
    f.call("PUT", &reverse_block, Some(&author), Value::Null)
        .await;
    publication(&f, &author, false).await;
    assert_eq!(
        f.call("PUT", &block_url, Some(&reader), Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    f.call("DELETE", &block_url, Some(&reader), Value::Null)
        .await;
    publication(&f, &author, true).await;
    assert_eq!(
        f.call(
            "PUT",
            &following_path(&author_profile),
            Some(&reader),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn safety_moderation_gates_all_surfaces_preserves_diaries_and_checks_versions() {
    let f = Fixture::new().await;
    let (_, operator) = safety_operator(&f).await;
    let (_, author) = f.user("moderated-author").await;
    let (_, reader) = f.user("moderated-reader").await;
    list_games_fixture(&f).await;
    publication(&f, &author, true).await;
    let profile = statistics_choice(&f, &author, "public").await;
    let run = feed_run(&f, &author, Some("2000-01-01"), Some("completed")).await;
    let run=share_run(&f,&author,&run,json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"})).await;
    let note = f
        .note(&author, &run, "Selected note", Some("2000-01-01"))
        .await;
    let note = share_note(&f, &author, &note).await;
    let list = new_list(&f, &author, "Selected list").await;
    let list = edit_list(
        &f,
        &author,
        &list,
        "/visibility",
        json!({"visibility":"public"}),
    )
    .await;
    let entry = f
        .value(
            "PUT",
            "/api/me/library/42",
            &author,
            json!({"status":"completed"}),
            StatusCode::OK,
        )
        .await;
    let entry = f
        .edit(
            "PUT",
            "/api/me/library/42/visibility",
            &author,
            &tag(&entry),
            json!({"visibility":"public"}),
            StatusCode::OK,
        )
        .await;
    follow_request(&f, &reader, &profile, "PUT").await;
    let public_profile = format!(
        "/api/public/profiles/{}",
        profile["public_id"].as_str().unwrap()
    );
    let before = public_body(&f, &public_profile, None, StatusCode::OK).await;
    let run_url = format!("/api/public/reviews/{}", run["id"].as_str().unwrap());
    let baseline = feed_body(&f, &reader).await;
    for (kind, id) in [
        ("review", &run["id"]),
        ("note", &note["id"]),
        ("list", &list["list"]["id"]),
    ] {
        restriction(&f, &operator, kind, id, true, 0).await;
    }
    assert_eq!(
        feed_body(&f, &reader).await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for (kind, id) in [
        ("reviews", &run["id"]),
        ("notes", &note["id"]),
        ("lists", &list["list"]["id"]),
    ] {
        let url = format!("/api/public/{kind}/{}", id.as_str().unwrap());
        for viewer in [None, Some(author.as_str()), Some(reader.as_str())] {
            public_body(&f, &url, viewer, StatusCode::NOT_FOUND).await;
        }
        let section = public_body(
            &f,
            &format!("{public_profile}/{kind}"),
            None,
            StatusCode::OK,
        )
        .await;
        assert_eq!(section["page"]["total"], 0);
        assert_eq!(
            public_body(
                &f,
                &format!("/api/me/public-preview/{kind}"),
                Some(&author),
                StatusCode::OK
            )
            .await,
            section
        );
    }
    let counts = public_body(&f, &public_profile, None, StatusCode::OK).await;
    assert_eq!(counts["counts"]["reviews"], 0);
    assert_eq!(counts["counts"]["ratings"], 1); // Independent rating and summary survive a review hide.
    public_body(
        &f,
        &format!("/api/public/ratings/{}", run["id"].as_str().unwrap()),
        None,
        StatusCode::OK,
    )
    .await;
    let public_stats = public_body(
        &f,
        &format!("{public_profile}/statistics"),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(public_stats["playthroughs"]["completed_playthroughs"], 1);
    assert_eq!(
        browse(&f, &author, "/api/me/restrictions")
            .await
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(browse(&f, &reader, "/api/me/restrictions").await, json!([]));
    let denied=f.edit("PUT",&format!("{}/visibility",run_path(&run)),&author,&tag(&run),json!({"summary_visibility":"public","rating_visibility":"public","review_visibility":"public"}),StatusCode::FORBIDDEN).await;
    assert_eq!(denied["error"]["code"], "moderation_restricted");
    let revised = f
        .edit(
            "PATCH",
            &run_path(&run),
            &author,
            &tag(&run),
            json!({"review":"Privately corrected","presentation_on":"1999-01-01"}),
            StatusCode::OK,
        )
        .await;
    public_body(&f, &run_url, None, StatusCode::NOT_FOUND).await;
    assert_eq!(revised["review_visibility"], "public");
    for (kind, id) in [
        ("review", &run["id"]),
        ("note", &note["id"]),
        ("list", &list["list"]["id"]),
    ] {
        restriction(&f, &operator, kind, id, false, 1).await;
    }
    assert_eq!(
        public_body(&f, &run_url, None, StatusCode::OK).await["review"],
        "Privately corrected"
    );
    assert_eq!(
        feed_body(&f, &reader).await["data"]
            .as_array()
            .unwrap()
            .len(),
        baseline["data"].as_array().unwrap().len()
    );
    // Account restrictions gate all components, summaries, profile lookup and preview.
    restriction(&f, &operator, "profile", &profile["public_id"], true, 0).await;
    for url in [
        &public_profile,
        &format!("{public_profile}/statistics"),
        &run_url,
        &format!(
            "/api/public/library/{}",
            entry["public_id"].as_str().unwrap()
        ),
        &format!("/api/public/playthroughs/{}", run["id"].as_str().unwrap()),
        &format!("/api/public/ratings/{}", run["id"].as_str().unwrap()),
        &format!("/api/public/notes/{}", note["id"].as_str().unwrap()),
        &format!("/api/public/lists/{}", list["list"]["id"].as_str().unwrap()),
    ] {
        public_body(&f, url, None, StatusCode::NOT_FOUND).await;
    }
    public_body(
        &f,
        "/api/me/public-preview",
        Some(&author),
        StatusCode::NOT_FOUND,
    )
    .await;
    public_body(
        &f,
        "/api/me/public-preview/statistics",
        Some(&author),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(feed_body(&f, &reader).await["data"], json!([]));
    assert_eq!(
        browse(&f, &reader, "/api/me/following").await["data"],
        json!([])
    );
    assert_eq!(
        browse(&f, &author, &run_path(&run)).await["review"],
        "Privately corrected"
    );
    assert_eq!(
        browse(&f, &author, "/api/me/statistics").await["playthroughs"]["completed_playthroughs"],
        1
    );
    let denied = f
        .edit(
            "PUT",
            "/api/me/publication",
            &author,
            &tag(&profile),
            json!({"public_enabled":true}),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(denied["error"]["code"], "moderation_restricted");
    let route = format!(
        "/api/operator/restrictions/profile/{}",
        profile["public_id"].as_str().unwrap()
    );
    let payload = json!({"active":false,"expected_version":1,"reason":"Appeal accepted"});
    let (a, b) = tokio::join!(
        f.call("PUT", &route, Some(&operator), payload.clone()),
        f.call("PUT", &route, Some(&operator), payload)
    );
    assert!(
        (a.status() == StatusCode::OK && b.status() == StatusCode::PRECONDITION_FAILED)
            || (b.status() == StatusCode::OK && a.status() == StatusCode::PRECONDITION_FAILED)
    );
    assert_eq!(
        public_body(&f, &public_profile, None, StatusCode::OK).await,
        before
    );
    assert_eq!(
        browse(&f, &operator, "/api/operator/audit")
            .await
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(browse(&f, &author, "/api/me/restrictions").await, json!([]));
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn safety_reports_are_private_bounded_deduplicated_and_preserve_only_public_evidence() {
    let f = Fixture::new().await;
    let (operator_id, operator) = safety_operator(&f).await;
    let (_, author) = f.user("reported-author").await;
    let (_, reporter) = f.user("reporter").await;
    let (_, other) = f.user("another-reporter").await;
    list_games_fixture(&f).await;
    let profile = publication(&f, &author, true).await;
    let run = feed_run(&f, &author, Some("1999-01-01"), Some("completed")).await;
    let run=share_run(&f,&author,&run,json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"})).await;
    let private_run = feed_run(&f, &author, None, None).await;
    let note = f.note(&author, &run, "Reportable note", None).await;
    let note = share_note(&f, &author, &note).await;
    let list = new_list(&f, &author, "Reportable list").await;
    let list = edit_list(&f, &author, &list, "/games", json!({"igdb_ids":[43,42]})).await;
    let list = edit_list(
        &f,
        &author,
        &list,
        "/visibility",
        json!({"visibility":"public"}),
    )
    .await;
    let input = json!({"kind":"review","target_id":run["id"],"reason":"harassment","details":"Please inspect this public review"});
    assert_eq!(
        f.call("POST", "/api/me/reports", None, input.clone())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (a, b) = tokio::join!(
        f.call("POST", "/api/me/reports", Some(&reporter), input.clone()),
        f.call("POST", "/api/me/reports", Some(&reporter), input.clone())
    );
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(b.status(), StatusCode::OK);
    let receipt = json_body(a, StatusCode::OK).await;
    assert_eq!(json_body(b, StatusCode::OK).await, receipt);
    assert_eq!(receipt.as_object().unwrap().len(), 1);
    let mut receipts = vec![receipt.clone()];
    for (kind, id) in [
        ("profile", &profile["public_id"]),
        ("note", &note["id"]),
        ("list", &list["list"]["id"]),
    ] {
        receipts.push(
            f.value(
                "POST",
                "/api/me/reports",
                &reporter,
                json!({"kind":kind,"target_id":id,"reason":"other"}),
                StatusCode::OK,
            )
            .await,
        );
    }
    for (input, expected) in [
        (
            json!({"kind":"rating","target_id":run["id"],"reason":"other"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"kind":"review","target_id":run["id"],"reason":"bogus"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"kind":"review","target_id":run["id"],"reason":"other","details":"a".repeat(2001)}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"kind":"review","target_id":run["id"],"reason":"other","details":"x\u{0}"}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            f.call("POST", "/api/me/reports", Some(&reporter), input)
                .await
                .status(),
            expected
        );
    }
    assert_eq!(
        f.call("POST", "/api/me/reports", Some(&author), input.clone())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.call(
            "POST",
            "/api/me/reports",
            Some(&reporter),
            json!({"kind":"review","target_id":private_run["id"],"reason":"other"})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let report_url = format!("/api/operator/reports/{}", receipt["id"].as_str().unwrap());
    let restriction_url = format!(
        "/api/operator/restrictions/review/{}",
        run["id"].as_str().unwrap()
    );
    for token in [
        None,
        Some(author.as_str()),
        Some(reporter.as_str()),
        Some(other.as_str()),
    ] {
        let expected = if token.is_none() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::FORBIDDEN
        };
        for (method, url, payload) in [
            ("GET", "/api/operator/reports", Value::Null),
            ("GET", report_url.as_str(), Value::Null),
            ("GET", "/api/operator/restrictions", Value::Null),
            ("GET", "/api/operator/audit", Value::Null),
            (
                "PUT",
                restriction_url.as_str(),
                json!({"active":true,"expected_version":0,"reason":"Not authorized"}),
            ),
            (
                "PUT",
                report_url.as_str(),
                json!({"status":"resolved","expected_version":1,"reason":"Not authorized"}),
            ),
        ] {
            assert_eq!(
                f.call(method, url, token, payload).await.status(),
                expected,
                "{method} {url}"
            );
        }
    }
    // Only operator routes can inspect a case; no reporter or reported-user read route exists.
    assert_eq!(
        f.call("GET", "/api/me/reports", Some(&reporter), Value::Null)
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let queue = browse(&f, &operator, "/api/operator/reports").await;
    assert_eq!(queue.as_array().unwrap().len(), 4);
    for row in queue.as_array().unwrap() {
        for key in ["reporter_id", "owner_id", "operator_id"] {
            assert!(row.get(key).is_none());
        }
        let evidence = &row["evidence"];
        for key in [
            "user_id",
            "game_id",
            "version",
            "created_at",
            "updated_at",
            "review_visibility",
            "rating_visibility",
            "summary_visibility",
        ] {
            assert!(evidence.get(key).is_none(), "evidence leaked {key}");
        }
        if row["kind"] == "review" {
            assert_eq!(evidence["review"], "Chosen review");
            assert_eq!(evidence["presentation_on"], "1999-01-01");
            for key in ["platform", "rating", "state", "started_on"] {
                assert!(evidence.get(key).is_none());
            }
        }
        if row["kind"] == "list" {
            assert_eq!(evidence["games"][0]["igdb_id"], 43);
            assert_eq!(evidence["games"][1]["igdb_id"], 42);
        }
    }
    assert_eq!(
        browse(&f, &operator, "/api/operator/reports?limit=1&offset=1").await[0],
        queue[1]
    );
    let resolution = json!({"status":"resolved","expected_version":1,"reason":"Reviewed the case"});
    let (a, b) = tokio::join!(
        f.call("PUT", &report_url, Some(&operator), resolution.clone()),
        f.call("PUT", &report_url, Some(&operator), resolution)
    );
    assert!(
        (a.status() == StatusCode::OK && b.status() == StatusCode::PRECONDITION_FAILED)
            || (b.status() == StatusCode::OK && a.status() == StatusCode::PRECONDITION_FAILED)
    );
    assert_eq!(
        browse(&f, &operator, &report_url).await["status"],
        "resolved"
    );
    assert_eq!(
        browse(&f, &operator, "/api/operator/audit").await[0]["action"],
        "resolve_report"
    );
    // Resolution is not deletion: retries keep the same case, never overwrite evidence.
    assert_eq!(
        f.value(
            "POST",
            "/api/me/reports",
            &reporter,
            input.clone(),
            StatusCode::OK
        )
        .await,
        receipt
    );
    assert_ne!(
        f.value("POST", "/api/me/reports", &other, input, StatusCode::OK)
            .await,
        receipt
    );
    // Stored evidence survives item deletion, but not longer than retention or account deletion.
    f.edit(
        "DELETE",
        &run_path(&run),
        &author,
        &tag(&run),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        browse(&f, &operator, &report_url).await["evidence"]["review"],
        "Chosen review"
    );
    // Revoking a DB grant immediately revokes the existing session's operator access.
    sqlx::query("DELETE FROM moderation_operators WHERE user_id=$1")
        .bind(operator_id.0.as_bytes().as_slice())
        .execute(&f.database.app)
        .await
        .unwrap();
    assert_eq!(
        f.call("GET", &report_url, Some(&operator), Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn safety_report_quota_retention_deletion_and_migration_preserve_private_data() {
    let f = Fixture::new().await;
    let (operator_id, operator) = safety_operator(&f).await;
    let (author_id, author) = f.user("retention-author").await;
    let (reporter_id, reporter) = f.user("retention-reporter").await;
    list_games_fixture(&f).await;
    let profile = publication(&f, &author, true).await;
    let run = feed_run(&f, &author, None, None).await;
    let run=share_run(&f,&author,&run,json!({"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"})).await;
    // Simulate upgrading a pre-safety database with already shared content.
    undo_moderation(&f.database.app).await;
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&f.database.app)
        .await
        .unwrap();
    assert_eq!(browse(&f, &author, &run_path(&run)).await, run);
    assert_eq!(browse(&f, &author, "/api/me").await["profile"], profile);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moderation_restrictions")
            .fetch_one(&f.database.app)
            .await
            .unwrap(),
        0
    );
    // The upgrade creates no operator grants either.
    assert_eq!(
        f.call("GET", "/api/operator/reports", Some(&operator), Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("INSERT INTO moderation_operators(user_id) VALUES($1)")
        .bind(operator_id.0.as_bytes().as_slice())
        .execute(&f.database.app)
        .await
        .unwrap();
    let mut receipts = Vec::new();
    let mut notes = Vec::new();
    for _ in 0..20 {
        let note = f.note(&author, &run, "Evidence to retain", None).await;
        let note = share_note(&f, &author, &note).await;
        let receipt=f.value("POST","/api/me/reports",&reporter,json!({"kind":"note","target_id":note["id"],"reason":"other","details":"Quoted evidence"}),StatusCode::OK).await;
        receipts.push(receipt);
        notes.push(note);
    }
    let payload = json!({"kind":"review","target_id":run["id"],"reason":"spam"});
    let denied = f
        .call("POST", "/api/me/reports", Some(&reporter), payload.clone())
        .await;
    assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(denied.headers().contains_key("retry-after"));
    assert_eq!(
        json_body(denied, StatusCode::TOO_MANY_REQUESTS).await["error"]["code"],
        "report_limit"
    );
    let repeated = f
        .value(
            "POST",
            "/api/me/reports",
            &reporter,
            json!({"kind":"note","target_id":notes[0]["id"],"reason":"spam"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(repeated, receipts[0]);
    // Expiry is enforced in reads before physical cleanup runs.
    let first = Uuid::parse_str(receipts[0]["id"].as_str().unwrap()).unwrap();
    sqlx::query("UPDATE moderation_reports SET created_at=now()-interval '91 days' WHERE id=$1")
        .bind(first)
        .execute(&f.database.app)
        .await
        .unwrap();
    let expired = browse(&f, &operator, &format!("/api/operator/reports/{first}")).await;
    assert_eq!(expired["evidence"], Value::Null);
    assert_eq!(expired["details"], "");
    gameboxd_backend::db::purge_moderation(&f.database.app)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, Option<Value>>(
            "SELECT evidence FROM moderation_reports WHERE id=$1"
        )
        .bind(first)
        .fetch_one(&f.database.app)
        .await
        .unwrap(),
        None
    );
    let review = f
        .value(
            "POST",
            "/api/me/reports",
            &reporter,
            payload,
            StatusCode::OK,
        )
        .await;
    restriction(&f, &operator, "profile", &profile["public_id"], true, 0).await;
    let block_url = format!("/api/me/blocks/{}", profile["public_id"].as_str().unwrap());
    assert_eq!(
        f.call("PUT", &block_url, Some(&reporter), Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    // Reporter deletion removes identity without destroying the reported evidence.
    f.auth
        .state()
        .operator()
        .delete_account(
            reporter_id,
            OperatorActor::new("safety-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moderation_reports WHERE reporter_id IS NOT NULL"
        )
        .fetch_one(&f.database.app)
        .await
        .unwrap(),
        0
    );
    let review_path = format!("/api/operator/reports/{}", review["id"].as_str().unwrap());
    assert_eq!(
        browse(&f, &operator, &review_path).await["evidence"]["review"],
        "Chosen review"
    );
    f.auth
        .state()
        .operator()
        .delete_account(
            author_id,
            OperatorActor::new("safety-test", None, None, None).unwrap(),
        )
        .await
        .unwrap();
    let deleted = browse(&f, &operator, &review_path).await;
    assert_eq!(deleted["evidence"], Value::Null);
    assert_eq!(deleted["details"], "");
    assert_eq!(
        browse(&f, &operator, "/api/operator/audit").await[0]["reason"],
        "Account deleted"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moderation_restrictions")
            .fetch_one(&f.database.app)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM account_blocks")
            .fetch_one(&f.database.app)
            .await
            .unwrap(),
        0
    );
    f.finish().await;
}
