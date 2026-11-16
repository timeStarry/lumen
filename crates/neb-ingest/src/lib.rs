//! High-throughput, non-destructive local/SD-card and remote import with restart-safe journals.
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Datelike, Utc};
use neb_storage::{
    atomic_json, hash_reader, quick_fingerprint_reader, validate_key, PreparedDestination,
    RemoteWriter, ScopedDestination, ScopedSource,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};
use uuid::Uuid;

pub mod organize;
use organize::{DateBasis, ItemDate, TypeSplit, UnknownDate};
pub use organize::{OrganizeRules, RenameMode};

const CAPTURE_DATE_CONCURRENCY: usize = 6;
const FINGERPRINT_CONCURRENCY: usize = 6;
const REMOTE_UPLOAD_CONCURRENCY: usize = 4;
const EXIF_READ_BUDGET: usize = 256 * 1024;
const VIDEO_META_BUDGET: usize = 256 * 1024;
const JOURNAL_FLUSH_ITEMS: usize = 32;
const JOURNAL_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportItem {
    pub source: String,
    /// Destination path relative to the destination root; never the source key.
    pub relative_path: String,
    pub size: u64,
    pub status: String,
    pub sha256: String,
    pub date_source: String,
    pub duplicate_path: Option<String>,
    pub source_relative_path: String,
    pub modified_nanos: u128,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quick_fingerprint: String,
}

impl ImportItem {
    fn batch_token(&self) -> Option<&str> {
        if !self.quick_fingerprint.is_empty() {
            Some(&self.quick_fingerprint)
        } else if !self.sha256.is_empty() {
            Some(&self.sha256)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportPlan {
    pub id: String,
    pub source: String,
    pub destination: String,
    pub layout: String,
    /// The frozen organization rules this plan was made with.
    pub rules: OrganizeRules,
    /// Day used when `date_basis` is `import`; frozen at planning time.
    pub import_day: String,
    pub deduplicate: bool,
    pub items: Vec<ImportItem>,
    pub total_bytes: u64,
    pub duplicate_count: usize,
    pub warnings: Vec<String>,
    pub created_at: String,
    /// Remote target: the WebDAV/S3 location and the library prefix inside it.
    #[serde(default)]
    pub destination_location: Option<String>,
    #[serde(default)]
    pub destination_prefix: String,
    /// Same name, size and capture day as an indexed file whose content was never
    /// hashed. Skipped by default; reported as unverified, never as a duplicate.
    #[serde(default)]
    pub likely_duplicate_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_destination: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportProgress {
    pub plan_id: String,
    pub status: String,
    pub total_items: usize,
    pub done_items: usize,
    pub total_bytes: u64,
    pub copied_bytes: u64,
    pub current_file: Option<String>,
    pub backup_copied: usize,
    pub cancelling: bool,
}

#[derive(Default)]
pub struct ExecuteOptions<'a> {
    pub selected_sources: Option<HashSet<String>>,
    pub backup_destination: Option<PathBuf>,
    pub include_likely_duplicates: bool,
    pub progress: Option<&'a (dyn Fn(ImportProgress) + Send + Sync)>,
    pub cancel: Option<&'a AtomicBool>,
}

/// What the target library already holds, from its catalog. Planning a remote
/// import never downloads files to compare them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownAsset {
    /// Location-relative key, including the library prefix.
    pub key: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub filename: String,
    /// Camera capture time as indexed (`YYYY-MM-DDT...`), empty when unknown.
    pub captured_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemResult {
    pub source: String,
    pub destination: String,
    pub status: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportResult {
    pub id: String,
    pub plan_id: String,
    pub source: String,
    pub destination: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub total: usize,
    pub copied: usize,
    pub duplicates: usize,
    pub failed: usize,
    pub items: Vec<ItemResult>,
    pub originals_retained: bool,
    /// Rules frozen in the executed plan; absent in journals from older versions.
    #[serde(default)]
    pub rules: Option<OrganizeRules>,
    #[serde(default)]
    pub skipped_unselected: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_destination: Option<String>,
    #[serde(default)]
    pub backup_copied: usize,
    #[serde(default)]
    pub backup_failed: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backup_errors: Vec<String>,
}

enum ReviewedDestination {
    Local(PreparedDestination),
    Remote,
}

struct ReviewedImport {
    plan: ImportPlan,
    destination: ReviewedDestination,
    backup: Option<(PathBuf, PreparedDestination)>,
    preserve_names: HashSet<String>,
}

struct JournalCheckpoint {
    path: PathBuf,
    items_since_flush: usize,
    last_flush: Instant,
}

impl JournalCheckpoint {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            items_since_flush: 0,
            last_flush: Instant::now(),
        }
    }

    fn record_item(&mut self, result: &ImportResult) -> Result<()> {
        self.items_since_flush += 1;
        if self.items_since_flush >= JOURNAL_FLUSH_ITEMS
            && self.last_flush.elapsed() >= JOURNAL_FLUSH_INTERVAL
        {
            self.flush(result)?;
        }
        Ok(())
    }

    fn flush(&mut self, result: &ImportResult) -> Result<()> {
        atomic_json(&self.path, result)?;
        self.items_since_flush = 0;
        self.last_flush = Instant::now();
        Ok(())
    }
}

/// Single-pass reader that verifies source length and modification timestamp at EOF
/// before the caller commits the destination file or WebDAV `MOVE`.
struct VerifiedSourceReader {
    file: fs::File,
    expected_size: u64,
    expected_modified: u128,
    bytes_read: u64,
    verified_eof: bool,
}

impl VerifiedSourceReader {
    fn open(scope: &ScopedSource, item: &ImportItem) -> Result<Self> {
        let file = scope.open_file(&item.source_relative_path)?;
        let metadata = file.metadata()?;
        if metadata.len() != item.size || modified_nanos(&metadata)? != item.modified_nanos {
            bail!("Source changed after planning; create a fresh plan");
        }
        Ok(Self {
            file,
            expected_size: item.size,
            expected_modified: item.modified_nanos,
            bytes_read: 0,
            verified_eof: false,
        })
    }
}

impl Read for VerifiedSourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(buf)?;
        self.bytes_read += count as u64;
        if self.bytes_read > self.expected_size {
            return Err(io::Error::other(
                "Source changed after planning; create a fresh plan",
            ));
        }
        if count == 0 && !self.verified_eof {
            if self.bytes_read != self.expected_size {
                return Err(io::Error::other(
                    "Source changed after planning; create a fresh plan",
                ));
            }
            let after = self.file.metadata()?;
            let after_modified = after
                .modified()
                .map_err(io::Error::other)?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            if after.len() != self.expected_size || after_modified != self.expected_modified {
                return Err(io::Error::other(
                    "Source changed after planning; create a fresh plan",
                ));
            }
            self.verified_eof = true;
        }
        Ok(count)
    }
}

pub struct IngestService {
    history_dir: PathBuf,
    plans: HashMap<String, ReviewedImport>,
}

