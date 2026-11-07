//! Per-instance, explicitly refreshed docs and OpenAPI intelligence.

mod docs;
mod openapi;

use crate::mcp::upstream::{ToolApiClient, UpstreamPolicy};
use lantern_core::{
    Timestamp,
    api::{ApiMeta, DiscoveryMeta, DocContent, DocsMeta, IntelError, IntelRefreshReport, IntelSnapshot},
    ids::InstanceId,
    manifest::ResolvedManifest,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, Semaphore};

/// The controller supplies the resolved manifest and checkout for each refresh.
#[derive(Debug, Clone)]
pub struct IntelInput {
    /// Instance whose checkout and server are being indexed.
    pub instance: InstanceId,
    /// Checkout's project directory.
    pub checkout: PathBuf,
    /// Manifest resolved specifically for this checkout.
    pub manifest: Arc<ResolvedManifest>,
    /// Registered instance upstream/base address, selected by the controller.
    pub base_url: Option<String>,
    /// Whether candidate OpenAPI endpoints may be probed.
    pub running: bool,
}
#[derive(Clone)]
struct Record {
    snapshot: Arc<IntelSnapshot>,
    root: Option<PathBuf>,
    manifest: Option<Arc<ResolvedManifest>>,
    max_bytes: u64,
}
struct Inner {
    records: RwLock<HashMap<InstanceId, Record>>,
    per_instance: Mutex<HashMap<InstanceId, Arc<Mutex<()>>>>,
    permits: Semaphore,
    cache_path: PathBuf,
    cache_enabled: bool,
    max_openapi_bytes: usize,
    save_lock: Mutex<Option<Instant>>,
    policy: UpstreamPolicy,
    client: Arc<dyn ToolApiClient>,
}
/// An injected-path, per-instance intel service; reads do not initiate refreshes.
#[derive(Clone)]
pub struct Intel {
    inner: Arc<Inner>,
}
impl std::fmt::Debug for Intel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Intel")
            .field("cache_path", &self.inner.cache_path)
            .finish_non_exhaustive()
    }
}
impl Intel {
    /// Construct a service with an explicit disposable cache path and upstream policy.
    pub fn new(cache_path: PathBuf, policy: UpstreamPolicy, client: Arc<dyn ToolApiClient>) -> Self {
        Self {
            inner: Arc::new(Inner {
                records: RwLock::new(HashMap::new()),
                per_instance: Mutex::new(HashMap::new()),
                permits: Semaphore::new(4),
                cache_path,
                cache_enabled: true,
                max_openapi_bytes: 5 * 1024 * 1024,
                save_lock: Mutex::new(None),
                policy,
                client,
            }),
        }
    }
    /// Configure cache persistence and the maximum OpenAPI response size before cloning.
    pub fn with_options(mut self, cache_enabled: bool, max_openapi_bytes: usize) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("configure intel before cloning");
        inner.cache_enabled = cache_enabled;
        inner.max_openapi_bytes = max_openapi_bytes.max(1);
        self
    }
    /// Return a cached snapshot only, with no I/O or refresh.
    pub async fn snapshot(&self, id: &InstanceId) -> Option<Arc<IntelSnapshot>> {
        self.inner
            .records
            .read()
            .await
            .get(id)
            .map(|r| r.snapshot.clone())
    }
    /// Check whether the cached docs were scanned from this exact checkout and manifest.
    pub async fn matches_manifest(
        &self,
        id: &InstanceId,
        checkout: &std::path::Path,
        manifest: &ResolvedManifest,
    ) -> bool {
        self.inner.records.read().await.get(id).is_some_and(|record| {
            record.root.as_deref() == Some(checkout) && record.manifest.as_deref() == Some(manifest)
        })
    }
    /// Refresh one instance; independent instances can refresh concurrently, at most four at a time.
    pub async fn refresh(&self, input: IntelInput) -> Result<Arc<IntelSnapshot>, String> {
        tokio::time::timeout(Duration::from_secs(30), self.refresh_inner(input))
            .await
            .map_err(|_| "intel refresh timed out".to_string())?
    }
    async fn refresh_inner(&self, input: IntelInput) -> Result<Arc<IntelSnapshot>, String> {
        let lock = {
            let mut locks = self.inner.per_instance.lock().await;
            locks
                .entry(input.instance.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _instance = lock.lock().await;
        let _permit = self
            .inner
            .permits
            .acquire()
            .await
            .map_err(|_| "intel is shutting down")?;
        let previous = self.snapshot(&input.instance).await;
        let root = input.checkout.clone();
        let manifest = input.manifest.clone();
        let (docs, docs_meta, max_bytes) = tokio::task::spawn_blocking(move || docs::scan(&root, &manifest))
            .await
            .map_err(|e| e.to_string())??;
        let (endpoints, api_meta, stale) = openapi::scan(openapi::Input {
            root: &input.checkout,
            manifest: &input.manifest,
            base: input.base_url.as_deref(),
            running: input.running,
            previous: previous.as_ref().map_or(&[], |p| p.endpoints.as_slice()),
            policy: &self.inner.policy,
            client: self.inner.client.as_ref(),
            max_bytes: self.inner.max_openapi_bytes,
        })
        .await;
        let now = Timestamp::now();
        let snapshot = Arc::new(IntelSnapshot {
            instance: input.instance.clone(),
            refreshed_at: Some(now),
            stale,
            docs,
            endpoints,
            discovery: DiscoveryMeta {
                refreshed_at: Some(now),
                docs: docs_meta,
                api: api_meta,
            },
        });
        self.inner.records.write().await.insert(
            input.instance,
            Record {
                snapshot: snapshot.clone(),
                root: Some(input.checkout),
                manifest: Some(input.manifest),
                max_bytes,
            },
        );
        // Cache failure is surfaced to the controller, but does not discard a good in-memory refresh.
        self.save_if_due().await?;
        Ok(snapshot)
    }
    /// Refresh a set of instances with a 30-second overall limit.
    pub async fn refresh_all(&self, inputs: Vec<IntelInput>) -> IntelRefreshReport {
        use futures_util::{StreamExt, stream::FuturesUnordered};
        let start = Instant::now();
        let mut pending = FuturesUnordered::new();
        for input in inputs {
            let this = self.clone();
            pending.push(async move {
                let id = input.instance.clone();
                (id, this.refresh(input).await)
            });
        }
        let mut refreshed = 0;
        let mut errors = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, pending.next()).await {
                Ok(Some((id, Ok(_)))) => {
                    let _ = id;
                    refreshed += 1
                }
                Ok(Some((id, Err(error)))) => errors.push(IntelError {
                    source: id.to_string(),
                    error,
                }),
                Ok(None) => break,
                Err(_) => {
                    errors.push(IntelError {
                        source: "refresh_all".into(),
                        error: "timed out".into(),
                    });
                    break;
                }
            }
        }
        IntelRefreshReport {
            refreshed,
            errors,
            took_ms: start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        }
    }
    /// Read a listed document through a no-symlink, checkout-contained file walk.
    pub async fn read_doc(
        &self,
        id: &InstanceId,
        path: &str,
        offset: u64,
        max_chars: usize,
    ) -> Result<DocContent, String> {
        let record = self
            .inner
            .records
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or("no cached intel; refresh first")?;
        let root = record.root.ok_or("docs are not loaded; refresh first")?;
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            docs::read(
                &root,
                &record.snapshot.docs,
                &path,
                offset,
                max_chars,
                record.max_bytes,
            )
        })
        .await
        .map_err(|e| e.to_string())?
    }
    /// Remove a cached instance after it is unregistered.
    pub async fn forget(&self, id: &InstanceId) {
        self.inner.records.write().await.remove(id);
    }
    async fn save_if_due(&self) -> Result<(), String> {
        let mut last = self.inner.save_lock.lock().await;
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return Ok(());
        }
        self.write_cache().await?;
        *last = Some(Instant::now());
        Ok(())
    }
    /// Atomically write endpoint-only cache data to the injected path.
    pub async fn write_cache(&self) -> Result<(), String> {
        if !self.inner.cache_enabled {
            return Ok(());
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let records = self.inner.records.read().await;
        let instances:BTreeMap<_,_>=records.iter().map(|(id,r)|(id.to_string(),json!({"refreshed_at":r.snapshot.refreshed_at,"endpoints":r.snapshot.endpoints,"api_meta":r.snapshot.discovery.api}))).collect();
        let bytes =
            serde_json::to_vec(&json!({"version":1,"instances":instances})).map_err(|e| e.to_string())?;
        let path = &self.inner.cache_path;
        let parent = path.parent().ok_or("cache has no parent directory")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temp = parent.join(format!(
            ".intel-cache-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let outcome = (|| -> Result<(), String> {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|e| e.to_string())?;
            file.write_all(&bytes).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&temp, path).map_err(|e| e.to_string())?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(temp);
        }
        outcome
    }
    /// Restore only endpoint data for currently registered ids; restored snapshots are stale.
    pub async fn load_cache(&self, valid: &[InstanceId]) -> Result<usize, String> {
        if !self.inner.cache_enabled {
            return Ok(0);
        }
        let bytes = match fs::read(&self.inner.cache_path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.to_string()),
        };
        let cache: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if cache["version"].as_u64() != Some(1) {
            return Ok(0);
        }
        let valid: HashSet<_> = valid.iter().map(InstanceId::as_str).collect();
        let mut records = self.inner.records.write().await;
        let mut count = 0;
        if let Some(instances) = cache["instances"].as_object() {
            for (id, v) in instances {
                if !valid.contains(id.as_str()) {
                    continue;
                }
                let Ok(id) = InstanceId::parse(id) else { continue };
                let Ok(endpoints) = serde_json::from_value(v["endpoints"].clone()) else {
                    continue;
                };
                let Ok(api) = serde_json::from_value::<ApiMeta>(v["api_meta"].clone()) else {
                    continue;
                };
                let refreshed_at = serde_json::from_value(v["refreshed_at"].clone()).ok().flatten();
                let docs = DocsMeta {
                    enabled: false,
                    count: 0,
                    source_count: 0,
                    sources: Vec::new(),
                    skipped_large: 0,
                    truncated: false,
                };
                let snapshot = IntelSnapshot {
                    instance: id.clone(),
                    refreshed_at,
                    stale: true,
                    docs: Vec::new(),
                    endpoints,
                    discovery: DiscoveryMeta {
                        refreshed_at,
                        docs,
                        api,
                    },
                };
                records.insert(
                    id,
                    Record {
                        snapshot: Arc::new(snapshot),
                        root: None,
                        manifest: None,
                        max_bytes: 1_048_576,
                    },
                );
                count += 1;
            }
        }
        Ok(count)
    }
}
