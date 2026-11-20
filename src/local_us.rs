//! In-process US router transport for data-api's UsClient.

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Method, Request};
use axum::Router;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as SyncMutex, OnceLock};
use tokio::sync::{broadcast, mpsc, Mutex};
use tower::ServiceExt;

use dl_us::{install_local, LocalRequest, LocalResponse};

static ROUTER: OnceLock<Router> = OnceLock::new();
static TX: OnceLock<mpsc::Sender<LocalRequest>> = OnceLock::new();

pub fn install_router(router: Router) {
    let _ = ROUTER.set(router);
}

static SNAPSHOTS: OnceLock<Mutex<HashMap<String, HashMap<String, LocalResponse>>>> = OnceLock::new();

/// In-flight reads keyed by request: the leader inserts a broadcast sender, waiters subscribe.
type Flights = Arc<SyncMutex<HashMap<String, broadcast::Sender<LocalResponse>>>>;

/// The leader of one in-flight US read.
///
/// It removes its entry when the task ends, **including when the task is cancelled** (client
/// disconnect, request deadline). In that case it also answers every waiter with an error:
/// leaving them subscribed to a broadcast that nobody will ever send on made them wait until
/// their own outer timeout expired.
struct Flight {
    key: String,
    flights: Flights,
    completed: bool,
}

impl Flight {
    fn complete(mut self, response: LocalResponse) {
        self.completed = true;
        if let Some(tx) = self.entries().remove(&self.key) {
            let _ = tx.send(response);
        }
    }

    fn entries(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, broadcast::Sender<LocalResponse>>> {
        self.flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if let Some(tx) = self.entries().remove(&self.key) {
            let _ = tx.send(LocalResponse {
                status: 503,
                headers: Vec::new(),
                body: b"US read cancelled".to_vec(),
            });
        }
    }
}

pub fn start() -> mpsc::Sender<LocalRequest> {
    let (tx, mut rx) = mpsc::channel::<LocalRequest>(128);
    install_local(tx.clone());
    let _ = TX.set(tx.clone());
    let flights: Flights = Arc::new(SyncMutex::new(HashMap::new()));
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            let flights = Arc::clone(&flights);
            tokio::spawn(async move {
                let key = make_key(&request);
                let snapshot = request.headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("x-dl-snapshot")).map(|(_, value)| value.clone());
                if let Some(snapshot) = &snapshot {
                    if let Some(cache) = SNAPSHOTS.get() {
                        if let Some(response) = cache.lock().await.get(snapshot).and_then(|map| map.get(&key).cloned()) {
                            let _ = request.reply.send(response);
                            return;
                        }
                    }
                }
                // Subscribe to the leader, or become it. The guard is dropped before any
                // await: a `std::sync` guard held across an await is not `Send`.
                let waiter = {
                    let mut map = flights
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match map.get(&key) {
                        Some(tx) => Some(tx.subscribe()),
                        None => {
                            let (tx, _) = broadcast::channel(8);
                            map.insert(key.clone(), tx);
                            None
                        }
                    }
                };
                if let Some(mut rx) = waiter {
                    if let Ok(response) = rx.recv().await {
                        let _ = request.reply.send(response);
                    }
                    return;
                }
                // From here on this task is the leader and owns the entry's cleanup.
                let flight = Flight {
                    key: key.clone(),
                    flights: Arc::clone(&flights),
                    completed: false,
                };
                let router = wait_router().await;
                let response = match router {
                    Some(router) => execute(router, &request).await,
                    None => LocalResponse {
                        status: 503,
                        headers: Vec::new(),
                        body: b"US router is not ready".to_vec(),
                    },
                };
                if let Some(snapshot) = &snapshot {
                    let cache = SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()));
                    let mut map = cache.lock().await;
                    if map.len() >= 256 {
                        map.clear();
                    }
                    map.entry(snapshot.clone()).or_default().insert(key.clone(), response.clone());
                }
                let _ = request.reply.send(response.clone());
                flight.complete(response);
            });
        }
    });
    tx
}

fn make_key(request: &LocalRequest) -> String {
    let mut headers: Vec<&(String, String)> = request.headers.iter().collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut key = request.path.clone();
    for (name, value) in headers {
        key.push('\n');
        key.push_str(name);
        key.push(':');
        key.push_str(value);
    }
    key
}

async fn wait_router() -> Option<Router> {
    for _ in 0..500 {
        if let Some(router) = ROUTER.get() {
            return Some(router.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    None
}

async fn execute(router: Router, request: &LocalRequest) -> LocalResponse {
    let mut builder = Request::builder().method(Method::GET).uri(&request.path);
    for (name, value) in &request.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            builder = builder.header(name, value);
        }
    }
    let req = match builder.body(Body::empty()) {
        Ok(req) => req,
        Err(error) => return LocalResponse { status: 400, headers: Vec::new(), body: error.to_string().into_bytes() },
    };
    let response = match tokio::time::timeout(crate::local_http::internal_timeout(), router.oneshot(req)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return LocalResponse { status: 502, headers: Vec::new(), body: error.to_string().into_bytes() },
        Err(_) => return LocalResponse { status: 504, headers: Vec::new(), body: b"US timeout".to_vec() },
    };
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.to_string(), value.to_string())))
        .collect();
    let body = match axum::body::to_bytes(response.into_body(), 50 * 1024 * 1024).await {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => error.to_string().into_bytes(),
    };
    LocalResponse { status, headers, body }
}

/// In-process request used by the engine's local HTTP adapter. The channel also performs
/// in-flight coalescing, so two identical US lookups in the same instant share one router call.
pub async fn call(path: String, headers: Vec<(String, String)>) -> Option<LocalResponse> {
    let sender = TX.get()?.clone();
    let (reply, response) = tokio::sync::oneshot::channel();
    sender.send(LocalRequest { path, headers, reply }).await.ok()?;
    response.await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> String {
        text.to_string()
    }

    fn leader(key: &str, flights: &Flights) -> Flight {
        let (tx, _) = broadcast::channel(8);
        flights
            .lock()
            .expect("flights")
            .insert(key.to_string(), tx);
        Flight {
            key: key.to_string(),
            flights: Arc::clone(flights),
            completed: false,
        }
    }

    /// A cancelled leader must wake its waiters instead of leaving them subscribed to a
    /// broadcast that will never send — that was a 60-second stall per waiter.
    #[tokio::test]
    async fn a_cancelled_leader_answers_waiters_and_clears_the_entry() {
        let flights: Flights = Arc::new(SyncMutex::new(HashMap::new()));
        let flight = leader(&key("entry"), &flights);
        let mut waiter = flights
            .lock()
            .expect("flights")
            .get("entry")
            .expect("entry")
            .subscribe();

        drop(flight);

        let response = waiter.recv().await.expect("waiter is woken");
        assert_eq!(response.status, 503);
        assert!(flights.lock().expect("flights").is_empty());
    }

    /// The happy path publishes the answer to the waiters and still removes the entry.
    #[tokio::test]
    async fn a_completed_leader_publishes_its_answer() {
        let flights: Flights = Arc::new(SyncMutex::new(HashMap::new()));
        let flight = leader(&key("entry"), &flights);
        let mut waiter = flights
            .lock()
            .expect("flights")
            .get("entry")
            .expect("entry")
            .subscribe();

        flight.complete(LocalResponse {
            status: 200,
            headers: Vec::new(),
            body: b"ok".to_vec(),
        });

        let response = waiter.recv().await.expect("waiter is woken");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
        assert!(flights.lock().expect("flights").is_empty());
    }
}