impl IngestService {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let history_dir = data_dir.as_ref().join("imports");
        fs::create_dir_all(&history_dir)?;
        Ok(Self {
            history_dir,
            plans: HashMap::new(),
        })
    }

    /// A folder is a source, whether removable, mounted, or selected manually.
    /// Planning is read-only and does not register or modify a card.
    /// Compatibility entry: `preserve` or `date` (capture day, `Y-M-D`).
    pub fn plan(
        &mut self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        layout: &str,
        deduplicate: bool,
    ) -> Result<ImportPlan> {
        let rules = OrganizeRules::from_legacy_layout(layout)?;
        self.plan_with_rules(source, destination, &rules, deduplicate)
    }

    pub fn plan_with_rules(
        &mut self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        rules: &OrganizeRules,
        deduplicate: bool,
    ) -> Result<ImportPlan> {
        self.plan_with_options(source, destination, rules, deduplicate, None)
    }

    pub fn plan_with_options(
        &mut self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        rules: &OrganizeRules,
        deduplicate: bool,
        backup_destination: Option<&Path>,
    ) -> Result<ImportPlan> {
        rules.validate()?;
        let layout = match rules.date_basis {
            DateBasis::Preserve => "preserve",
            _ => "date",
        };
        let import_day = chrono::Local::now().format("%Y-%m-%d").to_string();
        let source = fs::canonicalize(source.as_ref()).context("Import source is unavailable")?;
        if !source.is_dir() {
            bail!("Import source must be a directory");
        }
        let destination = prospective_root(destination.as_ref())?;
        if destination.starts_with(&source) || source.starts_with(&destination) {
            bail!("Import source and destination must be separate directory trees");
        }
        let destination_capability = PreparedDestination::new(&destination)?;
        let backup = match backup_destination {
            Some(raw) => Some(prepare_backup_root(&source, Some(&destination), raw)?),
            None => None,
        };
        let source_scope = ScopedSource::open(&source)?;
        let mut warnings = Vec::new();
        let source_discovery = discover(&source, &mut warnings)?;
        let source_files = source_discovery.files;
        let source_keys: Vec<String> = source_files.iter().map(|f| f.key.clone()).collect();
        let preserve_names = companion_names(&source_keys, &source_discovery.unavailable);
        let capture_dates = if rules.needs_capture_dates() {
            resolve_capture_dates(&source_scope, &source_files, &mut warnings)
        } else {
            HashMap::new()
        };
        let anchors = group_anchors(rules, &source_keys);
        let sequences = group_sequences(&source_keys);

        let (dest_scope, destination_by_size) = if deduplicate && destination.is_dir() {
            let dest_discovery = discover(&destination, &mut warnings)?;
            let mut by_size: HashMap<u64, Vec<String>> = HashMap::new();
            for entry in dest_discovery.files {
                by_size.entry(entry.size).or_default().push(entry.key);
            }
            (Some(ScopedSource::open(&destination)?), by_size)
        } else {
            (None, HashMap::new())
        };

        let mut source_size_counts: HashMap<u64, usize> = HashMap::new();
        for entry in &source_files {
            *source_size_counts.entry(entry.size).or_insert(0) += 1;
        }

        struct PlannedTarget {
            relative_path: String,
            date_source: String,
            preserve_name: bool,
            needs_quick_fp: bool,
        }

        let mut planned_targets: Vec<Option<PlannedTarget>> = Vec::with_capacity(source_files.len());
        for entry in &source_files {
            let preserve_name = preserve_names.contains(&entry.key);
            if preserve_name {
                if let Err(error) = source_scope.open_file(&entry.key) {
                    warnings.push(format!("Skipped {}: {error}", entry.key));
                    planned_targets.push(None);
                    continue;
                }
            }
            let seq = *sequences.get(&group_seq_key(&entry.key)).unwrap_or(&1);
            match target_for(
                rules,
                &entry.key,
                entry.modified_day.as_deref(),
                &capture_dates,
                &anchors,
                &import_day,
                seq,
            ) {
                Ok((relative_path, date_source)) => {
                    let has_dest_candidate = deduplicate
                        && destination_by_size.get(&entry.size).is_some_and(|cands| {
                            cands
                                .iter()
                                .any(|c| same_dedup_role(&relative_path, c, preserve_name))
                        });
                    let has_batch_collision =
                        deduplicate && source_size_counts.get(&entry.size).copied().unwrap_or(0) > 1;
                    planned_targets.push(Some(PlannedTarget {
                        relative_path,
                        date_source,
                        preserve_name,
                        needs_quick_fp: has_dest_candidate || has_batch_collision,
                    }));
                }
                Err(error) => {
                    warnings.push(format!("Skipped {}: {error}", entry.key));
                    planned_targets.push(None);
                }
            }
        }

        let fp_jobs: Vec<usize> = planned_targets
            .iter()
            .enumerate()
            .filter_map(|(idx, t)| t.as_ref().is_some_and(|p| p.needs_quick_fp).then_some(idx))
            .collect();
        let source_fps = compute_source_fingerprints(&source_scope, &source_files, &fp_jobs, false);

        let mut destination_fps: HashMap<String, String> = HashMap::new();
        let mut batch_fps: HashMap<String, String> = HashMap::new();
        let mut items = Vec::with_capacity(source_files.len());

        for (idx, entry) in source_files.iter().enumerate() {
            let Some(target) = &planned_targets[idx] else {
                continue;
            };
            let mut quick_fingerprint = String::new();
            if target.needs_quick_fp {
                match source_fps.get(&idx) {
                    Some(Ok((fp, _))) => quick_fingerprint = fp.clone(),
                    Some(Err(err)) => {
                        warnings.push(format!("Skipped {}: {err}", entry.key));
                        continue;
                    }
                    None => {}
                }
            }

            let mut duplicate_path = None;
            if deduplicate && !quick_fingerprint.is_empty() {
                if let (Some(dest_scope), Some(candidates)) =
                    (dest_scope.as_ref(), destination_by_size.get(&entry.size))
                {
                    for candidate in candidates {
                        if !same_dedup_role(&target.relative_path, candidate, target.preserve_name)
                        {
                            continue;
                        }
                        let candidate_fp = if let Some(fp) = destination_fps.get(candidate) {
                            Some(fp.clone())
                        } else {
                            match dest_scope
                                .open_file(candidate)
                                .and_then(|mut f| quick_fingerprint_reader(&mut f, entry.size))
                            {
                                Ok(fp) => {
                                    destination_fps.insert(candidate.clone(), fp.clone());
                                    Some(fp)
                                }
                                Err(error) => {
                                    warnings.push(format!(
                                        "Cannot verify destination {candidate}: {error}"
                                    ));
                                    None
                                }
                            }
                        };
                        if candidate_fp.as_deref() == Some(quick_fingerprint.as_str()) {
                            duplicate_path = Some(candidate.clone());
                            break;
                        }
                    }
                }
                let duplicate_identity = dedup_identity(
                    &target.relative_path,
                    &quick_fingerprint,
                    target.preserve_name,
                );
                if duplicate_path.is_none() {
                    duplicate_path = batch_fps.get(&duplicate_identity).cloned();
                }
                batch_fps.entry(duplicate_identity).or_insert_with(|| {
                    duplicate_path
                        .clone()
                        .unwrap_or_else(|| target.relative_path.clone())
                });
            }

            let status = if duplicate_path.is_some() {
                "duplicate"
            } else {
                "pending"
            };
            items.push(ImportItem {
                source: source.join(&entry.key).to_string_lossy().into_owned(),
                relative_path: target.relative_path.clone(),
                size: entry.size,
                status: status.to_string(),
                sha256: String::new(),
                date_source: target.date_source.clone(),
                duplicate_path,
                source_relative_path: entry.key.clone(),
                modified_nanos: entry.modified_nanos,
                quick_fingerprint,
            });
        }

        exclude_incomplete_groups(&mut items, &preserve_names, &mut warnings);
        let duplicate_count = items
            .iter()
            .filter(|item| item.status == "duplicate")
            .count();
        let total_bytes = items
            .iter()
            .filter(|item| item.status != "duplicate")
            .map(|item| item.size)
            .sum();

        let plan = ImportPlan {
            id: Uuid::new_v4().to_string(),
            source: source.to_string_lossy().into_owned(),
            destination: destination.to_string_lossy().into_owned(),
            layout: layout.to_string(),
            rules: rules.clone(),
            import_day,
            deduplicate,
            items,
            total_bytes,
            duplicate_count,
            warnings,
            created_at: Utc::now().to_rfc3339(),
            destination_location: None,
            destination_prefix: String::new(),
            likely_duplicate_count: 0,
            backup_destination: backup
                .as_ref()
                .map(|(root, _)| root.to_string_lossy().into_owned()),
        };
        if self.plans.len() >= 32 {
            self.plans.clear();
        }
        self.plans.insert(
            plan.id.clone(),
            ReviewedImport {
                plan: plan.clone(),
                destination: ReviewedDestination::Local(destination_capability),
                backup,
                preserve_names,
            },
        );
        Ok(plan)
    }

    pub fn execute(&mut self, plan_id: &str) -> Result<ImportResult> {
        self.execute_with_options(plan_id, ExecuteOptions::default())
    }

    pub fn execute_with_options(
        &mut self,
        plan_id: &str,
        options: ExecuteOptions<'_>,
    ) -> Result<ImportResult> {
        let reviewed = self
            .plans
            .get(plan_id)
            .context("Plan not found; review a fresh import plan")?;
        let plan = reviewed.plan.clone();
        let source = Path::new(&plan.source);
        let destination = Path::new(&plan.destination);
        if fs::canonicalize(source)? != source || prospective_root(destination)? != destination {
            bail!("Import roots changed after planning");
        }
        let source_scope = ScopedSource::open(source)?;
        let (active_items, skipped_unselected) = filter_selected_items(
            &plan.items,
            &reviewed.preserve_names,
            options.selected_sources.as_ref(),
        )?;
        let backup_target = materialize_backup(
            source,
            Some(destination),
            reviewed.backup.as_ref(),
            options.backup_destination.as_deref(),
        )?;
        let mut result = ImportResult {
            id: Uuid::new_v4().to_string(),
            plan_id: plan.id.clone(),
            source: plan.source.clone(),
            destination: plan.destination.clone(),
            status: "running".to_string(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            total: active_items.len(),
            copied: 0,
            duplicates: 0,
            failed: 0,
            items: Vec::with_capacity(active_items.len()),
            originals_retained: true,
            rules: Some(plan.rules.clone()),
            skipped_unselected,
            backup_destination: backup_target
                .as_ref()
                .map(|(root, _)| root.to_string_lossy().into_owned()),
            backup_copied: 0,
            backup_failed: 0,
            backup_errors: Vec::new(),
        };
        let mut journal =
            JournalCheckpoint::new(self.history_dir.join(format!("{}.json", result.id)));
        journal.flush(&result)?;
        let ReviewedDestination::Local(prepared) = &reviewed.destination else {
            bail!("This plan targets a remote location; execute it with a remote writer");
        };
        let scoped_destination = prepared.materialize()?;
        let mut group_members: HashMap<String, Vec<&ImportItem>> = HashMap::new();
        for item in &active_items {
            if reviewed.preserve_names.contains(&item.source_relative_path) {
                group_members
                    .entry(capture_group(&item.source_relative_path))
                    .or_default()
                    .push(*item);
            }
        }
        let active_owned: Vec<ImportItem> = active_items.iter().map(|&i| i.clone()).collect();
        let mut group_errors = conflicting_companion_groups(&active_owned, &group_members);
        let mut checked_groups = HashSet::new();
        let mut verified_batch: HashMap<String, String> = HashMap::new();
        let mut fp_cache: HashMap<String, String> = HashMap::new();
        let total_items = active_items.len();
        let total_bytes: u64 = active_items.iter().map(|i| i.size).sum();
        let mut done_items = 0usize;
        let mut copied_bytes = 0u64;
        let mut was_cancelled = false;
        let emit_progress = |status: &str,
                             current: Option<String>,
                             done: usize,
                             bytes: u64,
                             backup: usize,
                             cancelling: bool| {
            if let Some(cb) = options.progress {
                cb(ImportProgress {
                    plan_id: plan.id.clone(),
                    status: status.to_string(),
                    total_items,
                    done_items: done,
                    total_bytes,
                    copied_bytes: bytes,
                    current_file: current,
                    backup_copied: backup,
                    cancelling,
                });
            }
        };
        emit_progress("running", None, 0, 0, 0, false);
        for item in active_items {
            let preserve_name = reviewed.preserve_names.contains(&item.source_relative_path);
            let group = preserve_name.then(|| capture_group(&item.source_relative_path));
            let cancelling = options
                .cancel
                .is_some_and(|flag| flag.load(Ordering::Relaxed));
            let mid_companion_group = group.as_ref().is_some_and(|g| checked_groups.contains(g));
            if cancelling && !mid_companion_group {
                was_cancelled = true;
                break;
            }
            emit_progress(
                "running",
                Some(item.source_relative_path.clone()),
                done_items,
                copied_bytes,
                result.backup_copied,
                cancelling,
            );
            let duplicate_identity = item
                .batch_token()
                .map(|tok| dedup_identity(&item.relative_path, tok, preserve_name));
            let outcome = (|| -> Result<ItemResult> {
                if let Some(group) = &group {
                    if let Some(error) = group_errors.get(group) {
                        bail!("{error}");
                    }
                    if checked_groups.insert(group.clone()) {
                        preflight_companion_targets(
                            &source_scope,
                            &scoped_destination,
                            &group_members[group],
                            plan.deduplicate,
                            &mut fp_cache,
                        )?;
                    }
                }
                execute_item(
                    &source_scope,
                    destination,
                    &scoped_destination,
                    item,
                    plan.deduplicate,
                    preserve_name,
                    duplicate_identity
                        .as_ref()
                        .and_then(|id| verified_batch.get(id))
                        .map(String::as_str),
                    &mut fp_cache,
                )
            })();
            let item_result = match outcome {
                Ok(item_result) => {
                    if let (Some(id), Ok(key)) = (
                        duplicate_identity,
                        Path::new(&item_result.destination).strip_prefix(destination),
                    ) {
                        verified_batch.insert(id, key.to_string_lossy().replace('\\', "/"));
                    }
                    if item_result.status == "duplicate" {
                        result.duplicates += 1;
                    } else {
                        result.copied += 1;
                    }
                    if let Some((_, backup_scoped)) = &backup_target {
                        match copy_to_backup(
                            &source_scope,
                            backup_scoped,
                            item,
                            preserve_name,
                            item_result.sha256.as_deref(),
                            &mut fp_cache,
                        ) {
                            Ok(true) => result.backup_copied += 1,
                            Ok(false) => {}
                            Err(err) => {
                                result.backup_failed += 1;
                                result
                                    .backup_errors
                                    .push(format!("{}: {err:#}", item.source_relative_path));
                            }
                        }
                    }
                    item_result
                }
                Err(error) => {
                    if let Some(group) = group {
                        group_errors.entry(group).or_insert_with(|| {
                            format!(
                                "XMP companion group stopped after {} failed: {error:#}. Already verified files are retained; no remaining member will be published. Choose a new destination directory and retry the complete group",
                                item.source_relative_path
                            )
                        });
                    }
                    result.failed += 1;
                    ItemResult {
                        source: item.source.clone(),
                        destination: destination
                            .join(&item.relative_path)
                            .to_string_lossy()
                            .into_owned(),
                        status: "failed".to_string(),
                        size: item.size,
                        sha256: None,
                        error: Some(format!("{error:#}")),
                    }
                }
            };
            done_items += 1;
            copied_bytes = copied_bytes.saturating_add(item.size);
            result.items.push(item_result);
            journal.record_item(&result)?;
            emit_progress(
                "running",
                Some(item.source_relative_path.clone()),
                done_items,
                copied_bytes,
                result.backup_copied,
                options
                    .cancel
                    .is_some_and(|flag| flag.load(Ordering::Relaxed)),
            );
        }
        result.status = if was_cancelled {
            "cancelled"
        } else if result.failed == 0 && result.backup_failed == 0 {
            "completed"
        } else if result.copied + result.duplicates == 0 {
            "failed"
        } else {
            "partial"
        }
        .to_string();
        result.finished_at = Some(Utc::now().to_rfc3339());
        journal.flush(&result)?;
        emit_progress(
            &result.status,
            None,
            done_items,
            copied_bytes,
            result.backup_copied,
            was_cancelled,
        );
        Ok(result)
    }

    /// Plan an import into a WebDAV/S3 library. Read-only on both sides: source
    /// files are only sampled/hashed on exact byte-size collisions, and the
    /// destination is judged from `known` catalog records without network traffic.
    pub fn plan_remote(
        &mut self,
        source: impl AsRef<Path>,
        location_id: &str,
        prefix: &str,
        rules: &OrganizeRules,
        deduplicate: bool,
        known: &[KnownAsset],
    ) -> Result<ImportPlan> {
        self.plan_remote_with_options(source, location_id, prefix, rules, deduplicate, known, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn plan_remote_with_options(
        &mut self,
        source: impl AsRef<Path>,
        location_id: &str,
        prefix: &str,
        rules: &OrganizeRules,
        deduplicate: bool,
        known: &[KnownAsset],
        backup_destination: Option<&Path>,
    ) -> Result<ImportPlan> {
        rules.validate()?;
        let prefix = prefix.trim_matches('/').to_owned();
        validate_key(&prefix, true)?;
        let import_day = chrono::Local::now().format("%Y-%m-%d").to_string();
        let source = fs::canonicalize(source.as_ref()).context("Import source is unavailable")?;
        if !source.is_dir() {
            bail!("Import source must be a directory");
        }
        let backup = match backup_destination {
            Some(raw) => Some(prepare_backup_root(&source, None, raw)?),
            None => None,
        };
        let source_scope = ScopedSource::open(&source)?;
        let mut warnings = Vec::new();
        let discovery = discover(&source, &mut warnings)?;
        let source_files = discovery.files;
        let source_keys: Vec<String> = source_files.iter().map(|f| f.key.clone()).collect();
        let preserve_names = companion_names(&source_keys, &discovery.unavailable);
        let capture_dates = if rules.needs_capture_dates() {
            resolve_capture_dates(&source_scope, &source_files, &mut warnings)
        } else {
            HashMap::new()
        };
        let anchors = group_anchors(rules, &source_keys);
        let sequences = group_sequences(&source_keys);

        let mut known_hashed_by_size: HashMap<u64, Vec<&KnownAsset>> = HashMap::new();
        let mut by_name: HashMap<(String, u64), Vec<&KnownAsset>> = HashMap::new();
        for asset in known {
            if asset.sha256.is_some() {
                known_hashed_by_size
                    .entry(asset.size)
                    .or_default()
                    .push(asset);
            } else {
                by_name
                    .entry((asset.filename.to_lowercase(), asset.size))
                    .or_default()
                    .push(asset);
            }
        }

        let mut source_size_counts: HashMap<u64, usize> = HashMap::new();
        for entry in &source_files {
            *source_size_counts.entry(entry.size).or_insert(0) += 1;
        }

        struct RemoteTarget {
            relative_path: String,
            target: String,
            date_source: String,
            preserve_name: bool,
            needs_full_sha: bool,
            needs_quick_fp: bool,
        }

        let mut targets: Vec<Option<RemoteTarget>> = Vec::with_capacity(source_files.len());
        for entry in &source_files {
            let preserve_name = preserve_names.contains(&entry.key);
            if preserve_name {
                if let Err(error) = source_scope.open_file(&entry.key) {
                    warnings.push(format!("Skipped {}: {error}", entry.key));
                    targets.push(None);
                    continue;
                }
            }
            let seq = *sequences.get(&group_seq_key(&entry.key)).unwrap_or(&1);
            match target_for(
                rules,
                &entry.key,
                entry.modified_day.as_deref(),
                &capture_dates,
                &anchors,
                &import_day,
                seq,
            ) {
                Ok((relative_path, date_source)) => {
                    let target = join_key(&prefix, &relative_path);
                    let needs_full_sha = deduplicate
                        && known_hashed_by_size
                            .get(&entry.size)
                            .is_some_and(|list| {
                                list.iter()
                                    .any(|asset| same_dedup_role(&target, &asset.key, preserve_name))
                            });
                    let needs_quick_fp = deduplicate
                        && source_size_counts.get(&entry.size).copied().unwrap_or(0) > 1;
                    targets.push(Some(RemoteTarget {
                        relative_path,
                        target,
                        date_source,
                        preserve_name,
                        needs_full_sha,
                        needs_quick_fp,
                    }));
                }
                Err(error) => {
                    warnings.push(format!("Skipped {}: {error}", entry.key));
                    targets.push(None);
                }
            }
        }

        let full_jobs: Vec<usize> = targets
            .iter()
            .enumerate()
            .filter_map(|(idx, t)| t.as_ref().is_some_and(|p| p.needs_full_sha).then_some(idx))
            .collect();
        let quick_only_jobs: Vec<usize> = targets
            .iter()
            .enumerate()
            .filter_map(|(idx, t)| {
                t.as_ref()
                    .is_some_and(|p| !p.needs_full_sha && p.needs_quick_fp)
                    .then_some(idx)
            })
            .collect();
        let full_hashes = compute_source_fingerprints(&source_scope, &source_files, &full_jobs, true);
        let quick_hashes =
            compute_source_fingerprints(&source_scope, &source_files, &quick_only_jobs, false);

        let mut batch_fps: HashMap<String, String> = HashMap::new();
        let mut items = Vec::with_capacity(source_files.len());
        for (idx, entry) in source_files.iter().enumerate() {
            let Some(t) = &targets[idx] else {
                continue;
            };
            let mut sha256 = String::new();
            let mut quick_fingerprint = String::new();
            if t.needs_full_sha {
                match full_hashes.get(&idx) {
                    Some(Ok((fp, Some(full)))) => {
                        quick_fingerprint = fp.clone();
                        sha256 = full.clone();
                    }
                    Some(Err(err)) => {
                        warnings.push(format!("Skipped {}: {err}", entry.key));
                        continue;
                    }
                    _ => {}
                }
            } else if t.needs_quick_fp {
                match quick_hashes.get(&idx) {
                    Some(Ok((fp, _))) => quick_fingerprint = fp.clone(),
                    Some(Err(err)) => {
                        warnings.push(format!("Skipped {}: {err}", entry.key));
                        continue;
                    }
                    None => {}
                }
            }

            let mut status = "pending";
            let mut duplicate_path = None;
            if deduplicate {
                if !sha256.is_empty() {
                    if let Some(asset) = known_hashed_by_size
                        .get(&entry.size)
                        .and_then(|list| {
                            list.iter().find(|asset| {
                                asset.sha256.as_deref() == Some(sha256.as_str())
                                    && same_dedup_role(&t.target, &asset.key, t.preserve_name)
                            })
                        })
                    {
                        status = "duplicate";
                        duplicate_path = Some(asset.key.clone());
                    }
                }
                let batch_token = if !quick_fingerprint.is_empty() {
                    Some(quick_fingerprint.as_str())
                } else if !sha256.is_empty() {
                    Some(sha256.as_str())
                } else {
                    None
                };
                if status == "pending" {
                    if let Some(token) = batch_token {
                        let identity =
                            dedup_identity(&t.relative_path, token, t.preserve_name);
                        if let Some(existing) = batch_fps.get(&identity) {
                            status = "duplicate";
                            duplicate_path = Some(existing.clone());
                        }
                    }
                }
                if status == "pending" {
                    let filename = Path::new(&entry.key)
                        .file_name()
                        .map(|name| name.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    let day = capture_dates.get(&entry.key).and_then(|(day, _)| day.clone());
                    if let Some(day) = day {
                        if let Some(asset) = by_name.get(&(filename, entry.size)).and_then(|list| {
                            list.iter().find(|asset| {
                                asset.sha256.is_none()
                                    && asset.captured_at.get(..10) == Some(day.as_str())
                            })
                        }) {
                            status = "likely_duplicate";
                            duplicate_path = Some(asset.key.clone());
                        }
                    }
                }
                if let Some(token) = batch_token {
                    let identity = dedup_identity(&t.relative_path, token, t.preserve_name);
                    batch_fps
                        .entry(identity)
                        .or_insert_with(|| duplicate_path.clone().unwrap_or_else(|| t.target.clone()));
                }
            }

            items.push(ImportItem {
                source: source.join(&entry.key).to_string_lossy().into_owned(),
                relative_path: t.relative_path.clone(),
                size: entry.size,
                status: status.to_owned(),
                sha256,
                date_source: t.date_source.clone(),
                duplicate_path,
                source_relative_path: entry.key.clone(),
                modified_nanos: entry.modified_nanos,
                quick_fingerprint,
            });
        }

        exclude_incomplete_groups(&mut items, &preserve_names, &mut warnings);
        let count = |status: &str| items.iter().filter(|item| item.status == status).count();
        let plan = ImportPlan {
            id: Uuid::new_v4().to_string(),
            source: source.to_string_lossy().into_owned(),
            destination: format!("location:{location_id}/{prefix}"),
            layout: match rules.date_basis {
                DateBasis::Preserve => "preserve",
                _ => "date",
            }
            .to_string(),
            rules: rules.clone(),
            import_day,
            deduplicate,
            total_bytes: items
                .iter()
                .filter(|item| item.status == "pending")
                .map(|item| item.size)
                .sum(),
            duplicate_count: count("duplicate"),
            likely_duplicate_count: count("likely_duplicate"),
            items,
            warnings,
            created_at: Utc::now().to_rfc3339(),
            destination_location: Some(location_id.to_owned()),
            destination_prefix: prefix,
            backup_destination: backup
                .as_ref()
                .map(|(root, _)| root.to_string_lossy().into_owned()),
        };
        if self.plans.len() >= 32 {
            self.plans.clear();
        }
        self.plans.insert(
            plan.id.clone(),
            ReviewedImport {
                plan: plan.clone(),
                destination: ReviewedDestination::Remote,
                backup,
                preserve_names,
            },
        );
        Ok(plan)
    }

    /// Publish a reviewed remote plan across concurrent streaming workers.
    pub fn execute_remote(
        &mut self,
        plan_id: &str,
        writer: &RemoteWriter,
        include_likely: bool,
    ) -> Result<ImportResult> {
        self.execute_remote_with_options(
            plan_id,
            writer,
            ExecuteOptions {
                include_likely_duplicates: include_likely,
                ..ExecuteOptions::default()
            },
        )
    }

    pub fn execute_remote_with_options(
        &mut self,
        plan_id: &str,
        writer: &RemoteWriter,
        options: ExecuteOptions<'_>,
    ) -> Result<ImportResult> {
        let reviewed = self
            .plans
            .get(plan_id)
            .context("Plan not found; review a fresh import plan")?;
        let plan = reviewed.plan.clone();
        let preserve_names = reviewed.preserve_names.clone();
        ensure!(
            matches!(reviewed.destination, ReviewedDestination::Remote)
                && plan.destination_location.as_deref() == Some(writer.location_id()),
            "This plan does not target this remote location"
        );
        let source = Path::new(&plan.source);
        if fs::canonicalize(source)? != source {
            bail!("Import source changed after planning");
        }
        let source_scope = ScopedSource::open(source)?;
        let (active_items, skipped_unselected) = filter_selected_items(
            &plan.items,
            &preserve_names,
            options.selected_sources.as_ref(),
        )?;
        let backup_target = materialize_backup(
            source,
            None,
            reviewed.backup.as_ref(),
            options.backup_destination.as_deref(),
        )?;
        let mut initial_result = ImportResult {
            id: Uuid::new_v4().to_string(),
            plan_id: plan.id.clone(),
            source: plan.source.clone(),
            destination: plan.destination.clone(),
            status: "running".to_string(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            total: active_items.len(),
            copied: 0,
            duplicates: 0,
            failed: 0,
            items: Vec::with_capacity(active_items.len()),
            originals_retained: true,
            rules: Some(plan.rules.clone()),
            skipped_unselected,
            backup_destination: backup_target
                .as_ref()
                .map(|(root, _)| root.to_string_lossy().into_owned()),
            backup_copied: 0,
            backup_failed: 0,
            backup_errors: Vec::new(),
        };
        let journal_path = self.history_dir.join(format!("{}.json", initial_result.id));
        atomic_json(&journal_path, &initial_result)?;

        let mut group_members: HashMap<String, Vec<&ImportItem>> = HashMap::new();
        for item in &active_items {
            if preserve_names.contains(&item.source_relative_path) {
                group_members
                    .entry(capture_group(&item.source_relative_path))
                    .or_default()
                    .push(*item);
            }
        }
        let active_owned: Vec<ImportItem> = active_items.iter().map(|&i| i.clone()).collect();
        let initial_group_errors = conflicting_companion_groups(&active_owned, &group_members);

        // Build upload work units:
        // - Skipped duplicates / likely_duplicates are resolved deterministically without network I/O.
        // - Companion groups (`.xmp` + RAW/JPG) stay bundled as one sequential unit so mid-group
        //   failures or cooperative cancellation never split a companion group across workers.
        let mut upload_units: Vec<Vec<usize>> = Vec::new();
        let mut group_unit_index: HashMap<String, usize> = HashMap::new();
        let mut dedup_unit_index: HashMap<String, usize> = HashMap::new();
        let mut skipped_indices: Vec<usize> = Vec::new();

        for (idx, item) in active_owned.iter().enumerate() {
            let skip = item.status == "duplicate"
                || (item.status == "likely_duplicate" && !options.include_likely_duplicates);
            if skip {
                skipped_indices.push(idx);
                continue;
            }
            let preserve_name = preserve_names.contains(&item.source_relative_path);
            if preserve_name {
                let grp = capture_group(&item.source_relative_path);
                if let Some(&unit_idx) = group_unit_index.get(&grp) {
                    upload_units[unit_idx].push(idx);
                } else {
                    let unit_idx = upload_units.len();
                    group_unit_index.insert(grp, unit_idx);
                    upload_units.push(vec![idx]);
                }
                continue;
            }
            if plan.deduplicate {
                if let Some(tok) = item.batch_token() {
                    let id = dedup_identity(&item.relative_path, tok, false);
                    if let Some(&unit_idx) = dedup_unit_index.get(&id) {
                        upload_units[unit_idx].push(idx);
                        continue;
                    }
                    dedup_unit_index.insert(id, upload_units.len());
                }
            }
            upload_units.push(vec![idx]);
        }

        let total_items = active_owned.len();
        let total_bytes: u64 = active_owned.iter().map(|i| i.size).sum();
        let emit_progress = |status: &str,
                             current: Option<String>,
                             done: usize,
                             bytes: u64,
                             backup: usize,
                             cancelling: bool| {
            if let Some(cb) = options.progress {
                cb(ImportProgress {
                    plan_id: plan.id.clone(),
                    status: status.to_string(),
                    total_items,
                    done_items: done,
                    total_bytes,
                    copied_bytes: bytes,
                    current_file: current,
                    backup_copied: backup,
                    cancelling,
                });
            }
        };
        emit_progress("running", None, 0, 0, 0, false);

        struct SharedRemoteState {
            result: ImportResult,
            slots: Vec<Option<ItemResult>>,
            published: HashMap<String, (String, Option<String>)>,
            group_errors: HashMap<String, String>,
            fp_cache: HashMap<String, String>,
            done_items: usize,
            copied_bytes: u64,
            journal: JournalCheckpoint,
        }

        let shared = Mutex::new(SharedRemoteState {
            result: initial_result.clone(),
            slots: vec![None; total_items],
            published: HashMap::new(),
            group_errors: initial_group_errors,
            fp_cache: HashMap::new(),
            done_items: 0,
            copied_bytes: 0,
            journal: JournalCheckpoint::new(journal_path.clone()),
        });
        let next_unit = AtomicUsize::new(0);
        let was_cancelled = AtomicBool::new(false);
        let worker_count = upload_units.len().clamp(1, REMOTE_UPLOAD_CONCURRENCY);

        if !upload_units.is_empty() {
            std::thread::scope(|scope| {
                for _ in 0..worker_count {
                    scope.spawn(|| loop {
                        if options
                            .cancel
                            .is_some_and(|flag| flag.load(Ordering::Relaxed))
                        {
                            was_cancelled.store(true, Ordering::Relaxed);
                            break;
                        }
                        let unit_idx = next_unit.fetch_add(1, Ordering::Relaxed);
                        if unit_idx >= upload_units.len() {
                            break;
                        }
                        for &item_idx in &upload_units[unit_idx] {
                            let item = &active_owned[item_idx];
                            let preserve_name =
                                preserve_names.contains(&item.source_relative_path);
                            let group = preserve_name
                                .then(|| capture_group(&item.source_relative_path));
                            let identity = item
                                .batch_token()
                                .map(|tok| dedup_identity(&item.relative_path, tok, preserve_name));

                            let precheck = {
                                let guard = shared.lock().expect("remote state lock");
                                let cancelling = options
                                    .cancel
                                    .is_some_and(|flag| flag.load(Ordering::Relaxed));
                                emit_progress(
                                    "running",
                                    Some(item.source_relative_path.clone()),
                                    guard.done_items,
                                    guard.copied_bytes,
                                    guard.result.backup_copied,
                                    cancelling,
                                );
                                if let Some(err) =
                                    group.as_ref().and_then(|g| guard.group_errors.get(g))
                                {
                                    Some(Err(anyhow::anyhow!("{err}")))
                                } else if plan.deduplicate {
                                    identity
                                        .as_ref()
                                        .and_then(|id| guard.published.get(id))
                                        .map(|(dest, sha)| {
                                            Ok(ItemResult {
                                                source: item.source.clone(),
                                                destination: dest.clone(),
                                                status: "duplicate".into(),
                                                size: item.size,
                                                sha256: sha.clone(),
                                                error: None,
                                            })
                                        })
                                } else {
                                    None
                                }
                            };

                            let outcome = match precheck {
                                Some(res) => res,
                                None => upload_remote_item(
                                    &source_scope,
                                    writer,
                                    &plan.destination_prefix,
                                    item,
                                    preserve_name,
                                ),
                            };

                            let backup_outcome = if let (Ok(item_res), Some((_, backup_scoped))) =
                                (&outcome, backup_target.as_ref())
                            {
                                let mut fp_cache = {
                                    let guard = shared.lock().expect("remote state lock");
                                    guard.fp_cache.clone()
                                };
                                let copied = copy_to_backup(
                                    &source_scope,
                                    backup_scoped,
                                    item,
                                    preserve_name,
                                    item_res.sha256.as_deref(),
                                    &mut fp_cache,
                                );
                                Some((copied, fp_cache))
                            } else {
                                None
                            };

                            let mut guard = shared.lock().expect("remote state lock");
                            if let Some((copied_res, updated_cache)) = backup_outcome {
                                guard.fp_cache.extend(updated_cache);
                                match copied_res {
                                    Ok(true) => guard.result.backup_copied += 1,
                                    Ok(false) => {}
                                    Err(err) => {
                                        guard.result.backup_failed += 1;
                                        guard.result.backup_errors.push(format!(
                                            "{}: {err:#}",
                                            item.source_relative_path
                                        ));
                                    }
                                }
                            }

                            let item_result = match outcome {
                                Ok(item_result) => {
                                    if item_result.status == "copied" {
                                        guard.result.copied += 1;
                                        if let Some(id) = identity {
                                            guard.published.insert(
                                                id,
                                                (
                                                    item_result.destination.clone(),
                                                    item_result.sha256.clone(),
                                                ),
                                            );
                                        }
                                    } else {
                                        guard.result.duplicates += 1;
                                    }
                                    item_result
                                }
                                Err(error) => {
                                    if let Some(group) = group {
                                        guard.group_errors.entry(group).or_insert_with(|| {
                                            format!(
                                                "XMP companion group stopped after {} failed: {error:#}. Already published files are retained; no remaining member will be published",
                                                item.source_relative_path
                                            )
                                        });
                                    }
                                    guard.result.failed += 1;
                                    ItemResult {
                                        source: item.source.clone(),
                                        destination: join_key(
                                            &plan.destination_prefix,
                                            &item.relative_path,
                                        ),
                                        status: "failed".to_string(),
                                        size: item.size,
                                        sha256: None,
                                        error: Some(format!("{error:#}")),
                                    }
                                }
                            };

                            guard.done_items += 1;
                            guard.copied_bytes = guard.copied_bytes.saturating_add(item.size);
                            guard.slots[item_idx] = Some(item_result.clone());
                            guard.result.items.push(item_result);
                            {
                                let state_ref = &mut *guard;
                                let _ = state_ref.journal.record_item(&state_ref.result);
                            }
                            let done = guard.done_items;
                            let bytes = guard.copied_bytes;
                            let backup_copied = guard.result.backup_copied;
                            drop(guard);
                            emit_progress(
                                "running",
                                Some(item.source_relative_path.clone()),
                                done,
                                bytes,
                                backup_copied,
                                options
                                    .cancel
                                    .is_some_and(|flag| flag.load(Ordering::Relaxed)),
                            );
                        }
                    });
                }
            });
        }

        let cancelled = was_cancelled.load(Ordering::Relaxed);
        let mut state = shared.into_inner().expect("remote state lock");
        if !cancelled {
            for idx in skipped_indices {
                let item = &active_owned[idx];
                let preserve_name = preserve_names.contains(&item.source_relative_path);
                let identity = item
                    .batch_token()
                    .map(|tok| dedup_identity(&item.relative_path, tok, preserve_name));
                let published_match = identity
                    .as_ref()
                    .and_then(|id| state.published.get(id))
                    .cloned();
                let status = if item.status == "duplicate" {
                    "duplicate"
                } else {
                    "likely_duplicate"
                };
                let destination = published_match
                    .as_ref()
                    .map(|(dest, _)| dest.clone())
                    .or_else(|| item.duplicate_path.clone())
                    .unwrap_or_default();
                let sha256 = published_match
                    .and_then(|(_, sha)| sha)
                    .or_else(|| (!item.sha256.is_empty()).then(|| item.sha256.clone()));
                state.result.duplicates += 1;
                state.done_items += 1;
                state.copied_bytes = state.copied_bytes.saturating_add(item.size);
                state.slots[idx] = Some(ItemResult {
                    source: item.source.clone(),
                    destination,
                    status: status.into(),
                    size: item.size,
                    sha256,
                    error: None,
                });
            }
        }

        state.result.items = state.slots.into_iter().flatten().collect();
        initial_result = state.result;
        initial_result.status = if cancelled {
            "cancelled"
        } else if initial_result.failed == 0 && initial_result.backup_failed == 0 {
            "completed"
        } else if initial_result.copied + initial_result.duplicates == 0 {
            "failed"
        } else {
            "partial"
        }
        .to_string();
        initial_result.finished_at = Some(Utc::now().to_rfc3339());
        atomic_json(&journal_path, &initial_result)?;
        emit_progress(
            &initial_result.status,
            None,
            state.done_items,
            state.copied_bytes,
            initial_result.backup_copied,
            cancelled,
        );
        if !cancelled && options.selected_sources.is_none() {
            self.plans.remove(plan_id);
        }
        Ok(initial_result)
    }

    pub fn history(&self) -> Result<Vec<ImportResult>> {
        let mut history = Vec::new();
        for entry in fs::read_dir(&self.history_dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let mut result: ImportResult = serde_json::from_slice(&fs::read(entry.path())?)
                .context("Import journal is damaged")?;
            if result.status == "running" {
                result.status = "interrupted".to_string();
            }
            history.push(result);
        }
        history.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(history)
    }
}

fn upload_remote_item(
    source_scope: &ScopedSource,
    writer: &RemoteWriter,
    destination_prefix: &str,
    item: &ImportItem,
    preserve_name: bool,
) -> Result<ItemResult> {
    validate_key(&item.source_relative_path, false)?;
    validate_key(&item.relative_path, false)?;
    let mut cached_sha = (!item.sha256.is_empty()).then(|| item.sha256.clone());
    if writer.kind() == "s3" && cached_sha.is_none() {
        let mut reader = VerifiedSourceReader::open(source_scope, item)?;
        let (size, sha) = hash_reader(&mut reader)?;
        ensure!(size == item.size, "Source content changed after planning");
        cached_sha = Some(sha);
    }

    for attempt in 0..100 {
        let relative = if attempt == 0 {
            item.relative_path.clone()
        } else {
            let sha = match &cached_sha {
                Some(sha) => sha.as_str(),
                None => {
                    let mut reader = VerifiedSourceReader::open(source_scope, item)?;
                    let (_, sha) = hash_reader(&mut reader)?;
                    cached_sha = Some(sha);
                    cached_sha.as_deref().unwrap()
                }
            };
            collision_key(&item.relative_path, sha, attempt)?
        };
        let key = join_key(destination_prefix, &relative);
        let reader = VerifiedSourceReader::open(source_scope, item)?;
        match writer.put_new_streaming(&key, reader, item.size, cached_sha.as_deref()) {
            Ok(actual_sha) => {
                return Ok(ItemResult {
                    source: item.source.clone(),
                    destination: key,
                    status: "copied".into(),
                    size: item.size,
                    sha256: Some(actual_sha),
                    error: None,
                });
            }
            Err(error) if format!("{error:#}").contains("E_CONFLICT") => {
                if preserve_name {
                    bail!("Filename conflict in an XMP companion group at {key}: individual files cannot be renamed without breaking pairing. Choose another prefix; all source files are retained");
                }
            }
            Err(error) => return Err(error),
        }
    }
    bail!("Too many filename conflicts for {}", item.relative_path)
}

fn preflight_companion_targets(
    source_scope: &ScopedSource,
    destination: &ScopedDestination,
    items: &[&ImportItem],
    deduplicate: bool,
    fp_cache: &mut HashMap<String, String>,
) -> Result<()> {
    for item in items {
        if destination.exists(&item.relative_path)?
            && !(deduplicate
                && existing_matches(
                    source_scope,
                    destination,
                    &item.relative_path,
                    item,
                    fp_cache,
                ))
        {
            bail!(
                "Filename conflict at {} blocks the entire XMP companion group before publication. Choose a new destination directory; all source files are retained",
                item.relative_path
            );
        }
    }
    Ok(())
}

fn conflicting_companion_groups(
    items: &[ImportItem],
    groups: &HashMap<String, Vec<&ImportItem>>,
) -> HashMap<String, String> {
    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for item in items
        .iter()
        .filter(|item| is_pairable(&item.source_relative_path))
    {
        claims
            .entry(capture_group(&item.relative_path).to_lowercase())
            .or_default()
            .insert(capture_group(&item.source_relative_path));
    }
    let mut errors = HashMap::new();
    for (target_stem, source_groups) in claims {
        if source_groups.len() < 2 {
            continue;
        }
        for group in &source_groups {
            if groups.contains_key(group) {
                errors.entry(group.clone()).or_insert_with(|| {
                    format!(
                        "Multiple source groups map to target stem {target_stem}: {}. The entire XMP companion group is blocked to prevent mixed pairing. Choose a new destination directory or preserve source folders",
                        source_groups.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                });
            }
        }
    }
    errors
}

#[allow(clippy::too_many_arguments)]
fn execute_item(
    source_scope: &ScopedSource,
    destination: &Path,
    scoped: &ScopedDestination,
    item: &ImportItem,
    deduplicate: bool,
    preserve_name: bool,
    batch_duplicate: Option<&str>,
    fp_cache: &mut HashMap<String, String>,
) -> Result<ItemResult> {
    validate_key(&item.source_relative_path, false)?;
    validate_key(&item.relative_path, false)?;
    // Lightweight open + metadata precheck (0 content bytes read before dedup check).
    let precheck_reader = VerifiedSourceReader::open(source_scope, item)?;
    if deduplicate {
        for key in batch_duplicate
            .into_iter()
            .chain(item.duplicate_path.as_deref())
        {
            if existing_matches(source_scope, scoped, key, item, fp_cache) {
                return Ok(ItemResult {
                    source: item.source.clone(),
                    destination: destination.join(key).to_string_lossy().into_owned(),
                    status: "duplicate".to_string(),
                    size: item.size,
                    sha256: (!item.sha256.is_empty()).then(|| item.sha256.clone()),
                    error: None,
                });
            }
        }
    }

    if !scoped.exists(&item.relative_path)? {
        let mut reader = precheck_reader;
        let expected = (!item.sha256.is_empty()).then_some(item.sha256.as_str());
        let (_, actual_sha) = scoped.publish(&mut reader, &item.relative_path, expected)?;
        return Ok(ItemResult {
            source: item.source.clone(),
            destination: destination
                .join(&item.relative_path)
                .to_string_lossy()
                .into_owned(),
            status: "copied".to_string(),
            size: item.size,
            sha256: Some(actual_sha),
            error: None,
        });
    }

    drop(precheck_reader);
    if deduplicate && existing_matches(source_scope, scoped, &item.relative_path, item, fp_cache) {
        return Ok(ItemResult {
            source: item.source.clone(),
            destination: destination
                .join(&item.relative_path)
                .to_string_lossy()
                .into_owned(),
            status: "duplicate".to_string(),
            size: item.size,
            sha256: (!item.sha256.is_empty()).then(|| item.sha256.clone()),
            error: None,
        });
    }
    if preserve_name {
        bail!("Filename conflict in an XMP companion group: individual files cannot be renamed without breaking pairing. Choose a new destination directory; all source files are retained");
    }

    let suffix_sha = if !item.sha256.is_empty() {
        item.sha256.clone()
    } else {
        let mut reader = VerifiedSourceReader::open(source_scope, item)?;
        let (_, sha) = hash_reader(&mut reader)?;
        sha
    };
    for attempt in 1..100 {
        let key = collision_key(&item.relative_path, &suffix_sha, attempt)?;
        let target = destination.join(&key);
        if scoped.exists(&key)? {
            if deduplicate && existing_matches(source_scope, scoped, &key, item, fp_cache) {
                return Ok(ItemResult {
                    source: item.source.clone(),
                    destination: target.to_string_lossy().into_owned(),
                    status: "duplicate".to_string(),
                    size: item.size,
                    sha256: Some(suffix_sha),
                    error: None,
                });
            }
            continue;
        }
        let mut reader = VerifiedSourceReader::open(source_scope, item)?;
        let (_, actual_sha) = scoped.publish(&mut reader, &key, Some(&suffix_sha))?;
        return Ok(ItemResult {
            source: item.source.clone(),
            destination: target.to_string_lossy().into_owned(),
            status: "copied".to_string(),
            size: item.size,
            sha256: Some(actual_sha),
            error: None,
        });
    }
    bail!("Too many filename conflicts; no existing file was replaced")
}

fn ensure_item_quick_fingerprint(
    source_scope: &ScopedSource,
    item: &ImportItem,
    fp_cache: &mut HashMap<String, String>,
) -> Result<String> {
    if !item.quick_fingerprint.is_empty() {
        return Ok(item.quick_fingerprint.clone());
    }
    if let Some(fp) = fp_cache.get(&item.source_relative_path) {
        return Ok(fp.clone());
    }
    let mut file = source_scope.open_file(&item.source_relative_path)?;
    let fp = quick_fingerprint_reader(&mut file, item.size)?;
    fp_cache.insert(item.source_relative_path.clone(), fp.clone());
    Ok(fp)
}

fn existing_matches(
    source_scope: &ScopedSource,
    destination: &ScopedDestination,
    key: &str,
    item: &ImportItem,
    fp_cache: &mut HashMap<String, String>,
) -> bool {
    let Ok(expected_fp) = ensure_item_quick_fingerprint(source_scope, item, fp_cache) else {
        return false;
    };
    destination
        .reader(key)
        .and_then(|mut file| {
            if file.metadata()?.len() != item.size {
                return Ok(false);
            }
            Ok(quick_fingerprint_reader(&mut file, item.size)? == expected_fp)
        })
        .unwrap_or(false)
}

fn collision_key(key: &str, hash: &str, attempt: usize) -> Result<String> {
    if attempt == 0 {
        return Ok(key.to_string());
    }
    ensure!(hash.len() >= 12, "Collision suffix requires a content hash");
    let path = Path::new(key);
    let stem = path
        .file_stem()
        .context("Missing filename")?
        .to_string_lossy();
    let extension = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let suffix = if attempt == 1 {
        String::new()
    } else {
        format!("-{attempt}")
    };
    Ok(path
        .with_file_name(format!("{stem}-{}{suffix}{extension}", &hash[..12]))
        .to_string_lossy()
        .replace('\\', "/"))
}

fn prospective_root(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        bail!("Import root must not contain relative traversal");
    }
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(
            existing
                .file_name()
                .context("Invalid import root")?
                .to_os_string(),
        );
        existing = existing
            .parent()
            .context("Import root has no existing parent")?;
    }
    let mut resolved = fs::canonicalize(existing)?;
    for part in missing.iter().rev() {
        validate_key(&part.to_string_lossy(), false)?;
        resolved.push(part);
    }
    Ok(resolved)
}

fn modified_nanos(metadata: &fs::Metadata) -> Result<u128> {
    Ok(metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos())
}

fn supported(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    [
        "jpg", "jpeg", "png", "tif", "tiff", "webp", "avif", "heic", "heif", "gif", "bmp", "dng",
        "nef", "nrw", "cr2", "cr3", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "srw", "3fr",
        "fff", "iiq", "rwl", "x3f", "raw", "mov", "mp4", "m4v", "webm", "3gp", "avi", "mkv", "mts",
        "m2ts", "xmp",
    ]
    .contains(&extension.as_str())
}

struct DiscoveredFile {
    key: String,
    size: u64,
    modified_nanos: u128,
    modified_day: Option<String>,
}

struct DiscoveredFiles {
    files: Vec<DiscoveredFile>,
    unavailable: Vec<String>,
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

/// Fast single-root metadata discovery without per-file `canonicalize` syscalls.
/// Rejects symlinks, Windows reparse points, dotfiles, and unsafe filenames.
fn discover(root: &Path, warnings: &mut Vec<String>) -> Result<DiscoveredFiles> {
    let mut pending = vec![(PathBuf::new(), 0usize)];
    let mut files = Vec::new();
    let mut unavailable = Vec::new();
    while let Some((relative, depth)) = pending.pop() {
        if depth > 64 {
            warnings.push(format!(
                "Folder depth limit reached: {}",
                relative.display()
            ));
            continue;
        }
        let entries = match fs::read_dir(root.join(&relative)) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!("Cannot scan {}: {error}", relative.display()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(error.to_string());
                    continue;
                }
            };
            let raw_name = entry.file_name();
            let name = raw_name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = relative.join(&raw_name);
            let key = path.to_string_lossy().replace('\\', "/");
            if let Err(error) = validate_key(&name, false) {
                warnings.push(format!("Skipped {key}: {error}"));
                if supported(&path) {
                    unavailable.push(key);
                }
                continue;
            }
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!("Skipped {key}: {error}"));
                    if supported(&path) {
                        unavailable.push(key);
                    }
                    continue;
                }
            };
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                warnings.push(format!(
                    "Skipped {key}: Links and reparse points are not followed"
                ));
                if supported(&path) {
                    unavailable.push(key);
                }
                continue;
            }
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() && supported(&path) {
                match modified_nanos(&metadata) {
                    Ok(modified_nanos) => {
                        files.push(DiscoveredFile {
                            key,
                            size: metadata.len(),
                            modified_nanos,
                            modified_day: modified_day(&metadata),
                        });
                    }
                    Err(error) => {
                        warnings.push(format!("Skipped {key}: {error}"));
                        unavailable.push(key);
                    }
                }
            }
            if files.len() > 1_000_000 {
                bail!("Import file limit exceeded");
            }
        }
    }
    files.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(DiscoveredFiles { files, unavailable })
}

