//! Where an owner's bundle is kept between tasks.
//!
//! One trait, two implementations: a directory for tests and local runs, and
//! Cloud Storage for deployments. The Cloud Storage store is one bucket per
//! deployment with one prefix per owner, and relies on the bucket's object
//! versioning for history: an overwritten or deleted file remains a past
//! version, reachable by someone who looks for it and by nobody who does not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::bundle::{Bundle, Changes};

/// An owner's memory, loaded whole and saved as a set of changes.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Every file of the owner. An owner with no memory yet has an empty bundle.
    async fn load(&self, owner: &str) -> Result<Bundle>;

    /// Writes and deletes, each file independently; a failure part way leaves
    /// the files already written in place, which the next load reflects.
    async fn save(&self, owner: &str, changes: &Changes) -> Result<()>;
}

impl std::fmt::Debug for dyn MemoryStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MemoryStore")
    }
}

/// The owner key as a path segment: it already contains only letters, digits,
/// a colon and hyphens, and the colon is replaced so it works everywhere.
pub fn owner_segment(owner: &str) -> String {
    owner.replace(':', "_")
}

/// A directory per owner under a root, for tests and local runs.
pub struct LocalMemoryStore {
    root: PathBuf,
}

impl LocalMemoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn owner_root(&self, owner: &str) -> PathBuf {
        self.root.join(owner_segment(owner))
    }
}

#[async_trait]
impl MemoryStore for LocalMemoryStore {
    async fn load(&self, owner: &str) -> Result<Bundle> {
        let root = self.owner_root(owner);
        let mut files = BTreeMap::new();
        if !root.exists() {
            return Ok(Bundle::new());
        }
        let mut pending = vec![root.clone()];
        while let Some(directory) = pending.pop() {
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if entry.file_type().await?.is_dir() {
                    pending.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(&root)
                    .context("file outside the owner root")?
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                files.insert(relative, tokio::fs::read_to_string(&path).await?);
            }
        }
        Ok(Bundle::from_files(files))
    }

    async fn save(&self, owner: &str, changes: &Changes) -> Result<()> {
        let root = self.owner_root(owner);
        for (path, content) in &changes.put {
            let full = safe_join(&root, path)?;
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&full, content).await?;
        }
        for path in &changes.delete {
            let full = safe_join(&root, path)?;
            match tokio::fs::remove_file(&full).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("invalid memory path {relative:?}");
    }
    Ok(root.join(relative))
}

/// Where the Google access token comes from: the metadata server through
/// Workload Identity, or a fixed token for a local run.
pub enum GoogleToken {
    Metadata,
    Static(Arc<str>),
}

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// One Cloud Storage bucket, one prefix per owner, object versioning as history.
pub struct GcsMemoryStore {
    client: Client,
    api: String,
    bucket: String,
    token: GoogleToken,
    cached: Mutex<Option<(String, Instant)>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    items: Vec<ListedObject>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct ListedObject {
    name: String,
}

impl GcsMemoryStore {
    /// A store on `bucket`. `api` is `https://storage.googleapis.com` in
    /// deployments and a local fake in tests.
    pub fn new(api: &str, bucket: &str, token: GoogleToken) -> Result<Self> {
        if bucket.trim().is_empty() {
            bail!("memory bucket must not be empty");
        }
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            drop(rustls::crypto::ring::default_provider().install_default());
        }
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()?,
            api: api.trim_end_matches('/').to_owned(),
            bucket: bucket.to_owned(),
            token,
            cached: Mutex::new(None),
        })
    }

    fn prefix(owner: &str) -> String {
        format!("owners/{}/", owner_segment(owner))
    }

    fn object_name(owner: &str, path: &str) -> String {
        format!("{}{path}", Self::prefix(owner))
    }

    async fn access_token(&self) -> Result<String> {
        match &self.token {
            GoogleToken::Static(token) => Ok(token.to_string()),
            GoogleToken::Metadata => {
                let mut cached = self.cached.lock().await;
                if let Some((token, expires)) = cached.as_ref()
                    && Instant::now() + TOKEN_REFRESH_MARGIN < *expires
                {
                    return Ok(token.clone());
                }
                let response: TokenResponse = self
                    .client
                    .get(METADATA_TOKEN_URL)
                    .header("Metadata-Flavor", "Google")
                    .send()
                    .await
                    .context("metadata server unreachable")?
                    .error_for_status()
                    .context("metadata server refused the token request")?
                    .json()
                    .await
                    .context("metadata server returned an invalid token")?;
                let expires = Instant::now() + Duration::from_secs(response.expires_in);
                *cached = Some((response.access_token.clone(), expires));
                Ok(response.access_token)
            }
        }
    }

    async fn authorized(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let token = self.access_token().await?;
        request
            .bearer_auth(token)
            .send()
            .await
            .context("Cloud Storage request failed")
    }
}

