//! HTTP client for `us`, the storage every DataLens object lives in.
//!
//! One endpoint matters on the data path: `GET /private/entries/{entry_id}`, which returns
//! the dataset JSON (`dl_us_entries_client.clients.private.models.entry`). Authorisation is
//! either the master token (`X-US-Master-Token`, what the container uses) or the caller's
//! bearer token, forwarded as is.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use dl_core::Dataset;
use serde_json::Value;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE,
};
use reqwest::StatusCode;

use crate::dataset::to_dataset;
use crate::entry::UsEntry;
use crate::error::UsError;


/// `DL_US_LOCAL_TIMEOUT_SECS` — deadline for one in-process US request (default 60 s).
fn local_timeout() -> Duration {
    static TIMEOUT: OnceLock<Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        Duration::from_secs(
            std::env::var("DL_US_LOCAL_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|seconds| *seconds > 0)
                .unwrap_or(60),
        )
    })
}

/// `X-US-Master-Token` (`dl_constants.api_constants.DLHeadersCommon.US_MASTER_TOKEN`).
const MASTER_TOKEN_HEADER: &str = "x-us-master-token";
/// `X-DL-Component`: which component of the platform is asking.
const COMPONENT_HEADER: &str = "x-dl-component";

/// How the client authenticates against `us`.
#[derive(Debug, Clone, Default)]
pub enum UsAuth {
    /// No credentials: the container runs without `us` (dry runs, tests).
    #[default]
    None,
    /// `X-US-Master-Token`, the mode the data-api container uses.
    MasterToken(String),
}

/// A minimal `us` client: read entries, decode datasets.
#[derive(Debug, Clone)]
pub struct UsClient {
    http: reqwest::Client,
    base_url: String,
    auth: UsAuth,
    branch: String,
    component: String,
}