fn compute_source_fingerprints(
    source_scope: &ScopedSource,
    files: &[DiscoveredFile],
    job_indices: &[usize],
    include_full_sha: bool,
) -> HashMap<usize, Result<(String, Option<String>)>> {
    if job_indices.is_empty() {
        return HashMap::new();
    }
    let next = AtomicUsize::new(0);
    let out: Mutex<HashMap<usize, Result<(String, Option<String>)>>> =
        Mutex::new(HashMap::with_capacity(job_indices.len()));
    let workers = job_indices.len().clamp(1, FINGERPRINT_CONCURRENCY);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let pos = next.fetch_add(1, Ordering::Relaxed);
                if pos >= job_indices.len() {
                    break;
                }
                let idx = job_indices[pos];
                let entry = &files[idx];
                let res = (|| -> Result<(String, Option<String>)> {
                    let mut file = source_scope.open_file(&entry.key)?;
                    let before = file.metadata()?;
                    if before.len() != entry.size
                        || modified_nanos(&before)? != entry.modified_nanos
                    {
                        bail!("Source changed during planning");
                    }
                    let quick = quick_fingerprint_reader(&mut file, entry.size)?;
                    let full = if include_full_sha {
                        file.seek(SeekFrom::Start(0))?;
                        let (read_len, sha) = hash_reader(&mut file)?;
                        if read_len != entry.size {
                            bail!("Source changed during planning");
                        }
                        Some(sha)
                    } else {
                        None
                    };
                    let after = file.metadata()?;
                    if after.len() != entry.size || modified_nanos(&after)? != entry.modified_nanos
                    {
                        bail!("Source changed during planning");
                    }
                    Ok((quick, full))
                })();
                out.lock()
                    .expect("fingerprint map lock")
                    .insert(idx, res);
            });
        }
    });
    out.into_inner().expect("fingerprint map lock")
}