#[async_trait]
impl MemoryStore for GcsMemoryStore {
    async fn load(&self, owner: &str) -> Result<Bundle> {
        let prefix = Self::prefix(owner);
        let mut names = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut request = self
                .client
                .get(format!("{}/storage/v1/b/{}/o", self.api, self.bucket))
                .query(&[
                    ("prefix", prefix.as_str()),
                    ("fields", "items(name),nextPageToken"),
                ]);
            if let Some(token) = &page_token {
                request = request.query(&[("pageToken", token.as_str())]);
            }
            let response = self.authorized(request).await?;
            if !response.status().is_success() {
                bail!("Cloud Storage listing failed: {}", response.status());
            }
            let listing: Listing = response
                .json()
                .await
                .context("invalid Cloud Storage listing")?;
            names.extend(listing.items.into_iter().map(|object| object.name));
            match listing.next_page_token {
                Some(token) => page_token = Some(token),
                None => break,
            }
        }
        let mut files = BTreeMap::new();
        for name in names {
            let Some(path) = name.strip_prefix(&prefix) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            let response = self
                .authorized(
                    self.client
                        .get(format!(
                            "{}/storage/v1/b/{}/o/{}",
                            self.api,
                            self.bucket,
                            percent_encode(&name)
                        ))
                        .query(&[("alt", "media")]),
                )
                .await?;
            if response.status() == StatusCode::NOT_FOUND {
                continue;
            }
            if !response.status().is_success() {
                bail!("Cloud Storage read of {name} failed: {}", response.status());
            }
            files.insert(path.to_owned(), response.text().await?);
        }
        Ok(Bundle::from_files(files))
    }

    async fn save(&self, owner: &str, changes: &Changes) -> Result<()> {
        for (path, content) in &changes.put {
            let name = Self::object_name(owner, path);
            let response = self
                .authorized(
                    self.client
                        .post(format!(
                            "{}/upload/storage/v1/b/{}/o",
                            self.api, self.bucket
                        ))
                        .query(&[("uploadType", "media"), ("name", name.as_str())])
                        .header("content-type", "text/markdown; charset=utf-8")
                        .body(content.clone()),
                )
                .await?;
            if !response.status().is_success() {
                bail!(
                    "Cloud Storage write of {name} failed: {}",
                    response.status()
                );
            }
        }
        for path in &changes.delete {
            let name = Self::object_name(owner, path);
            let response = self
                .authorized(self.client.delete(format!(
                    "{}/storage/v1/b/{}/o/{}",
                    self.api,
                    self.bucket,
                    percent_encode(&name)
                )))
                .await?;
            if !response.status().is_success() && response.status() != StatusCode::NOT_FOUND {
                bail!(
                    "Cloud Storage delete of {name} failed: {}",
                    response.status()
                );
            }
        }
        Ok(())
    }
}