/// Root certificates for outbound HTTPS: the compiled-in webpki roots plus the PEM bundle
/// named by `DL_CA_FILE` (or `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE`). A private CA therefore
/// works in the `scratch` image without a rebuild — mount the PEM and set the variable.
fn with_ca_bundle(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let path = ["DL_CA_FILE", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    let Some(path) = path else {
        return builder;
    };
    let pem = match std::fs::read(&path) {
        Ok(pem) => pem,
        Err(error) => {
            tracing::warn!(path, %error, "cannot read the CA bundle");
            return builder;
        }
    };
    let certificates = match reqwest::Certificate::from_pem_bundle(&pem) {
        Ok(certificates) => certificates,
        Err(error) => {
            tracing::warn!(path, %error, "cannot parse the CA bundle");
            return builder;
        }
    };
    let mut builder = builder;
    for certificate in certificates {
        builder = builder.add_root_certificate(certificate);
    }
    builder
}

/// `reqwest::Error`'s `Display` is only "error sending request": the cause (a TLS
/// verification failure, a DNS error, a reset) is one or two links down `source()`.
fn describe_reqwest(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// An in-process request to the US router (used by the monolith).
pub struct LocalRequest {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub reply: oneshot::Sender<LocalResponse>,
}

#[derive(Clone)]
pub struct LocalResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

static LOCAL_TX: OnceLock<mpsc::Sender<LocalRequest>> = OnceLock::new();

/// Install the in-process US transport. `UsClient::new("inproc://us")` uses it.
pub fn install_local(sender: mpsc::Sender<LocalRequest>) {
    let _ = LOCAL_TX.set(sender);
}

/// A synchronous, allocation-free alternative to the channel transport: the monolith
/// resolves `/private/entries/{id}` directly from the already-initialised `us-rs`
/// state (request-scoped bundle first, then the SQL replica pool) instead of sending a
/// `LocalRequest` through the `local_us` channel and a `tower::oneshot` router.
pub type DirectResolver = Arc<
    dyn Fn(String, Vec<(String, String)>) -> Pin<Box<dyn Future<Output = Option<LocalResponse>> + Send>>
        + Send
        + Sync,
>;

static DIRECT_RESOLVER: OnceLock<DirectResolver> = OnceLock::new();

/// Install the direct resolver. It takes precedence over the channel transport; the
/// channel stays available for standalone builds (`UsClient` without the monolith).
pub fn install_direct(resolver: DirectResolver) {
    let _ = DIRECT_RESOLVER.set(resolver);
}

/// A resolver that already returns the parsed entry JSON. The monolith installs it so an
/// in-process US read does not have to serialize its `Value` to bytes and parse the bytes
/// straight back into a `Value` (`get_entry` then only decodes the typed `UsEntry`).
pub type ValueResolver = Arc<
    dyn Fn(String, Vec<(String, String)>) -> Pin<Box<dyn Future<Output = Option<(u16, Arc<Value>)>> + Send>>
        + Send
        + Sync,
>;

static VALUE_RESOLVER: OnceLock<ValueResolver> = OnceLock::new();

/// Install the value resolver; it takes precedence over [`install_direct`].
pub fn install_value_direct(resolver: ValueResolver) {
    let _ = VALUE_RESOLVER.set(resolver);
}

/// A resolver that returns the request's typed dataset without any JSON hop.
///
/// `data-api` asks for the dataset behind an entry on every `/fields` and every `/result`.
/// In the monolith the US read model already holds the request-scoped bundle, so the dataset
/// can be decoded once per request and shared (`Arc<Dataset>`) by all those calls instead of
/// being re-decoded from the entry JSON 2-3 times.
pub type DatasetResolver =
    Arc<dyn Fn(&str) -> Option<(Arc<Dataset>, Option<String>)> + Send + Sync>;

static DATASET_RESOLVER: OnceLock<DatasetResolver> = OnceLock::new();

/// Install the typed dataset resolver (monolith chart path).
pub fn install_dataset_direct(resolver: DatasetResolver) {
    let _ = DATASET_RESOLVER.set(resolver);
}

/// The typed dataset from the installed resolver, when there is one in scope.
pub fn direct_dataset(entry_id: &str) -> Option<(Arc<Dataset>, Option<String>)> {
    DATASET_RESOLVER.get().and_then(|resolver| resolver(entry_id))
}

/// Decode a `(status, Value)` US answer into an `UsEntry`, with the same error mapping as the
/// byte path.
fn entry_from_value(entry_id: &str, status: u16, value: Arc<Value>) -> Result<UsEntry, UsError> {
    let status = match StatusCode::from_u16(status) {
        Ok(status) => status,
        Err(error) => return Err(UsError::Transport(error.to_string())),
    };
    if status == StatusCode::NOT_FOUND {
        return Err(UsError::NotFound {
            entry_id: entry_id.to_string(),
        });
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(UsError::Unauthorized {
            status: status.as_u16(),
        });
    }
    if !status.is_success() {
        let body = value.to_string();
        return Err(UsError::Status {
            status: status.as_u16(),
            body: truncate(&body, 512),
        });
    }
    // `UsEntry` owns its `data`, so this clone is unavoidable here; the hot chart path does
    // not reach this function (its entry and dataset come from the bundle as `Arc`s).
    serde_json::from_value((*value).clone()).map_err(|error| UsError::Decode {
        entry_id: entry_id.to_string(),
        reason: error.to_string(),
    })
}

impl UsClient {
    /// A client for `base_url` (e.g. `http://us:8081`).
    pub fn new(base_url: impl Into<String>) -> Result<Self, UsError> {
        let http = with_ca_bundle(reqwest::Client::builder().timeout(Duration::from_secs(10)).tcp_nodelay(true))
            .build()
            .map_err(|error| UsError::Transport(describe_reqwest(&error)))?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: UsAuth::None,
            branch: "published".to_string(),
            component: "backend".to_string(),
        })
    }

    pub fn with_master_token(mut self, token: impl Into<String>) -> Self {
        self.auth = UsAuth::MasterToken(token.into());
        self
    }

    /// `branch` query parameter: `published` (what the UI reads) or `saved`.
    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = branch.into();
        self
    }

    pub fn with_component(mut self, component: impl Into<String>) -> Self {
        self.component = component.into();
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// `GET /private/entries/{entry_id}?branch=...` with the configured credentials.
    ///
    /// A saved-but-unpublished object (`publishedId: null` — every demo connection) is not
    /// served on the `published` branch: `us` answers either `404` or `data: null`, so the call
    /// is repeated on `saved`. Python's `us_manager` does the same — the object is read from
    /// the branch it lives in.
    pub async fn get_entry(&self, entry_id: &str) -> Result<UsEntry, UsError> {
        let entry = match self.request(entry_id, None, None).await {
            Ok(entry) => entry,
            Err(error) => return self.retry_saved(entry_id, error, None).await,
        };
        self.follow_saved(entry, None).await
    }

    /// The same call, on behalf of the user whose bearer token this is.
    pub async fn get_entry_as(&self, entry_id: &str, token: &str) -> Result<UsEntry, UsError> {
        let entry = match self.request(entry_id, Some(token), None).await {
            Ok(entry) => entry,
            Err(error) => return self.retry_saved(entry_id, error, Some(token)).await,
        };
        self.follow_saved(entry, Some(token)).await
    }

    /// The dataset behind `entry_id`, ready for the query translator.
    pub async fn get_dataset(&self, entry_id: &str) -> Result<Dataset, UsError> {
        let entry = self.get_entry(entry_id).await?;
        to_dataset(entry_id, &entry.dataset_data()?)
    }

    /// The `connection` entry `connection_id` points at, with its `unversioned_data` secrets.
    pub async fn get_connection(&self, connection_id: &str) -> Result<UsEntry, UsError> {
        let entry = self.get_entry(connection_id).await?;
        entry.connection_data()?;
        Ok(entry)
    }

    async fn follow_saved(&self, entry: UsEntry, bearer: Option<&str>) -> Result<UsEntry, UsError> {
        if entry.data.is_some() || entry.published_id.is_some() || self.branch == "saved" {
            return Ok(entry);
        }
        self.request(&entry.entry_id, bearer, Some("saved")).await
    }

    /// A `404` on the configured branch may simply mean "not published"; try `saved`.
    async fn retry_saved(
        &self,
        entry_id: &str,
        error: UsError,
        bearer: Option<&str>,
    ) -> Result<UsEntry, UsError> {
        match (&error, self.branch.as_str()) {
            (UsError::NotFound { .. }, "published") => {
                self.request(entry_id, bearer, Some("saved")).await
            }
            _ => Err(error),
        }
    }

    async fn request(
        &self,
        entry_id: &str,
        bearer: Option<&str>,
        branch: Option<&str>,
    ) -> Result<UsEntry, UsError> {
        let branch = branch.unwrap_or(&self.branch);
        let url = format!(
            "{}/private/entries/{}?branch={}",
            self.base_url,
            percent_encode(entry_id),
            percent_encode(branch)
        );
        let mut headers = HeaderMap::new();
        if let Ok(component) = HeaderValue::from_str(&self.component) {
            headers.insert(COMPONENT_HEADER, component);
        }
        // MessagePack is opt-in for this hop: `us` metadata answers are small, and its msgpack
        // layer has to buffer JSON, parse it and re-encode it. JSON is the default; the
        // `DL_US_MSGPACK=1` experiment keeps the old negotiated path available.
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        if std::env::var("DL_US_MSGPACK")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            headers.insert(ACCEPT, HeaderValue::from_static("application/msgpack"));
        }
        match (&self.auth, bearer) {
            (_, Some(token)) => {
                let value = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| UsError::Unauthorized { status: 401 })?;
                headers.insert(AUTHORIZATION, value);
            }
            (UsAuth::MasterToken(token), None) => {
                let value = HeaderValue::from_str(token)
                    .map_err(|error| UsError::Transport(error.to_string()))?;
                headers.insert(MASTER_TOKEN_HEADER, value);
            }
            (UsAuth::None, None) => {}
        }

        let (status, content_type, bytes) = if self.base_url == "inproc://us" {
            let path = format!(
                "/private/entries/{}?branch={}",
                percent_encode(entry_id),
                percent_encode(branch)
            );
            let mut local_headers: Vec<(String, String)> = headers
                .iter()
                .filter_map(|(name, value)| {
                    value.to_str().ok().map(|value| (name.to_string(), value.to_string()))
                })
                .collect();
            if let Some(snapshot) = crate::current_snapshot() {
                local_headers.push(("x-dl-snapshot".to_string(), snapshot));
            }
            // Already-parsed answer (monolith chart path): no JSON bytes on the in-process hop.
            if let Some(resolver) = VALUE_RESOLVER.get() {
                if let Some((status, value)) = resolver(path.clone(), local_headers.clone()).await {
                    return entry_from_value(entry_id, status, value);
                }
            }
            // Direct typed read (monolith chart path): the request-scoped bundle or the
            // `us-rs` SQL replica pool, with no channel hop and no router round trip.
            let direct = match DIRECT_RESOLVER.get() {
                Some(resolver) => resolver(path.clone(), local_headers.clone()).await,
                None => None,
            };
            let response = match direct {
                Some(response) => response,
                None => {
                    let Some(sender) = LOCAL_TX.get() else {
                        return Err(UsError::Transport(
                            "in-process US transport is not installed".to_string(),
                        ));
                    };
                    // The in-process hop skips the reqwest client (and its 10 s timeout), so
                    // it needs its own deadline: a stalled US handler must not hold a data-api
                    // request forever.
                    let exchange = async {
                        let (reply, response) = oneshot::channel();
                        if sender
                            .send(LocalRequest { path, headers: local_headers, reply })
                            .await
                            .is_err()
                        {
                            return Err(UsError::Transport(
                                "in-process US transport closed".to_string(),
                            ));
                        }
                        response.await.map_err(|_| {
                            UsError::Transport("in-process US reply dropped".to_string())
                        })
                    };
                    match tokio::time::timeout(local_timeout(), exchange).await {
                        Ok(result) => result?,
                        Err(_) => {
                            return Err(UsError::Transport(format!(
                                "in-process US timeout after {}s",
                                local_timeout().as_secs()
                            )))
                        }
                    }
                }
            };
            let status = StatusCode::from_u16(response.status)
                .map_err(|error| UsError::Transport(error.to_string()))?;
            let content_type = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.to_ascii_lowercase())
                .unwrap_or_default();
            (status, content_type, bytes::Bytes::from(response.body))
        } else {
            let response = self
                .http
                .get(&url)
                .headers(headers)
                .send()
                .await
                .map_err(|error| UsError::Transport(describe_reqwest(&error)))?;
            let status = response.status();
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| UsError::Transport(describe_reqwest(&error)))?;
            (status, content_type, bytes)
        };
        if status == StatusCode::NOT_FOUND {
            return Err(UsError::NotFound {
                entry_id: entry_id.to_string(),
            });
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(UsError::Unauthorized {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            return Err(UsError::Status {
                status: status.as_u16(),
                body: truncate(&body, 512),
            });
        }
        if content_type.contains("application/msgpack") {
            rmp_serde::from_slice(&bytes).map_err(|error| UsError::Decode {
                entry_id: entry_id.to_string(),
                reason: format!("msgpack: {error}"),
            })
        } else {
            serde_json::from_slice(&bytes).map_err(|error| UsError::Decode {
                entry_id: entry_id.to_string(),
                reason: error.to_string(),
            })
        }
    }
}