fn extension_role(key: &str) -> String {
    let extension = Path::new(key)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "jpg" => "jpeg".to_string(),
        "tif" => "tiff".to_string(),
        _ => extension,
    }
}

fn is_video_role(role: &str) -> bool {
    matches!(
        role,
        "mov" | "mp4" | "m4v" | "webm" | "3gp" | "avi" | "mkv" | "mts" | "m2ts"
    )
}

fn is_pairable(key: &str) -> bool {
    !is_video_role(&extension_role(key))
}

fn capture_group(key: &str) -> String {
    Path::new(key)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

fn resolve_capture_dates(
    source_scope: &ScopedSource,
    files: &[DiscoveredFile],
    warnings: &mut Vec<String>,
) -> HashMap<String, (Option<String>, String)> {
    let candidates: Vec<&DiscoveredFile> = files
        .iter()
        .filter(|entry| extension_role(&entry.key) != "xmp")
        .collect();
    let next = AtomicUsize::new(0);
    let probed: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
    if !candidates.is_empty() {
        let workers = candidates.len().clamp(1, CAPTURE_DATE_CONCURRENCY);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let pos = next.fetch_add(1, Ordering::Relaxed);
                    if pos >= candidates.len() {
                        break;
                    }
                    let entry = candidates[pos];
                    if let Some(day) = capture_day(source_scope, &entry.key, entry.size) {
                        probed
                            .lock()
                            .expect("capture date lock")
                            .insert(entry.key.clone(), day);
                    }
                });
            }
        });
    }
    let direct = probed.into_inner().expect("capture date lock");
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in files {
        if !is_pairable(&entry.key) || extension_role(&entry.key) == "xmp" {
            continue;
        }
        if let Some(date) = direct.get(&entry.key) {
            groups
                .entry(capture_group(&entry.key))
                .or_default()
                .insert(date.clone());
        }
    }
    for (group, dates) in &groups {
        if dates.len() > 1 {
            warnings.push(format!(
                "Conflicting EXIF capture dates for {group}: {}. Images retain their own dates; companions without an unambiguous date remain in unknown-date.",
                dates.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    files
        .iter()
        .map(|entry| {
            let key = &entry.key;
            let resolved = if let Some(date) = direct.get(key) {
                (Some(date.clone()), "exif".to_string())
            } else if is_pairable(key) {
                match groups.get(&capture_group(key)) {
                    Some(dates) if dates.len() == 1 => {
                        (dates.first().cloned(), "paired_exif".to_string())
                    }
                    _ => (None, "unknown".to_string()),
                }
            } else {
                (None, "unknown".to_string())
            };
            (key.clone(), resolved)
        })
        .collect()
}

fn same_dedup_role(target: &str, candidate: &str, preserve_name: bool) -> bool {
    let role = extension_role(target);
    role == extension_role(candidate) && (!(preserve_name || role == "xmp") || target == candidate)
}

fn dedup_identity(target: &str, hash: &str, preserve_name: bool) -> String {
    let role = extension_role(target);
    if preserve_name || role == "xmp" {
        format!("{role}\0{target}\0{hash}")
    } else {
        format!("{role}\0{hash}")
    }
}

/// Destination key and date provenance for one source file under `rules`.
fn target_for(
    rules: &OrganizeRules,
    key: &str,
    modified_day_value: Option<&str>,
    capture_dates: &HashMap<String, (Option<String>, String)>,
    anchors: &HashMap<String, String>,
    import_day: &str,
    seq: usize,
) -> Result<(String, String)> {
    let item_date = match rules.date_basis {
        DateBasis::Preserve | DateBasis::None => {
            let day = if rules.rename_mode == RenameMode::DatePrefixSeq {
                capture_dates
                    .get(key)
                    .and_then(|(d, _)| d.clone())
                    .or_else(|| {
                        (rules.unknown_date == UnknownDate::ModifiedTime)
                            .then(|| modified_day_value.map(str::to_owned))
                            .flatten()
                    })
            } else {
                None
            };
            ItemDate {
                day,
                source: "preserved".into(),
            }
        }
        DateBasis::Import => ItemDate {
            day: Some(import_day.to_owned()),
            source: "import_date".into(),
        },
        DateBasis::Capture => {
            let (day, provenance) = capture_dates
                .get(key)
                .cloned()
                .unwrap_or_else(|| (None, "unknown".to_string()));
            match day {
                Some(day) => ItemDate {
                    day: Some(day),
                    source: provenance,
                },
                None if rules.unknown_date == UnknownDate::ModifiedTime => ItemDate {
                    day: modified_day_value.map(str::to_owned),
                    source: "modified_time".into(),
                },
                None => ItemDate {
                    day: None,
                    source: provenance,
                },
            }
        }
    };
    let mut relative_path = organize::target_path_with_seq(rules, key, &item_date, seq)?;
    if let Some(anchor) = anchors.get(&capture_group(key)) {
        if extension_role(key) == "xmp" {
            let folder = organize::kind_folder(anchor);
            if let Some(index) = relative_path.rfind("/RAW/") {
                relative_path.replace_range(index + 1..index + 4, folder);
            }
        }
    }
    Ok((relative_path, item_date.source))
}

fn group_seq_key(key: &str) -> String {
    if is_pairable(key) {
        capture_group(key)
    } else {
        key.to_owned()
    }
}

fn group_sequences(source_files: &[String]) -> HashMap<String, usize> {
    let mut by_group = HashMap::new();
    let mut next_seq = 1usize;
    for key in source_files {
        let group = group_seq_key(key);
        if let std::collections::hash_map::Entry::Vacant(entry) = by_group.entry(group) {
            entry.insert(next_seq);
            next_seq += 1;
        }
    }
    by_group
}

fn prepare_backup_root(
    source: &Path,
    primary_destination: Option<&Path>,
    raw_backup: &Path,
) -> Result<(PathBuf, PreparedDestination)> {
    let backup = prospective_root(raw_backup)?;
    if backup.starts_with(source) || source.starts_with(&backup) {
        bail!("E_INVALID: Secondary backup destination must be separate from source");
    }
    if let Some(primary) = primary_destination {
        if backup.starts_with(primary) || primary.starts_with(&backup) {
            bail!(
                "E_INVALID: Secondary backup destination must be separate from primary destination"
            );
        }
    }
    let prepared = PreparedDestination::new(&backup)?;
    Ok((backup, prepared))
}

fn materialize_backup(
    source: &Path,
    primary_destination: Option<&Path>,
    planned: Option<&(PathBuf, PreparedDestination)>,
    override_backup: Option<&Path>,
) -> Result<Option<(PathBuf, ScopedDestination)>> {
    if let Some(raw) = override_backup {
        let (root, prepared) = prepare_backup_root(source, primary_destination, raw)?;
        return Ok(Some((root, prepared.materialize()?)));
    }
    if let Some((planned_root, prepared)) = planned {
        if prospective_root(planned_root)? != *planned_root {
            bail!("Backup destination root changed after planning");
        }
        return Ok(Some((planned_root.clone(), prepared.materialize()?)));
    }
    Ok(None)
}

fn filter_selected_items<'a>(
    items: &'a [ImportItem],
    preserve_names: &HashSet<String>,
    selected: Option<&HashSet<String>>,
) -> Result<(Vec<&'a ImportItem>, usize)> {
    let Some(selected_set) = selected else {
        return Ok((items.iter().collect(), 0));
    };
    let matches_direct = |item: &ImportItem| {
        selected_set.contains(&item.source) || selected_set.contains(&item.source_relative_path)
    };
    let selected_companion_groups: HashSet<String> = items
        .iter()
        .filter(|item| matches_direct(item) && preserve_names.contains(&item.source_relative_path))
        .map(|item| capture_group(&item.source_relative_path))
        .collect();
    let active: Vec<&'a ImportItem> = items
        .iter()
        .filter(|item| {
            matches_direct(item)
                || (preserve_names.contains(&item.source_relative_path)
                    && selected_companion_groups
                        .contains(&capture_group(&item.source_relative_path)))
        })
        .collect();
    ensure!(
        !active.is_empty(),
        "E_INVALID: Select at least one item to import"
    );
    let skipped = items.len().saturating_sub(active.len());
    Ok((active, skipped))
}

fn copy_to_backup(
    source_scope: &ScopedSource,
    backup_scoped: &ScopedDestination,
    item: &ImportItem,
    preserve_name: bool,
    known_sha256: Option<&str>,
    fp_cache: &mut HashMap<String, String>,
) -> Result<bool> {
    let expected = known_sha256.or_else(|| (!item.sha256.is_empty()).then_some(item.sha256.as_str()));
    if !backup_scoped.exists(&item.relative_path)? {
        let mut reader = VerifiedSourceReader::open(source_scope, item)?;
        backup_scoped.publish(&mut reader, &item.relative_path, expected)?;
        return Ok(true);
    }
    if existing_matches(source_scope, backup_scoped, &item.relative_path, item, fp_cache) {
        return Ok(false);
    }
    if preserve_name {
        bail!("Backup conflict in XMP companion group at {}", item.relative_path);
    }
    let suffix_sha = match expected {
        Some(sha) => sha.to_owned(),
        None => {
            let mut reader = VerifiedSourceReader::open(source_scope, item)?;
            let (_, sha) = hash_reader(&mut reader)?;
            sha
        }
    };
    for attempt in 1..100 {
        let key = collision_key(&item.relative_path, &suffix_sha, attempt)?;
        if backup_scoped.exists(&key)? {
            if existing_matches(source_scope, backup_scoped, &key, item, fp_cache) {
                return Ok(false);
            }
            continue;
        }
        let mut reader = VerifiedSourceReader::open(source_scope, item)?;
        backup_scoped.publish(&mut reader, &key, Some(&suffix_sha))?;
        return Ok(true);
    }
    bail!("Too many filename conflicts on backup destination")
}

/// With type folders, each companion group is anchored on its RAW (else first image).
fn group_anchors(rules: &OrganizeRules, source_files: &[String]) -> HashMap<String, String> {
    let mut by_group: HashMap<String, String> = HashMap::new();
    if rules.type_split != TypeSplit::ByKind {
        return by_group;
    }
    for key in source_files {
        if extension_role(key) == "xmp" || !is_pairable(key) {
            continue;
        }
        let entry = by_group
            .entry(capture_group(key))
            .or_insert_with(|| key.clone());
        if organize::kind_folder(key) == "RAW" {
            *entry = key.clone();
        }
    }
    by_group
}

fn join_key(prefix: &str, relative: &str) -> String {
    if prefix.is_empty() {
        relative.to_owned()
    } else {
        format!("{prefix}/{relative}")
    }
}

/// Members of XMP companion groups keep their exact names.
fn companion_names(files: &[String], unavailable: &[String]) -> HashSet<String> {
    let sidecars: HashSet<_> = files
        .iter()
        .chain(unavailable)
        .filter(|key| extension_role(key) == "xmp")
        .map(|key| capture_group(key))
        .collect();
    files
        .iter()
        .chain(unavailable)
        .filter(|key| is_pairable(key) && sidecars.contains(&capture_group(key)))
        .cloned()
        .collect()
}

/// A companion group with an unreadable member is excluded as a whole.
fn exclude_incomplete_groups(
    items: &mut Vec<ImportItem>,
    preserve_names: &HashSet<String>,
    warnings: &mut Vec<String>,
) {
    let planned: HashSet<_> = items
        .iter()
        .map(|item| item.source_relative_path.clone())
        .collect();
    let incomplete: BTreeSet<_> = preserve_names
        .iter()
        .filter(|key| !planned.contains(*key))
        .map(|key| capture_group(key))
        .collect();
    for group in &incomplete {
        warnings.push(format!(
            "Incomplete XMP companion group {group} was excluded from the plan because a source member could not be read. No member of this group will be imported; restore source access and create a new plan"
        ));
    }
    items.retain(|item| !incomplete.contains(&capture_group(&item.source_relative_path)));
}

fn modified_day(metadata: &fs::Metadata) -> Option<String> {
    let modified: DateTime<chrono::Local> = metadata.modified().ok()?.into();
    Some(modified.format("%Y-%m-%d").to_string())
}

struct BudgetedSeekReader {
    file: fs::File,
    len: u64,
    pos: u64,
    budget: usize,
}

impl BudgetedSeekReader {
    fn new(file: fs::File, len: u64, budget: usize) -> Self {
        Self {
            file,
            len,
            pos: 0,
            budget,
        }
    }
}

impl Read for BudgetedSeekReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        if self.budget == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Header metadata read budget exceeded",
            ));
        }
        let max_to_read = buf.len().min(self.budget);
        let count = self.file.read(&mut buf[..max_to_read])?;
        self.pos += count as u64;
        self.budget -= count;
        Ok(count)
    }
}

