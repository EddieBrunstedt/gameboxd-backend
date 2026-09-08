//! Out-of-band refresh of metadata for games that are already stored.
//!
//! Attaching a personal record to a game the user already has must not wait for
//! IGDB. Stored metadata is served immediately however old it is, and a stale
//! row is queued here instead, so provider latency — its request spacing, its
//! queue, its rate limits, its outages — is paid by a background task rather
//! than by somebody pressing Save.
//!
//! Refreshing is best-effort by design. A dropped or failed refresh costs
//! nothing beyond metadata staying stale a while longer, so the queue is
//! bounded and a request never waits on it, never fails because of it, and
//! never learns whether it happened.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use sqlx::PgPool;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    igdb::IgdbClient,
    models::{find_game, upsert_game},
};

/// Distinct games that may be waiting for a refresh at once. Requests past
/// this are dropped rather than queued: the backlog stays bounded, and the
/// next save of the same game asks again.
const QUEUE_CAPACITY: usize = 64;

/// The handle a request handler uses to ask that one game's stored metadata be
/// refreshed later.
///
/// Cheap to clone and safe to call from a handler: [`Refresher::request`] never
/// blocks, never awaits upstream, and reports no error a caller could act on.
#[derive(Clone)]
pub struct Refresher {
    queue: Option<Queue>,
}

#[derive(Clone)]
struct Queue {
    sender: mpsc::Sender<i64>,
    /// The games already queued. Holding at most one pending refresh per game
    /// keeps a burst of saves for one stale game from crowding every other
    /// game out of a bounded queue.
    pending: Arc<Mutex<HashSet<i64>>>,
}

/// The consuming half of the queue.
///
/// It deliberately owns no [`Refresher`], so the channel closes — and the
/// worker stops — as soon as the last handle is dropped. For the server that
/// is the moment the router state goes away at shutdown.
pub struct Worker {
    receiver: mpsc::Receiver<i64>,
    pending: Arc<Mutex<HashSet<i64>>>,
}

impl Refresher {
    /// Starts the worker and returns the handle requests go through.
    ///
    /// One worker, processing one game at a time: background refreshes can
    /// then never occupy more than a single one of the provider client's
    /// in-flight slots, so they cannot crowd out the foreground imports that a
    /// user is actually waiting for.
    pub fn spawn(pool: PgPool, igdb: IgdbClient) -> (Self, JoinHandle<()>) {
        let (refresher, worker) = Self::channel(QUEUE_CAPACITY);
        (refresher, tokio::spawn(worker.run(pool, igdb)))
    }

    /// A handle that accepts and discards every request, for an assembly that
    /// deliberately runs no worker — above all a test that asserts on exactly
    /// which upstream calls the request under test makes.
    pub fn disabled() -> Self {
        Self { queue: None }
    }

    fn channel(capacity: usize) -> (Self, Worker) {
        let (sender, receiver) = mpsc::channel(capacity);
        let pending = Arc::new(Mutex::new(HashSet::new()));
        (
            Self {
                queue: Some(Queue {
                    sender,
                    pending: pending.clone(),
                }),
            },
            Worker { receiver, pending },
        )
    }

    /// Asks for `igdb_id` to be refreshed, and returns immediately.
    ///
    /// A game already queued, a full queue and a stopped worker are all
    /// ordinary outcomes here, not failures: each one leaves the stored
    /// metadata exactly as the caller already served it.
    pub fn request(&self, igdb_id: i64) {
        let Some(queue) = &self.queue else { return };
        if !lock(&queue.pending).insert(igdb_id) {
            return;
        }
        if let Err(error) = queue.sender.try_send(igdb_id) {
            lock(&queue.pending).remove(&igdb_id);
            match error {
                mpsc::error::TrySendError::Full(_) => {
                    tracing::debug!(igdb_id, "Refresh queue is full; metadata stays stale");
                }
                mpsc::error::TrySendError::Closed(_) => {
                    tracing::warn!(igdb_id, "Refresh worker has stopped; metadata stays stale");
                }
            }
        }
    }
}

impl Worker {
    async fn run(mut self, pool: PgPool, igdb: IgdbClient) {
        while let Some(igdb_id) = self.take().await {
            refresh(&pool, &igdb, igdb_id).await;
        }
        tracing::debug!("Metadata refresh worker stopped");
    }

    /// The next game to refresh, released from the pending set before its work
    /// starts rather than after.
    ///
    /// A save arriving during a slow upstream call then queues a new request
    /// instead of being silently swallowed by the one already running. The
    /// duplicate costs one local lookup, because [`refresh`] finds the row
    /// current by then and stops.
    async fn take(&mut self) -> Option<i64> {
        let igdb_id = self.receiver.recv().await?;
        lock(&self.pending).remove(&igdb_id);
        Some(igdb_id)
    }
}

/// Refreshes one game, if it is still both stored and stale.
///
/// Every failure is logged and swallowed. The save that queued this refresh
/// has already succeeded and been reported to its caller; stale metadata is
/// the documented fallback, and there is nobody left to tell.
async fn refresh(pool: &PgPool, igdb: &IgdbClient, igdb_id: i64) {
    match find_game(pool, igdb_id).await {
        Ok(Some(game)) if !game.is_fresh() => {}
        // Gone, already current, or unreadable. A background task imports
        // nothing the user did not ask for, so a row that has disappeared must
        // not be recreated here; a lookup failure was logged where it happened.
        _ => return,
    }
    match igdb.get(igdb_id).await {
        Ok(upstream) => {
            if upsert_game(pool, upstream).await.is_ok() {
                tracing::info!(igdb_id, "Refreshed stale game metadata");
            }
        }
        Err(error) => tracing::info!(
            igdb_id,
            code = error.code.as_str(),
            "Background metadata refresh failed; stored metadata retained"
        ),
    }
}

/// The pending set is only ever inserted into and removed from, so a panic
/// elsewhere cannot leave it inconsistent. Recover from poisoning rather than
/// spreading it: losing refreshes would be a poor way to react to it.
fn lock(pending: &Mutex<HashSet<i64>>) -> MutexGuard<'_, HashSet<i64>> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_coalesce_per_game_and_stop_at_capacity() {
        let (refresher, mut worker) = Refresher::channel(2);
        for _ in 0..3 {
            refresher.request(42);
        }
        refresher.request(7);
        // The queue is full: this one is dropped rather than queued or awaited.
        refresher.request(9);
        assert_eq!(worker.take().await, Some(42));
        assert_eq!(worker.take().await, Some(7));

        // Taking a game releases it, so a later save can queue it again, and a
        // game dropped by a full queue was never left marked as pending.
        refresher.request(42);
        refresher.request(9);
        assert_eq!(worker.take().await, Some(42));
        assert_eq!(worker.take().await, Some(9));

        // Dropping the last handle ends the worker rather than parking it.
        drop(refresher);
        assert_eq!(worker.take().await, None);
    }

    #[tokio::test]
    async fn a_disabled_refresher_discards_requests_without_a_worker() {
        let refresher = Refresher::disabled();
        refresher.clone().request(42);
        refresher.request(42);
    }
}
