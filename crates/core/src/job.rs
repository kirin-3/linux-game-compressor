use crate::{
    copy::{self, CopyOutcome},
    journal::{self, Journal, JournalEntry, Report},
    output::check_output,
    planning::{hash_input, validate_plan},
    registry::TransformerRegistry,
    scheduler::{transform_one_with_force, ConversionOutcome},
    tools::check_tools,
    Action, Plan, StoredPath,
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Component, Path},
    sync::{
        atomic::{AtomicI64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    sync::{broadcast, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    JobStart {
        total: usize,
    },
    FileStart {
        path: StoredPath,
        done: usize,
        total: usize,
        bytes_saved: i128,
    },
    FileResult {
        path: StoredPath,
        outcome: String,
        done: usize,
        total: usize,
        bytes_saved: i128,
    },
    JobEnd {
        done: usize,
        total: usize,
        bytes_saved: i128,
        complete: bool,
    },
}

/// Execute a validated plan; preflight happens before creating output files.
pub async fn run_job(
    plan: &Plan,
    output: &Path,
    registry: &TransformerRegistry,
    jobs: usize,
    cancel: CancellationToken,
    events: broadcast::Sender<Event>,
) -> io::Result<Report> {
    run_job_with_force(
        plan,
        output,
        registry,
        jobs,
        cancel,
        CancellationToken::new(),
        events,
    )
    .await
}

pub async fn run_job_with_force(
    plan: &Plan,
    output: &Path,
    registry: &TransformerRegistry,
    jobs: usize,
    cancel: CancellationToken,
    force: CancellationToken,
    events: broadcast::Sender<Event>,
) -> io::Result<Report> {
    if jobs == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "jobs must be positive",
        ));
    }
    validate_plan(plan, &plan.input_root.0)?;
    let output = check_output(&plan.input_root.0, output)?;
    let tools = check_tools(plan)?;
    let metadata = output.join(".compressor");
    let existing = if metadata.join("manifest.json").exists() {
        let manifest: crate::Manifest =
            serde_json::from_slice(&std::fs::read(metadata.join("manifest.json"))?)?;
        Some(manifest.plan)
    } else if metadata.join("plan.json").exists() {
        Some(serde_json::from_slice::<Plan>(&std::fs::read(
            metadata.join("plan.json"),
        )?)?)
    } else {
        None
    };
    if let Some(existing) = existing {
        if journal::fingerprint(&existing) != journal::fingerprint(plan) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output manifest belongs to a different plan",
            ));
        }
    }
    std::fs::create_dir_all(metadata.join("tmp"))?;
    journal::write_json_atomic(&metadata.join("plan.json"), plan)?;
    let journal = Arc::new(Mutex::new(Journal::open(&metadata.join("journal.jsonl"))?));
    let records = journal
        .lock()
        .map_err(|_| io::Error::other("journal lock poisoned"))?
        .records()?;
    let completed = journal::resumable_records(plan, &records, &output);
    let semaphore = Arc::new(Semaphore::new(jobs));
    let total = plan.entries.len();
    let _ = events.send(Event::JobStart { total });
    let mut done = 0usize;
    let mut saved = 0i128;
    let mut workers = JoinSet::new();
    let plan_hash = crate::journal::fingerprint(plan);
    // Shared (done, bytes saved) snapshot for start events sent from workers.
    let progress = Arc::new((AtomicUsize::new(0), AtomicI64::new(0)));
    for (index, entry) in plan.entries.iter().enumerate() {
        if let Some(record) = completed.get(&entry.path) {
            done += 1;
            saved += i128::from(record.input_size) - i128::from(record.output_size);
            progress.0.store(done, Ordering::Relaxed);
            progress.1.store(saved as i64, Ordering::Relaxed);
            let _ = events.send(Event::FileResult {
                path: entry.path.clone(),
                outcome: record.outcome.clone(),
                done,
                total,
                bytes_saved: saved,
            });
            continue;
        }
        let transformer = match &entry.action {
            Action::Convert { kind, .. } => Some(
                registry
                    .get(kind)
                    .ok_or_else(|| io::Error::other("missing transformer"))?,
            ),
            _ => None,
        };
        let entry = entry.clone();
        let root = plan.input_root.0.clone();
        let output = output.clone();
        let temporary = metadata.join("tmp").join(format!("{index}.stage"));
        let token = cancel.clone();
        let force = force.clone();
        let semaphore = semaphore.clone();
        let plan_hash = plan_hash.clone();
        let events = events.clone();
        let progress = progress.clone();
        workers.spawn(async move {
            let _permit = tokio::select! { p=semaphore.acquire_owned()=>p.map_err(io::Error::other)?, _=token.cancelled()=>return Err(io::Error::new(io::ErrorKind::Interrupted,"cancelled")) };
            if token.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted,"cancelled")); }
            let _ = events.send(Event::FileStart {
                path: entry.path.clone(),
                done: progress.0.load(Ordering::Relaxed),
                total,
                bytes_saved: i128::from(progress.1.load(Ordering::Relaxed)),
            });
            let path = entry.path.clone(); let destination=output.join(&path.0); ensure_output_parent(&output, &path.0)?;
            let action = async {
                let (outcome, failure, exists) = match (&entry.action, transformer) {
                    (Action::Convert { settings, .. }, Some(t)) => match transform_one_with_force(root.join(&path.0), destination.clone(), temporary.clone(), settings.clone(), t, &token, &force).await? {
                        ConversionOutcome::Converted => ("converted; metadata dropped".into(), false, true),
                        ConversionOutcome::KeptOriginal { reason, failed } => (reason, failed, true),
                    },
                    (Action::Copy { reason } | Action::Skip { reason }, _) => {
                        match copy::stage_copy_entry(&root, &output, &path.0, &temporary)? {
                            CopyOutcome::Copied => { if token.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted,"cancelled")); } tokio::fs::rename(&temporary, &destination).await?; (reason.clone(), false, true) },
                            CopyOutcome::Skipped(reason) => (reason, false, false),
                        }
                    },
                    _ => return Err(io::Error::other("missing transformer")),
                };
                let (size, hash) = if exists { let (size, hash)=hash_input(&destination)?; (size,Some(hash)) } else { (0,None) };
                Ok(JournalEntry { path, plan_hash, input_hash:entry.hash, output_hash:hash, input_size:entry.size, output_size:size, outcome, failure })
            }.await;
            if action.is_err() { let _=tokio::fs::remove_file(&temporary).await; }
            action
        });
    }
    let mut first_error = None;
    while let Some(result) = workers.join_next().await {
        match result {
            Ok(Ok(record)) => {
                journal
                    .lock()
                    .map_err(|_| io::Error::other("journal lock poisoned"))?
                    .append(&record)?;
                done += 1;
                saved += i128::from(record.input_size) - i128::from(record.output_size);
                progress.0.store(done, Ordering::Relaxed);
                progress.1.store(saved as i64, Ordering::Relaxed);
                let _ = events.send(Event::FileResult {
                    path: record.path,
                    outcome: record.outcome,
                    done,
                    total,
                    bytes_saved: saved,
                });
            }
            Ok(Err(e)) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(io::Error::other(e));
                }
            }
        }
    }
    let journal = journal
        .lock()
        .map_err(|_| io::Error::other("journal lock poisoned"))?;
    if cancel.is_cancelled() {
        journal::finish_cancelled(plan, &output, &journal)?;
    }
    let records = journal.records()?;
    let complete = first_error.is_none() && !cancel.is_cancelled();
    let mut manifest = journal::make_manifest(plan.clone(), &records, complete);
    manifest.tool_versions = tools;
    let report = journal::build_report(&manifest);
    journal::write_json_atomic(&metadata.join("manifest.json"), &manifest)?;
    journal::write_json_atomic(&metadata.join("report.json"), &report)?;
    let _ = events.send(Event::JobEnd {
        done,
        total,
        bytes_saved: saved,
        complete,
    });
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(report)
}