impl Seek for BudgetedSeekReader {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        let next: i128 = match style {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::Current(delta) => self.pos as i128 + delta as i128,
            SeekFrom::End(delta) => self.len as i128 + delta as i128,
        };
        if next < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Negative seek"));
        }
        let target = next as u64;
        self.pos = self.file.seek(SeekFrom::Start(target))?;
        Ok(self.pos)
    }
}

fn capture_day(source_scope: &ScopedSource, key: &str, size: u64) -> Option<String> {
    let role = extension_role(key);
    let mut file = source_scope.open_file(key).ok()?;
    if matches!(role.as_str(), "mp4" | "mov" | "m4v" | "3gp") {
        return quicktime_capture_day(&mut file, size);
    }
    let mut reader = BufReader::with_capacity(
        16 * 1024,
        BudgetedSeekReader::new(file, size, EXIF_READ_BUDGET),
    );
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    let field = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;
    let value = match &field.value {
        exif::Value::Ascii(values) => values.first()?,
        _ => return None,
    };
    let text = std::str::from_utf8(value).ok()?;
    let date = text.get(..10)?;
    chrono::NaiveDate::parse_from_str(date, "%Y:%m:%d")
        .ok()
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// Bounded ISO-BMFF / QuickTime `moov` atom seek for video capture day.
/// Skips `mdat` payloads via `Seek`, reading at most `VIDEO_META_BUDGET` bytes.
fn quicktime_capture_day(file: &mut fs::File, size: u64) -> Option<String> {
    const QT_EPOCH_OFFSET: u64 = 2_082_844_800; // 1904-01-01 .. 1970-01-01
    let mut offset = 0u64;
    let mut budget = VIDEO_META_BUDGET;
    for _ in 0..256 {
        if offset + 8 > size || budget < 8 {
            break;
        }
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut head = [0u8; 8];
        file.read_exact(&mut head).ok()?;
        budget -= 8;
        let raw_size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
        let fourcc = &head[4..8];
        let (header_len, box_size) = if raw_size == 1 {
            if offset + 16 > size || budget < 8 {
                break;
            }
            let mut ext = [0u8; 8];
            file.read_exact(&mut ext).ok()?;
            budget -= 8;
            (16u64, u64::from_be_bytes(ext))
        } else if raw_size == 0 {
            (8u64, size - offset)
        } else {
            (8u64, raw_size)
        };
        if box_size < header_len || offset.saturating_add(box_size) > size {
            break;
        }
        if fourcc == b"moov" {
            let payload_len = (box_size - header_len).min(budget as u64) as usize;
            let mut moov = vec![0u8; payload_len];
            file.read_exact(&mut moov).ok()?;
            if let Some(day) = parse_quicktime_iso_date_in_moov(&moov) {
                return Some(day);
            }
            if let Some(qt_secs) = parse_mvhd_creation_time(&moov) {
                if qt_secs > QT_EPOCH_OFFSET + 31_536_000 {
                    let unix_secs = (qt_secs - QT_EPOCH_OFFSET) as i64;
                    if let Some(dt) = DateTime::from_timestamp(unix_secs, 0) {
                        if (1971..=2100).contains(&dt.year()) {
                            return Some(dt.format("%Y-%m-%d").to_string());
                        }
                    }
                }
            }
            return None;
        }
        offset += box_size;
    }
    None
}

fn parse_quicktime_iso_date_in_moov(moov: &[u8]) -> Option<String> {
    let needle = b"com.apple.quicktime.creationdate";
    let _pos = moov.windows(needle.len()).position(|w| w == needle)?;
    // Scan ISO-8601 `YYYY-MM-DDT` strings inside `moov` metadata atoms.
    for window in moov.windows(11) {
        if window[4] == b'-' && window[7] == b'-' && window[10] == b'T' {
            if let Ok(day_str) = std::str::from_utf8(&window[..10]) {
                if let Ok(date) = chrono::NaiveDate::parse_from_str(day_str, "%Y-%m-%d") {
                    if (1971..=2100).contains(&date.year()) {
                        return Some(date.format("%Y-%m-%d").to_string());
                    }
                }
            }
        }
    }
    None
}

fn parse_mvhd_creation_time(moov: &[u8]) -> Option<u64> {
    let mut offset = 0usize;
    while offset + 8 <= moov.len() {
        let box_size = u32::from_be_bytes([
            moov[offset],
            moov[offset + 1],
            moov[offset + 2],
            moov[offset + 3],
        ]) as usize;
        if box_size < 8 || offset + box_size > moov.len() {
            break;
        }
        if &moov[offset + 4..offset + 8] == b"mvhd" {
            let body = &moov[offset + 8..offset + box_size];
            let version = *body.first()?;
            if version == 0 && body.len() >= 8 {
                return Some(u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as u64);
            } else if version == 1 && body.len() >= 12 {
                return Some(u64::from_be_bytes([
                    body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
                ]));
            }
            return None;
        }
        offset += box_size;
    }
    None
}