/// Percent-encodes an object name for the path of a JSON API URL: everything
/// but unreserved characters, including the slashes inside the name.
pub fn percent_encode(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len() * 3);
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{PROFILE, fixtures::sample};

    #[tokio::test]
    async fn the_local_store_round_trips_a_bundle_and_applies_changes() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalMemoryStore::new(directory.path());
        let owner = "identity:0f1861f6-fa4b-481e-a577-30bf48d658ff";
        assert!(store.load(owner).await.unwrap().is_empty());
        let bundle = sample();
        store
            .save(owner, &Changes::between(&Bundle::new(), &bundle))
            .await
            .unwrap();
        assert_eq!(store.load(owner).await.unwrap(), bundle);
        let mut next = bundle.clone();
        next.remove("journal/2026-09-21.md");
        next.insert(PROFILE, "# Xavi\n\nRewritten 2026-09-22.\n");
        store
            .save(owner, &Changes::between(&bundle, &next))
            .await
            .unwrap();
        assert_eq!(store.load(owner).await.unwrap(), next);
        assert!(
            directory
                .path()
                .join("identity_0f1861f6-fa4b-481e-a577-30bf48d658ff")
                .exists()
        );
        assert!(
            store
                .save(
                    owner,
                    &Changes {
                        put: [("../escape.md".to_owned(), "x".to_owned())].into(),
                        delete: vec![]
                    }
                )
                .await
                .is_err()
        );
    }

    #[test]
    fn object_names_encode_every_reserved_character() {
        assert_eq!(
            percent_encode("owners/identity_abc/preferences/dining.md"),
            "owners%2Fidentity_abc%2Fpreferences%2Fdining.md"
        );
        assert_eq!(GcsMemoryStore::prefix("account:1"), "owners/account_1/");
    }

    mod fake_gcs {
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        use axum::extract::{Path, Query, State};
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use serde_json::{Value, json};

        #[derive(Clone, Default)]
        pub struct Objects(pub Arc<Mutex<BTreeMap<String, String>>>);

        pub fn router(objects: Objects) -> Router {
            Router::new()
                .route("/storage/v1/b/{bucket}/o", get(list))
                .route("/storage/v1/b/{bucket}/o/{name}", get(read).delete(remove))
                .route("/upload/storage/v1/b/{bucket}/o", post(upload))
                .with_state(objects)
        }

        async fn list(
            State(objects): State<Objects>,
            Query(query): Query<BTreeMap<String, String>>,
        ) -> Json<Value> {
            let prefix = query.get("prefix").cloned().unwrap_or_default();
            let items: Vec<Value> = objects
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|name| name.starts_with(&prefix))
                .map(|name| json!({"name": name}))
                .collect();
            Json(json!({"items": items}))
        }

        async fn read(
            State(objects): State<Objects>,
            Path((_, name)): Path<(String, String)>,
        ) -> Result<String, axum::http::StatusCode> {
            objects
                .0
                .lock()
                .unwrap()
                .get(&name)
                .cloned()
                .ok_or(axum::http::StatusCode::NOT_FOUND)
        }

        async fn remove(
            State(objects): State<Objects>,
            Path((_, name)): Path<(String, String)>,
        ) -> axum::http::StatusCode {
            objects.0.lock().unwrap().remove(&name);
            axum::http::StatusCode::NO_CONTENT
        }

        async fn upload(
            State(objects): State<Objects>,
            Query(query): Query<BTreeMap<String, String>>,
            body: String,
        ) -> Json<Value> {
            let name = query["name"].clone();
            objects.0.lock().unwrap().insert(name.clone(), body);
            Json(json!({"name": name}))
        }

        pub async fn serve(objects: Objects) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, router(objects)).await.unwrap() });
            format!("http://{address}")
        }
    }

    #[tokio::test]
    async fn the_cloud_store_lists_reads_writes_and_deletes_under_the_owner_prefix() {
        let objects = fake_gcs::Objects::default();
        let api = fake_gcs::serve(objects.clone()).await;
        let store = GcsMemoryStore::new(
            &api,
            "nanocodex-memory-test",
            GoogleToken::Static(Arc::from("token")),
        )
        .unwrap();
        let owner = "account:7c0f1b7e-1111-2222-3333-444444444444";
        assert!(store.load(owner).await.unwrap().is_empty());
        let bundle = sample();
        store
            .save(owner, &Changes::between(&Bundle::new(), &bundle))
            .await
            .unwrap();
        assert_eq!(store.load(owner).await.unwrap(), bundle);
        assert!(objects.0.lock().unwrap().contains_key(
            "owners/account_7c0f1b7e-1111-2222-3333-444444444444/preferences/dining.md"
        ));
        let mut next = bundle.clone();
        next.remove("places/home.md");
        store
            .save(owner, &Changes::between(&bundle, &next))
            .await
            .unwrap();
        assert_eq!(store.load(owner).await.unwrap(), next);
        assert!(store.load("account:other").await.unwrap().is_empty());
    }
}