/// Never traverse output symlinks when creating a destination parent.
fn ensure_output_parent(output: &Path, relative: &Path) -> io::Result<()> {
    let mut current = output.to_owned();
    let parent = relative
        .parent()
        .ok_or_else(|| io::Error::other("invalid relative path"))?;
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe output path",
            ));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "output parent is not a directory",
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match std::fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                };
                let m = std::fs::symlink_metadata(&current)?;
                if !m.is_dir() || m.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "output parent is not a directory",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    if relative.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid output filename",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        registry::{EngineAdapter, Operation, Transformer},
        EngineInfo, MediaKind, Profile, ToolSpec,
    };
    use std::{collections::BTreeMap, os::unix::fs::MetadataExt};
    struct Adapter;
    impl EngineAdapter for Adapter {
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            unreachable!()
        }
        fn classify(
            &self,
            _: &Path,
            path: &Path,
            _: &EngineInfo,
            profile: &Profile,
        ) -> io::Result<Action> {
            if path == Path::new("game/images/opaque.png") {
                Ok(Action::Convert {
                    kind: MediaKind::Image,
                    target: "webp".into(),
                    encoder: "fake".into(),
                    settings: profile.clone(),
                })
            } else {
                Ok(Action::Copy {
                    reason: "copy fixture".into(),
                })
            }
        }
    }
    struct Fake {
        fail: bool,
        wait: bool,
    }
    impl Transformer for Fake {
        fn kind(&self) -> MediaKind {
            MediaKind::Image
        }
        fn required_tools(&self) -> Vec<ToolSpec> {
            vec![]
        }
        fn transform<'a>(
            &'a self,
            _: &'a Path,
            dest: &'a Path,
            _: &'a Profile,
            cancel: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                if self.wait {
                    tokio::select! { _=cancel.cancelled()=>return Err(io::Error::new(io::ErrorKind::Interrupted,"cancelled")), _=tokio::time::sleep(std::time::Duration::from_secs(2))=>{} }
                }
                if self.fail {
                    return Err(io::Error::other("fake failure"));
                }
                tokio::fs::write(dest, b"tiny").await
            })
        }
        fn validate<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    fn snapshot(root: &Path) -> io::Result<BTreeMap<std::path::PathBuf, (String, u32, i64, i64)>> {
        let mut result = BTreeMap::new();
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let entry = entry.map_err(io::Error::other)?;
            if entry.file_type().is_dir() {
                continue;
            }
            let meta = std::fs::symlink_metadata(entry.path())?;
            let (_, hash) = hash_input(entry.path())?;
            result.insert(
                entry.path().strip_prefix(root).unwrap().to_owned(),
                (hash, meta.mode(), meta.mtime(), meta.mtime_nsec()),
            );
        }
        Ok(result)
    }
    #[tokio::test]
    async fn full_job_preserves_inputs_and_reports_every_result() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let input = fixture.root().canonicalize()?;
        let initial = snapshot(&input)?;
        let dir = tempfile::tempdir()?;
        for mode in ["success", "failure", "cancel"] {
            let mut registry = TransformerRegistry::default();
            registry.register(Arc::new(Fake {
                fail: mode == "failure",
                wait: mode == "cancel",
            }));
            let engine = EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            };
            let plan = crate::planning::build_plan(
                &input,
                &Adapter,
                engine,
                Profile::default(),
                &registry,
            )?;
            let (sender, mut receiver) = broadcast::channel(4096);
            let cancel = CancellationToken::new();
            if mode == "cancel" {
                let token = cancel.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    token.cancel();
                });
            }
            let out = dir.path().join(mode);
            let result = run_job(&plan, &out, &registry, 2, cancel, sender).await;
            if mode == "cancel" {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
                let r: Report =
                    serde_json::from_slice(&std::fs::read(out.join(".compressor/report.json"))?)?;
                assert!(!r.complete);
            } else {
                let report = result?;
                assert!(report.complete);
                assert_eq!(report.failures.len(), usize::from(mode == "failure"));
                let mut count = 0;
                while let Ok(event) = receiver.try_recv() {
                    if matches!(event, Event::FileResult { .. }) {
                        count += 1;
                    }
                }
                assert_eq!(count, plan.entries.len());
                let mut total = 0;
                for entry in walkdir::WalkDir::new(&out).follow_links(false) {
                    let entry = entry.map_err(io::Error::other)?;
                    if entry.path().starts_with(out.join(".compressor"))
                        || entry.file_type().is_dir()
                    {
                        continue;
                    }
                    total += std::fs::symlink_metadata(entry.path())?.len();
                }
                assert_eq!(report.output_bytes, total);
            }
            assert_eq!(snapshot(&input)?, initial, "input changed during {mode}");
        }
        Ok(())
    }
}
