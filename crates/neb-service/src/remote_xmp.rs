//! Remote `.xmp` sidecar read/write sync over WebDAV and S3.
//!
//! Original media files remain strictly read-only. Sidecars live beside each
//! remote asset (`<stem>.xmp` or an existing companion `.xmp`) and preserve
//! third-party Camera Raw (`crs:*`) and Dublin Core tags on every update.
use super::*;
use neb_core::{xmp, Asset};
use neb_storage::{RemoteWriter, StorageBrowser};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Default)]
struct QueueState {
    queue: VecDeque<String>,
    pending_keywords: HashMap<String, bool>,
    in_flight: usize,
    stop: bool,
}

/// Background queue that flushes automatic `.xmp` updates for remote assets
/// without blocking interactive rating, flagging, cropping, or keywording.
pub(crate) struct RemoteXmpSyncer {
    state: Arc<Mutex<QueueState>>,
    notify: Arc<Condvar>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl RemoteXmpSyncer {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(QueueState::default())),
            notify: Arc::new(Condvar::new()),
            worker: Mutex::new(None),
        }
    }

    fn ensure_worker(&self, core: &Arc<Core>, storage: &Arc<Mutex<StorageRegistry>>) {
        let Ok(mut slot) = self.worker.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let core = Arc::clone(core);
        let storage = Arc::clone(storage);
        let state = Arc::clone(&self.state);
        let notify = Arc::clone(&self.notify);
        if let Ok(handle) = std::thread::Builder::new()
            .name("neb-remote-xmp".into())
            .spawn(move || worker_loop(core, storage, state, notify))
        {
            *slot = Some(handle);
        }
    }

    pub(crate) fn enqueue(
        &self,
        core: &Arc<Core>,
        storage: &Arc<Mutex<StorageRegistry>>,
        items: Vec<(String, bool)>,
    ) {
        if items.is_empty() {
            return;
        }
        self.ensure_worker(core, storage);
        let Ok(mut guard) = self.state.lock() else {
            return;
        };
        if guard.stop {
            return;
        }
        for (id, force_keywords) in items {
            let already_queued = guard.pending_keywords.contains_key(&id);
            let entry = guard.pending_keywords.entry(id.clone()).or_insert(false);
            *entry |= force_keywords;
            if !already_queued {
                guard.queue.push_back(id);
            }
        }
        self.notify.notify_all();
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.state
            .lock()
            .map(|guard| guard.queue.len() + guard.in_flight)
            .unwrap_or(0)
    }

    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let Ok(mut guard) = self.state.lock() else {
            return false;
        };
        while guard.queue.len() + guard.in_flight > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let Ok((next, _)) = self.notify.wait_timeout(guard, deadline - now) else {
                return false;
            };
            guard = next;
        }
        true
    }

    pub(crate) fn shutdown(&self) {
        if let Ok(mut guard) = self.state.lock() {
            guard.stop = true;
            self.notify.notify_all();
        }
        if let Ok(mut slot) = self.worker.lock() {
            if let Some(handle) = slot.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for RemoteXmpSyncer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(
    core: Arc<Core>,
    storage: Arc<Mutex<StorageRegistry>>,
    state: Arc<Mutex<QueueState>>,
    notify: Arc<Condvar>,
) {
    let mut writers: HashMap<String, RemoteWriter> = HashMap::new();
    loop {
        let next_item = {
            let Ok(mut guard) = state.lock() else {
                return;
            };
            while guard.queue.is_empty() && !guard.stop {
                writers.clear();
                guard = match notify.wait(guard) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
            if guard.stop && guard.queue.is_empty() {
                return;
            }
            let id = guard.queue.pop_front().expect("checked non-empty");
            let force_keywords = guard.pending_keywords.remove(&id).unwrap_or(false);
            guard.in_flight += 1;
            (id, force_keywords)
        };

        let (asset_id, force_keywords) = next_item;
        if core.xmp_auto_write().unwrap_or(false) {
            if let Ok(asset) = core.asset(&asset_id) {
                if asset.source_key.is_some() {
                    if let Ok(library) = core.library(&asset.library_id) {
                        if let Some(storage_id) = library.storage_id.as_deref() {
                            if !writers.contains_key(storage_id) {
                                if let Ok(writer) = storage
                                    .lock()
                                    .map_err(|_| anyhow!("Storage state unavailable"))
                                    .and_then(|reg| reg.open_writer(storage_id))
                                {
                                    writers.insert(storage_id.to_owned(), writer);
                                }
                            }
                            if let Some(writer) = writers.get(storage_id) {
                                let _ = sync_remote_asset_xmp(writer, &asset, force_keywords);
                            }
                        }
                    }
                }
            }
        }

        if let Ok(mut guard) = state.lock() {
            guard.in_flight = guard.in_flight.saturating_sub(1);
            if guard.queue.is_empty() && guard.in_flight == 0 {
                notify.notify_all();
            }
        }
    }
}

fn parent_dir_of_key(key: &str) -> &str {
    key.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn index_remote_dir_sidecars(
    browser: &StorageBrowser,
    parent_dir: &str,
    max_pages: usize,
) -> Option<HashMap<String, String>> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    let mut sidecars = HashMap::new();
    for _ in 0..max_pages {
        let page = browser.browse(parent_dir, 1000, cursor.as_deref()).ok()?;
        for entry in page.entries {
            if entry.is_dir {
                continue;
            }
            let is_xmp = Path::new(&entry.key)
                .extension()
                .and_then(|v| v.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("xmp"));
            if is_xmp {
                sidecars.insert(entry.key.to_ascii_lowercase(), entry.key);
            }
        }
        match page.next_cursor {
            None => return Some(sidecars),
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return None;
                }
                cursor = Some(next);
            }
        }
    }
    None
}

fn match_indexed_sidecar(
    dir_sidecars: &HashMap<String, String>,
    source_key: &str,
) -> Option<String> {
    let candidates = xmp::sidecar_candidates_for_key(source_key)?;
    candidates
        .into_iter()
        .find_map(|cand| dir_sidecars.get(&cand.to_ascii_lowercase()).cloned())
}

fn find_remote_sidecar(
    writer: &RemoteWriter,
    source_key: &str,
) -> Result<Option<(String, neb_storage::RemoteSidecar)>> {
    let Some(candidates) = xmp::sidecar_candidates_for_key(source_key) else {
        return Ok(None);
    };
    for candidate in candidates {
        if let Some(sidecar) = writer.read_sidecar(&candidate, xmp::MAX_XMP_BYTES)? {
            return Ok(Some((candidate, sidecar)));
        }
    }
    Ok(None)
}

fn find_remote_sidecar_for_read(
    browser: &StorageBrowser,
    source_key: &str,
    indexed_dir: Option<&HashMap<String, String>>,
) -> Result<Option<(String, Vec<u8>)>> {
    if let Some(dir_map) = indexed_dir {
        let Some(matched_key) = match_indexed_sidecar(dir_map, source_key) else {
            return Ok(None);
        };
        return Ok(browser
            .read_sidecar(&matched_key, xmp::MAX_XMP_BYTES)?
            .map(|bytes| (matched_key, bytes)));
    }
    let Some(candidates) = xmp::sidecar_candidates_for_key(source_key) else {
        return Ok(None);
    };
    for candidate in candidates {
        if let Some(bytes) = browser.read_sidecar(&candidate, xmp::MAX_XMP_BYTES)? {
            return Ok(Some((candidate, bytes)));
        }
    }
    Ok(None)
}

pub(crate) fn sync_remote_asset_xmp(
    writer: &RemoteWriter,
    asset: &Asset,
    force_keywords: bool,
) -> Result<Option<String>> {
    sync_remote_asset_xmp_with_dir(writer, asset, force_keywords, None)
}

fn sync_remote_asset_xmp_with_dir(
    writer: &RemoteWriter,
    asset: &Asset,
    force_keywords: bool,
    indexed_dir: Option<&HashMap<String, String>>,
) -> Result<Option<String>> {
    let Some(source_key) = asset.source_key.as_deref() else {
        return Ok(None);
    };
    let existing = match indexed_dir {
        Some(dir_map) => match match_indexed_sidecar(dir_map, source_key) {
            Some(matched_key) => writer
                .read_sidecar(&matched_key, xmp::MAX_XMP_BYTES)?
                .map(|sidecar| (matched_key, sidecar)),
            None => None,
        },
        None => find_remote_sidecar(writer, source_key)?,
    };
    let target_key = match &existing {
        Some((key, _)) => key.clone(),
        None => xmp::default_sidecar_key_for_write(source_key)
            .context("E_INVALID: Remote key has no valid file stem")?,
    };
    let kw_arg = if force_keywords || !asset.keywords.is_empty() {
        Some(asset.keywords.as_slice())
    } else {
        None
    };

    let mut current_sidecar = existing.map(|(_, sidecar)| sidecar);
    for attempt in 0..2 {
        let existing_xml = match &current_sidecar {
            Some(sidecar) => {
                ensure!(
                    sidecar.bytes.len() as u64 <= xmp::MAX_XMP_BYTES,
                    "E_LIMIT: Existing XMP sidecar exceeds 2 MiB limit"
                );
                Some(
                    std::str::from_utf8(&sidecar.bytes)
                        .context("E_INVALID: Existing XMP sidecar is not valid UTF-8")?,
                )
            }
            None => None,
        };
        let Some(bytes) = xmp::prepare_sidecar_bytes(
            existing_xml,
            asset.rating,
            &asset.flag,
            Some(&asset.color_label),
            asset.crop,
            kw_arg,
        )?
        else {
            return Ok(None);
        };
        let etag = current_sidecar.as_ref().and_then(|s| s.etag.as_deref());
        let create_only = current_sidecar.is_none();
        match writer.write_sidecar(&target_key, &bytes, etag, create_only) {
            Ok(()) => return Ok(Some(target_key)),
            Err(err) if attempt == 0 && format!("{err:#}").contains("E_CONFLICT") => {
                current_sidecar = writer.read_sidecar(&target_key, xmp::MAX_XMP_BYTES)?;
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("loop returns on second attempt")
}

impl Service {
    pub(crate) fn enqueue_remote_xmp_if_enabled(&self, ids: &[String], force_keywords: bool) {
        if ids.is_empty() || !self.core.xmp_auto_write().unwrap_or(false) {
            return;
        }
        let expanded = self
            .core
            .expand_pair_members(ids)
            .unwrap_or_else(|_| ids.to_vec());
        let mut remote_items = Vec::new();
        let mut seen_pair_targets = HashSet::new();
        for id in expanded {
            if let Ok(asset) = self.core.asset(&id) {
                if let Some(source_key) = asset.source_key.as_deref() {
                    let paired = self
                        .core
                        .library(&asset.library_id)
                        .map(|lib| lib.pair_raw_jpeg)
                        .unwrap_or(false);
                    if paired {
                        if let Some(default_key) = xmp::default_sidecar_key_for_write(source_key) {
                            if !seen_pair_targets.insert((
                                asset.library_id.clone(),
                                default_key.to_ascii_lowercase(),
                            )) {
                                continue;
                            }
                        }
                    }
                    remote_items.push((id, force_keywords));
                }
            }
        }
        if !remote_items.is_empty() {
            self.remote_xmp
                .enqueue(&self.core, &self.storage, remote_items);
        }
    }

    pub fn flush_remote_xmp(&self, timeout: Duration) -> bool {
        self.remote_xmp.flush(timeout)
    }

    pub(crate) fn write_xmp_sidecars(&self, params: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct TargetArgs {
            ids: Option<Vec<String>>,
            library_id: Option<String>,
        }
        let args: TargetArgs = serde_json::from_value(params)?;
        let library_wide = args.library_id.is_some();
        let assets = self
            .core
            .resolve_xmp_assets(args.ids.as_deref(), args.library_id.as_deref())?;

        let mut local_ids = Vec::new();
        let mut remote_assets = Vec::new();
        for asset in assets {
            if asset.source_key.is_some() {
                remote_assets.push(asset);
            } else {
                local_ids.push(asset.id);
            }
        }
        if remote_assets.is_empty() {
            if local_ids.is_empty() {
                return Ok(json!({"written": 0, "skipped": 0, "errors": []}));
            }
            return self
                .core
                .dispatch("asset.xmp.write", json!({ "ids": local_ids }));
        }

        // Drain pending auto-write items first so background writes never race manual sync.
        let _ = self.remote_xmp.flush(Duration::from_secs(10));

        if library_wide {
            // In remote RAW+JPEG paired libraries, per-file marks live on the rated variant
            // (typically the raster representative in the grid). Process marked / raster items
            // before unmarked RAW siblings so shared `<stem>.xmp` sidecars are never clobbered.
            let has_marks = |a: &Asset| {
                a.rating > 0
                    || a.flag != "none"
                    || a.color_label != "none"
                    || a.crop.is_some()
                    || !a.keywords.is_empty()
            };
            remote_assets.sort_by_key(|a| (!has_marks(a), a.kind == "raw"));
        }

        let mut written = 0usize;
        let mut skipped = 0usize;
        let mut failed: Vec<Value> = Vec::new();
        if !local_ids.is_empty() {
            let local_report = self
                .core
                .dispatch("asset.xmp.write", json!({ "ids": local_ids }))?;
            written += local_report["written"].as_u64().unwrap_or(0) as usize;
            skipped += local_report["skipped"].as_u64().unwrap_or(0) as usize;
            if let Some(errs) = local_report["errors"].as_array() {
                failed.extend(errs.iter().cloned());
            }
        }

        let mut writers: HashMap<String, Result<RemoteWriter, String>> = HashMap::new();
        let mut browsers: HashMap<String, Option<StorageBrowser>> = HashMap::new();
        let mut lib_storage: HashMap<String, Option<String>> = HashMap::new();
        let mut dir_counts: HashMap<(String, String), usize> = HashMap::new();
        for asset in &remote_assets {
            let storage_id = match lib_storage.get(&asset.library_id) {
                Some(cached) => cached.clone(),
                None => {
                    let resolved = self
                        .core
                        .library(&asset.library_id)
                        .ok()
                        .and_then(|lib| lib.storage_id);
                    lib_storage.insert(asset.library_id.clone(), resolved.clone());
                    resolved
                }
            };
            if let (Some(storage_id), Some(source_key)) = (storage_id, asset.source_key.as_deref())
            {
                let parent = parent_dir_of_key(source_key).to_owned();
                *dir_counts.entry((storage_id, parent)).or_insert(0) += 1;
            }
        }

        let mut dir_indexes: HashMap<(String, String), Option<HashMap<String, String>>> =
            HashMap::new();
        let mut seen_sidecars: HashSet<(String, String)> = HashSet::new();

        for asset in remote_assets {
            let source_key = asset.source_key.as_deref().unwrap_or_default().to_owned();
            let Some(storage_id) = lib_storage.get(&asset.library_id).cloned().flatten() else {
                failed.push(json!({
                    "id": asset.id,
                    "path": source_key,
                    "message": "E_NOT_FOUND: Remote library has no storage source",
                }));
                continue;
            };
            let parent = parent_dir_of_key(&source_key).to_owned();
            let dir_key = (storage_id.clone(), parent.clone());
            let count = dir_counts.get(&dir_key).copied().unwrap_or(0);
            let indexed_dir = if library_wide || count >= 2 {
                dir_indexes
                    .entry(dir_key)
                    .or_insert_with(|| {
                        let browser = browsers
                            .entry(storage_id.clone())
                            .or_insert_with(|| {
                                self.storage
                                    .lock()
                                    .ok()
                                    .and_then(|reg| reg.open_browser(&storage_id).ok())
                            })
                            .as_ref()?;
                        let max_pages = if library_wide || count >= 4 { 64 } else { 2 };
                        index_remote_dir_sidecars(browser, &parent, max_pages)
                    })
                    .as_ref()
            } else {
                None
            };

            let dedup_target = indexed_dir
                .and_then(|map| match_indexed_sidecar(map, &source_key))
                .or_else(|| xmp::default_sidecar_key_for_write(&source_key));
            if let Some(target_key) = dedup_target {
                if !seen_sidecars.insert((storage_id.clone(), target_key.to_ascii_lowercase())) {
                    skipped += 1;
                    continue;
                }
            }

            let writer_res = writers.entry(storage_id.clone()).or_insert_with(|| {
                self.storage
                    .lock()
                    .map_err(|_| "Storage state unavailable".to_owned())
                    .and_then(|reg| reg.open_writer(&storage_id).map_err(|e| format!("{e:#}")))
            });
            let writer = match writer_res {
                Ok(writer) => writer,
                Err(message) => {
                    failed.push(json!({
                        "id": asset.id,
                        "path": source_key,
                        "message": message,
                    }));
                    continue;
                }
            };
            match sync_remote_asset_xmp_with_dir(writer, &asset, true, indexed_dir) {
                Ok(Some(_)) => written += 1,
                Ok(None) => skipped += 1,
                Err(error) => failed.push(json!({
                    "id": asset.id,
                    "path": source_key,
                    "message": format!("{error:#}"),
                })),
            }
        }

        Ok(json!({
            "written": written,
            "skipped": skipped,
            "errors": failed,
        }))
    }

    pub(crate) fn read_xmp_sidecars(&self, params: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct TargetArgs {
            ids: Option<Vec<String>>,
            library_id: Option<String>,
        }
        let args: TargetArgs = serde_json::from_value(params)?;
        let library_wide = args.library_id.is_some();
        let assets = self
            .core
            .resolve_xmp_assets(args.ids.as_deref(), args.library_id.as_deref())?;

        let mut local_ids = Vec::new();
        let mut remote_assets = Vec::new();
        for asset in assets {
            if asset.source_key.is_some() {
                remote_assets.push(asset);
            } else {
                local_ids.push(asset.id);
            }
        }
        if remote_assets.is_empty() {
            if local_ids.is_empty() {
                return Ok(json!({"updated": 0, "skipped": 0, "errors": []}));
            }
            let mut value = self
                .core
                .dispatch("asset.xmp.read", json!({ "ids": local_ids }))?;
            if let Some(obj) = value.as_object_mut() {
                obj.remove("crop_changed_ids");
            }
            return Ok(value);
        }

        // Drain pending auto-write items first so stale remote sidecars do not clobber fresh marks.
        let _ = self.remote_xmp.flush(Duration::from_secs(10));

        let mut updated = 0usize;
        let mut skipped = 0usize;
        let mut failed: Vec<Value> = Vec::new();
        if !local_ids.is_empty() {
            let local_report = self
                .core
                .dispatch("asset.xmp.read", json!({ "ids": local_ids }))?;
            updated += local_report["updated"].as_u64().unwrap_or(0) as usize;
            skipped += local_report["skipped"].as_u64().unwrap_or(0) as usize;
            if let Some(errs) = local_report["errors"].as_array() {
                failed.extend(errs.iter().cloned());
            }
        }

        let mut browsers: HashMap<String, Result<StorageBrowser, String>> = HashMap::new();
        let mut lib_storage: HashMap<String, Option<String>> = HashMap::new();
        let mut dir_counts: HashMap<(String, String), usize> = HashMap::new();
        for asset in &remote_assets {
            let storage_id = match lib_storage.get(&asset.library_id) {
                Some(cached) => cached.clone(),
                None => {
                    let resolved = self
                        .core
                        .library(&asset.library_id)
                        .ok()
                        .and_then(|lib| lib.storage_id);
                    lib_storage.insert(asset.library_id.clone(), resolved.clone());
                    resolved
                }
            };
            if let (Some(storage_id), Some(source_key)) = (storage_id, asset.source_key.as_deref())
            {
                let parent = parent_dir_of_key(source_key).to_owned();
                *dir_counts.entry((storage_id, parent)).or_insert(0) += 1;
            }
        }

        let mut dir_indexes: HashMap<(String, String), Option<HashMap<String, String>>> =
            HashMap::new();
        let mut sidecar_cache: HashMap<(String, String), Vec<u8>> = HashMap::new();
        let mut pending_updates = Vec::new();

        for asset in remote_assets {
            let source_key = asset.source_key.as_deref().unwrap_or_default().to_owned();
            let Some(storage_id) = lib_storage.get(&asset.library_id).cloned().flatten() else {
                failed.push(json!({
                    "id": asset.id,
                    "path": source_key,
                    "message": "E_NOT_FOUND: Remote library has no storage source",
                }));
                continue;
            };
            let browser_res = browsers.entry(storage_id.clone()).or_insert_with(|| {
                self.storage
                    .lock()
                    .map_err(|_| "Storage state unavailable".to_owned())
                    .and_then(|reg| reg.open_browser(&storage_id).map_err(|e| format!("{e:#}")))
            });
            let browser = match browser_res {
                Ok(browser) => browser,
                Err(message) => {
                    failed.push(json!({
                        "id": asset.id,
                        "path": source_key,
                        "message": message,
                    }));
                    continue;
                }
            };
            let parent = parent_dir_of_key(&source_key).to_owned();
            let dir_key = (storage_id.clone(), parent.clone());
            let count = dir_counts.get(&dir_key).copied().unwrap_or(0);
            let indexed_dir = if library_wide || count >= 2 {
                dir_indexes
                    .entry(dir_key)
                    .or_insert_with(|| {
                        let max_pages = if library_wide || count >= 4 { 64 } else { 2 };
                        index_remote_dir_sidecars(browser, &parent, max_pages)
                    })
                    .as_ref()
            } else {
                None
            };

            let loaded = if let Some(matched_key) =
                indexed_dir.and_then(|map| match_indexed_sidecar(map, &source_key))
            {
                let cache_key = (storage_id.clone(), matched_key.clone());
                if let Some(cached) = sidecar_cache.get(&cache_key) {
                    Ok(Some((matched_key, cached.clone())))
                } else {
                    match browser.read_sidecar(&matched_key, xmp::MAX_XMP_BYTES) {
                        Ok(Some(bytes)) => {
                            sidecar_cache.insert(cache_key, bytes.clone());
                            Ok(Some((matched_key, bytes)))
                        }
                        Ok(None) => Ok(None),
                        Err(e) => Err(e),
                    }
                }
            } else {
                find_remote_sidecar_for_read(browser, &source_key, indexed_dir)
            };

            match loaded {
                Ok(Some((_, bytes))) => match xmp::parse_xmp_bytes(&bytes) {
                    Ok(sidecar) => {
                        if let Some(update) = neb_core::compute_xmp_update(&asset, sidecar) {
                            pending_updates.push(update);
                        } else {
                            skipped += 1;
                        }
                    }
                    Err(error) => failed.push(json!({
                        "id": asset.id,
                        "path": source_key,
                        "message": format!("{error:#}"),
                    })),
                },
                Ok(None) => skipped += 1,
                Err(error) => failed.push(json!({
                    "id": asset.id,
                    "path": source_key,
                    "message": format!("{error:#}"),
                })),
            }
        }

        let (remote_updated, _) = self.core.apply_xmp_updates(pending_updates)?;
        updated += remote_updated;

        Ok(json!({
            "updated": updated,
            "skipped": skipped,
            "errors": failed,
        }))
    }
}
