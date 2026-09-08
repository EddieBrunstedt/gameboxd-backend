pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod igdb;
pub mod models;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use doorman::prelude::AuthService;
use sqlx::PgPool;
use tower_http::{
    cors::CorsLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

use crate::{config::Config, error::ApiError, igdb::IgdbClient};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub igdb: IgdbClient,
}

pub fn router(config: &Config, state: AppState, auth: &AuthService) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(
            HeaderValue::from_str(&config.frontend_origin.origin().ascii_serialization())
                .expect("validated origin"),
        )
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .expose_headers([
            header::HeaderName::from_static("x-request-id"),
            header::RETRY_AFTER,
        ]);
    let request_id = header::HeaderName::from_static("x-request-id");
    api::routes()
        .with_state(state.clone())
        .merge(auth.router())
        .layer(auth.session_layer())
        // Health checks do not require an authentication DB lookup.
        .route("/health/live", get(|| async { Json(serde_json::json!({"status":"ok"})) }))
        .route("/health/ready", get(ready).with_state(state))
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async { ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "Method not allowed") })
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(response_policy))
        .layer(cors)
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(TraceLayer::new_for_http().make_span_with(|request: &Request| {
            // Do not log query strings, cookies, authorization headers, or bodies.
            tracing::info_span!("http_request", method = %request.method(), path = %request.uri().path(),
                request_id = request.headers().get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or(""))
        }))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
}

async fn ready(State(state): State<AppState>) -> Response {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => Json(serde_json::json!({"status":"ok"})).into_response(),
        Err(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Database is unavailable",
        )
        .into_response(),
    }
}

async fn response_policy(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    // Normalize Axum's text rejections (including those inside Doorman) without
    // rewriting Doorman's existing JSON machine-readable error codes.
    let json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if response.status().is_client_error() && !json {
        let status = response.status();
        response = ApiError::new(
            status,
            "request_rejected",
            status.canonical_reason().unwrap_or("Request rejected"),
        )
        .into_response();
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if response.status() == StatusCode::TOO_MANY_REQUESTS
        && !response.headers().contains_key(header::RETRY_AFTER)
    {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}