/// Percent-encode a path segment or a query value (`entry ids` are UUIDs, but ids of
/// workbooks are not guaranteed to stay that way).
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn truncate(raw: &str, limit: usize) -> String {
    if raw.len() <= limit {
        raw.to_string()
    } else {
        format!("{}...", &raw[..limit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockUs;

    #[tokio::test]
    async fn an_entry_is_fetched_by_id_with_the_master_token() {
        let us = MockUs::with_dataset().await;
        let client = UsClient::new(us.base_url())
            .unwrap()
            .with_master_token("AC1ofiek8coB");
        let entry = client.get_entry("dataset-1").await.unwrap();
        assert_eq!(entry.entry_id, "dataset-1");
        assert_eq!(entry.scope, "dataset");

        let requests = us.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].entry_id, "dataset-1");
        assert_eq!(requests[0].branch.as_deref(), Some("published"));
        assert_eq!(requests[0].master_token.as_deref(), Some("AC1ofiek8coB"));
        assert_eq!(requests[0].authorization, None);
        assert_eq!(requests[0].component.as_deref(), Some("backend"));
    }

    #[tokio::test]
    async fn the_callers_token_is_forwarded_and_the_branch_can_be_chosen() {
        let us = MockUs::with_dataset().await;
        let client = UsClient::new(us.base_url())
            .unwrap()
            .with_master_token("master")
            .with_branch("saved");
        client.get_entry_as("dataset-1", "user-jwt").await.unwrap();

        let requests = us.requests();
        assert_eq!(requests[0].branch.as_deref(), Some("saved"));
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer user-jwt")
        );
        assert_eq!(requests[0].master_token, None);
    }

    #[tokio::test]
    async fn a_missing_entry_is_a_404_the_api_can_return_as_is() {
        let us = MockUs::start(Default::default()).await;
        let client = UsClient::new(us.base_url()).unwrap();
        let error = client.get_entry("nope").await.unwrap_err();
        assert_eq!(error.status(), 404);
        assert_eq!(error.code(), "ERR.DS_API.DB.DATASET_NOT_FOUND");
        // `published` first, then `saved`: an unpublished object is not a missing one.
        let requests = us.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].branch.as_deref(), Some("saved"));
    }

    #[tokio::test]
    async fn an_unpublished_entry_is_re_read_from_the_saved_branch() {
        // A connection that was saved but never published: `us` answers `data: null` on the
        // `published` branch, so the client has to ask the branch the object lives in.
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            "conn-1".to_string(),
            serde_json::json!({
                "entryId": "conn-1",
                "scope": "connection",
                "type": "postgres",
                "publishedId": null,
                "data": null,
            }),
        );
        let us = MockUs::start(entries).await;
        let client = UsClient::new(us.base_url()).unwrap();
        client.get_entry("conn-1").await.unwrap();

        let requests = us.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].branch.as_deref(), Some("published"));
        assert_eq!(requests[1].branch.as_deref(), Some("saved"));
    }

    #[tokio::test]
    async fn a_dataset_is_decoded_straight_from_the_entry() {
        let us = MockUs::with_dataset().await;
        let client = UsClient::new(us.base_url()).unwrap();
        let dataset = client.get_dataset("dataset-1").await.unwrap();
        assert_eq!(dataset.sources[0].table, "public.orders");
        assert_eq!(
            dataset
                .field_by_guid("dim_region")
                .unwrap()
                .physical_column(),
            "region"
        );
    }

    #[test]
    fn ids_are_percent_encoded_in_the_path() {
        assert_eq!(percent_encode("dataset-1"), "dataset-1");
        assert_eq!(percent_encode("a/b c"), "a%2Fb%20c");
    }
}
