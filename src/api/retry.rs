//! Private successful-request receipts. No pending row ever commits: the
//! transaction lock, domain mutation and receipt are one atomic unit.
use super::*;
use sqlx::{Postgres, Transaction};

type StoredReceipt = (bool, bool, i16, Option<Vec<u8>>, Option<String>);

pub(super) struct Receipt {
    key: Option<String>,
    operation: String,
    payload: Vec<u8>,
}

impl Receipt {
    pub(super) fn new(
        headers: &HeaderMap,
        operation: &str,
        input: &impl Serialize,
    ) -> ApiResult<Self> {
        let values = headers.get_all("idempotency-key");
        let mut values = values.iter();
        let key = values
            .next()
            .map(|v| v.to_str())
            .transpose()
            .map_err(|_| invalid_key())?;
        if values.next().is_some()
            || key.is_some_and(|key| {
                key.is_empty()
                    || key.len() > 128
                    || !key
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
            })
        {
            return Err(invalid_key());
        }
        // Serialize the parsed input through Value's sorted object maps. JSON
        // whitespace/key order and omitted nullable defaults are insignificant;
        // array order and string content are significant. Include preconditions.
        let payload = if key.is_some() {
            let preconditions: Vec<_> = headers
                .get_all(header::IF_MATCH)
                .iter()
                .map(|v| v.as_bytes())
                .collect();
            let value = serde_json::to_value((input, preconditions)).map_err(|_| internal())?;
            serde_json::to_vec(&value).map_err(|_| internal())?
        } else {
            Vec::new()
        };
        Ok(Self {
            key: key.map(str::to_owned),
            operation: operation.to_owned(),
            payload,
        })
    }

    pub(super) async fn replay(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        owner: &[u8],
    ) -> ApiResult<Option<Response>> {
        let Some(key) = &self.key else {
            return Ok(None);
        };
        // Bound waiting on both account deletion and competing requests. A
        // timeout/cancellation drops the transaction and leaves no reservation.
        sqlx::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut **tx)
            .await?;
        sqlx::query("SELECT id FROM gameboxd_auth.users WHERE id=$1 FOR KEY SHARE")
            .bind(owner)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(ApiError::not_found)?;
        // Namespace and tuple encoding avoid ambiguous concatenation. A hash
        // collision can only serialize unrelated work, never share its result.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(json_build_array('gameboxd:receipt',encode($1::bytea,'hex'),$2::text,$3::text)::text,0))")
            .bind(owner).bind(&self.operation).bind(key).execute(&mut **tx).await
            .map_err(|error| {
                if error.as_database_error().is_some_and(|e| e.code().as_deref() == Some("55P03")) {
                    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::DatabaseUnavailable,
                        "A matching request is still running; retry with the same key and payload")
                } else { error.into() }
            })?;
        self.read(tx, owner).await
    }

    // Check committed receipts before provider work without holding a write
    // lock or connection across network calls. The final transaction rechecks
    // under the lock, so concurrent cache misses still commit exactly once.
    pub(super) async fn cached(
        &self,
        pool: &sqlx::PgPool,
        owner: &[u8],
    ) -> ApiResult<Option<Response>> {
        if self.key.is_none() {
            return Ok(None);
        }
        let mut tx = pool.begin().await?;
        self.read(&mut tx, owner).await
    }

    async fn read(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        owner: &[u8],
    ) -> ApiResult<Option<Response>> {
        let Some(key) = &self.key else {
            return Ok(None);
        };
        let saved: Option<StoredReceipt> = sqlx::query_as(
            "SELECT fingerprint=sha256($4),expires_at<=clock_timestamp(),status,
             CASE WHEN expires_at>clock_timestamp() THEN body END,etag
             FROM request_receipts WHERE user_id=$1 AND operation=$2 AND key=$3",
        )
        .bind(owner)
        .bind(&self.operation)
        .bind(key)
        .bind(&self.payload)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((matches, expired, status, body, etag)) = saved else {
            return Ok(None);
        };
        if !matches {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::IdempotencyConflict,
                "This key already succeeded with a different payload or precondition",
            ));
        }
        if expired || body.is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::IdempotencyExpired,
                "The result expired; inspect your records before deliberately submitting a new operation with a new key",
            ));
        }
        let mut response = (
            StatusCode::from_u16(status as u16).map_err(|_| internal())?,
            [(header::CONTENT_TYPE, "application/json")],
            body.unwrap(),
        )
            .into_response();
        if let Some(etag) = etag {
            response.headers_mut().insert(
                header::ETAG,
                HeaderValue::from_str(&etag).map_err(|_| internal())?,
            );
        }
        Ok(Some(response))
    }

    pub(super) async fn finish(
        self,
        mut tx: Transaction<'_, Postgres>,
        owner: &[u8],
        result: impl IntoResponse,
    ) -> ApiResult<Response> {
        let response = result.into_response();
        // Do not turn a serializer failure into a committed write or receipt.
        if !response.status().is_success() {
            return Err(internal());
        }
        if let Some(key) = self.key {
            let (parts, body) = response.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX)
                .await
                .map_err(|_| internal())?;
            let etag = parts
                .headers
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok());
            sqlx::query(
                "INSERT INTO request_receipts(user_id,operation,key,fingerprint,status,body,etag)
                VALUES($1,$2,$3,sha256($4),$5,$6,$7)",
            )
            .bind(owner)
            .bind(self.operation)
            .bind(key)
            .bind(self.payload)
            .bind(parts.status.as_u16() as i16)
            .bind(bytes.as_ref())
            .bind(etag)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(Response::from_parts(parts, axum::body::Body::from(bytes)))
        } else {
            tx.commit().await?;
            Ok(response)
        }
    }
}

fn invalid_key() -> ApiError {
    ApiError::invalid("Send one Idempotency-Key containing 1–128 ASCII letters, digits, or -_.:")
}
fn internal() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::InternalError,
        "Could not serialize the request result",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_are_bounded_and_unambiguous() {
        for key in ["", "has spaces", "a,b", "a/b", &"a".repeat(129)] {
            let mut headers = HeaderMap::new();
            headers.insert("idempotency-key", HeaderValue::from_str(key).unwrap());
            assert!(Receipt::new(&headers, "test", &()).is_err());
        }
        let mut headers = HeaderMap::new();
        headers.append("idempotency-key", HeaderValue::from_static("one"));
        headers.append("idempotency-key", HeaderValue::from_static("two"));
        assert!(Receipt::new(&headers, "test", &()).is_err());
        headers.remove("idempotency-key");
        assert!(Receipt::new(&headers, "test", &()).unwrap().key.is_none());
        headers.insert(
            "idempotency-key",
            HeaderValue::from_static("valid-KEY_1.2:3"),
        );
        let first = Receipt::new(&headers, "test", &serde_json::json!({"b":1,"a":2})).unwrap();
        let second = Receipt::new(&headers, "test", &serde_json::json!({"a":2,"b":1})).unwrap();
        assert_eq!(first.payload, second.payload);
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"1\""));
        assert_ne!(
            first.payload,
            Receipt::new(&headers, "test", &serde_json::json!({"a":2,"b":1}))
                .unwrap()
                .payload
        );
    }
}
