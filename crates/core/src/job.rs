use crate::{
    copy::{self, CopyOutcome},
    journal::{self, Journal, JournalEntry, Report},
    output::check_output,
    planning::{hash_input, validate_plan_with_cancel},
    registry::{
        ArchiveInput, ArchiveRegistry, EnvelopeRegistry, PackRegistry, TransformerRegistry,
    },
    scheduler::{transform_one_with_force, ConversionOutcome},
    tools::check_tools,
    Action, Plan, StoredPath,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
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
    run_job_with_packs(
        plan,
        output,
        registry,
        &PackRegistry::default(),
        jobs,
        cancel,
        force,
        events,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_job_with_packs(
    plan: &Plan,
    output: &Path,
    registry: &TransformerRegistry,
    packs: &PackRegistry,
    jobs: usize,
    cancel: CancellationToken,
    force: CancellationToken,
    events: broadcast::Sender<Event>,
) -> io::Result<Report> {
    run_job_with_envelopes(
        plan,
        output,
        registry,
        packs,
        &EnvelopeRegistry::default(),
        jobs,
        cancel,
        force,
        events,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_job_with_envelopes(
    plan: &Plan,
    output: &Path,
    registry: &TransformerRegistry,
    packs: &PackRegistry,
    envelopes: &EnvelopeRegistry,
    jobs: usize,
    cancel: CancellationToken,
    force: CancellationToken,
    events: broadcast::Sender<Event>,
) -> io::Result<Report> {
    run_job_with_archives(
        plan,
        output,
        registry,
        packs,
        envelopes,
        &ArchiveRegistry::default(),
        jobs,
        cancel,
        force,
        events,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_job_with_archives(
    plan: &Plan,
    output: &Path,
    registry: &TransformerRegistry,
    packs: &PackRegistry,
    envelopes: &EnvelopeRegistry,
    archives: &ArchiveRegistry,
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
    validate_plan_with_cancel(plan, &plan.input_root.0, &cancel)?;
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let output = check_output(&plan.input_root.0, output)?;
    let metadata = output.join(".compressor");
    let existing = if metadata.join("manifest.json").exists() {
        let manifest = crate::planning::read_manifest(&metadata.join("manifest.json"))?;
        Some(manifest.plan)
    } else if metadata.join("plan.json").exists() {
        Some(crate::planning::read_plan(&metadata.join("plan.json"))?)
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
    // Cache each source directory once; reject malformed plans before creating output.
    let shared_plan = Arc::new(plan.clone());
    let shared_packs = packs.clone();
    let shared_envelopes = envelopes.clone();
    let mut pack_dirs = HashMap::new();
    for entry in &plan.entries {
        if let Action::Assemble { format, .. } = &entry.action {
            if archives.get(format).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing archive format: {format}"),
                ));
            }
        }
        if let Action::Rebuild { format } = &entry.action {
            let handler = packs.get(format).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing pack format: {format}"),
                )
            })?;
            let directory = handler.read_directory(&plan.input_root.0.join(&entry.path.0))?;
            pack_dirs.insert(entry.path.clone(), (handler, directory));
        }
    }
    for entry in plan.entries.iter().filter(|entry| entry.member.is_some()) {
        let (_, directory) = pack_dirs.get(&entry.path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "member has no rebuild entry")
        })?;
        if !directory
            .iter()
            .any(|item| Some(&item.name) == entry.member.as_ref())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "member missing from pack",
            ));
        }
    }
    let tools = check_tools(plan)?;
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
        if matches!(
            entry.action,
            Action::Rebuild { .. } | Action::Archive { .. } | Action::Assemble { .. }
        ) {
            continue;
        }
        if let Some(record) = completed.get(&(entry.path.clone(), entry.member.clone())) {
            done += 1;
            if record.member.is_none() {
                saved += i128::from(record.input_size) - i128::from(record.output_size);
            }
            progress.0.store(done, Ordering::Relaxed);
            progress.1.store(saved as i64, Ordering::Relaxed);
            let _ = events.send(Event::FileResult {
                path: journal::display_path(&entry.path, entry.member.as_deref()),
                outcome: record.outcome.clone(),
                done,
                total,
                bytes_saved: saved,
            });
            continue;
        }
        let transformer = match &entry.action {
            Action::Convert { encoder, .. } => Some(
                registry
                    .get(encoder)
                    .ok_or_else(|| io::Error::other("missing transformer"))?,
            ),
            _ => None,
        };
        let envelope = match &entry.action {
            Action::Convert {
                envelope: Some(envelope),
                ..
            } => Some((
                envelopes.get(&envelope.format).ok_or_else(|| {
                    io::Error::other(format!("missing envelope format: {}", envelope.format))
                })?,
                envelope.params.clone(),
            )),
            _ => None,
        };
        let pack_member = entry.member.as_ref().map(|name| {
            let (handler, directory) = &pack_dirs[&entry.path];
            (
                handler.clone(),
                directory
                    .iter()
                    .find(|item| &item.name == name)
                    .unwrap()
                    .clone(),
            )
        });
        let entry = entry.clone();
        let stage_plan = shared_plan.clone();
        let stage_packs = shared_packs.clone();
        let stage_envelopes = shared_envelopes.clone();
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
                path: journal::display_path(&entry.path, entry.member.as_deref()),
                done: progress.0.load(Ordering::Relaxed),
                total,
                bytes_saved: i128::from(progress.1.load(Ordering::Relaxed)),
            });
            let path = entry.path.clone();
            let action = async {
                if let Some((pack, member)) = pack_member {
                    let source = temporary.with_extension("src");
                    let destination = output.join(".compressor/members").join(index.to_string());
                    let (outcome, failure, hash, size) = match (&entry.action, transformer) {
                        (Action::Convert { settings, .. }, Some(t)) => {
                            let bytes = pack.read_member(&root.join(&path.0), &member)?;
                            if bytes.len() as u64 != entry.size || blake3::hash(&bytes).to_hex().as_str() != entry.hash {
                                return Err(io::Error::new(io::ErrorKind::InvalidData, "stale pack member"));
                            }
                            let staged = envelope.as_ref().map(|_| crate::planning::stage_bytes(bytes.clone(), &entry.action, &stage_envelopes, temporary.parent().unwrap()));
                            tokio::fs::write(&source, bytes).await?;
                            let result = if let (Some((handler, params)), Some(staged)) = (envelope, staged) {
                                crate::scheduler::transform_enveloped(source.clone(), destination.clone(), temporary.clone(), settings.clone(), t, handler, params, staged, &token, &force).await
                            } else {
                                transform_one_with_force(source.clone(), destination.clone(), temporary.clone(), settings.clone(), t, &token, &force).await
                            };
                            let _ = tokio::fs::remove_file(&source).await;
                            match result? {
                                ConversionOutcome::Converted { note } => {
                                    let (size, hash) = hash_input(&destination)?;
                                    (note.map_or_else(|| "converted".to_owned(), |n| format!("converted; {n}")), false, hash, size)
                                }
                                ConversionOutcome::KeptOriginal { reason, failed } => {
                                    let _ = tokio::fs::remove_file(&destination).await;
                                    (reason, failed, entry.hash.clone(), entry.size)
                                }
                            }
                        }
                        (Action::Copy { reason } | Action::Skip { reason }, _) => (reason.clone(), false, entry.hash.clone(), entry.size),
                        _ => return Err(io::Error::other("invalid member action")),
                    };
                    Ok(JournalEntry { path, member: entry.member, plan_hash, input_hash: entry.hash, output_hash: Some(hash), input_size: entry.size, output_size: size, outcome, failure })
                } else {
                    let destination=output.join(&path.0); ensure_output_parent(&output, &path.0)?;
                    let (outcome, failure, exists) = match (&entry.action, transformer) {
                        (Action::Convert { settings, .. }, Some(t)) => match if let Some((handler, params)) = envelope {
                            crate::scheduler::transform_enveloped(root.join(&path.0), destination.clone(), temporary.clone(), settings.clone(), t, handler, params, crate::planning::stage_entry_source(&root, &stage_plan, &entry, &stage_packs, &stage_envelopes, temporary.parent().unwrap()), &token, &force).await?
                        } else {
                            transform_one_with_force(root.join(&path.0), destination.clone(), temporary.clone(), settings.clone(), t, &token, &force).await?
                        } {
                            ConversionOutcome::Converted { note } => (note.map_or_else(|| "converted".to_owned(), |n| format!("converted; {n}")), false, true),
                            ConversionOutcome::KeptOriginal { reason, failed } => (reason, failed, true),
                        },
                        (Action::Remove { .. }, _) => {
                            match tokio::fs::remove_file(&destination).await {
                                Ok(()) => {},
                                Err(e) if e.kind() == io::ErrorKind::NotFound => {},
                                Err(e) => return Err(e),
                            }
                            ("removed".to_owned(), false, false)
                        },
                        (Action::Copy { reason } | Action::Skip { reason }, _) => match copy::stage_copy_entry(&root, &output, &path.0, &temporary)? {
                            CopyOutcome::Copied => { if token.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted,"cancelled")); } tokio::fs::rename(&temporary, &destination).await?; (reason.clone(), false, true) },
                            CopyOutcome::Skipped(reason) => (reason, false, false),
                        },
                        _ => return Err(io::Error::other("missing transformer")),
                    };
                    let (size, hash) = if exists { let (size, hash)=hash_input(&destination)?; (size,Some(hash)) } else { (0,None) };
                    Ok(JournalEntry { path, member: None, plan_hash, input_hash:entry.hash, output_hash:hash, input_size:entry.size, output_size:size, outcome, failure })
                }
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
                if record.member.is_none() {
                    saved += i128::from(record.input_size) - i128::from(record.output_size);
                }
                progress.0.store(done, Ordering::Relaxed);
                progress.1.store(saved as i64, Ordering::Relaxed);
                let _ = events.send(Event::FileResult {
                    path: journal::display_path(&record.path, record.member.as_deref()),
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
    // Rebuild only after all member work has finished successfully.
    if first_error.is_none() && !cancel.is_cancelled() {
        let current = journal
            .lock()
            .map_err(|_| io::Error::other("journal lock poisoned"))?
            .records()?;
        let latest = journal::make_manifest(plan.clone(), &current, false);
        for (index, entry) in plan
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.action, Action::Rebuild { .. }))
        {
            if cancel.is_cancelled() {
                break;
            }
            let all_members_resumed = plan
                .entries
                .iter()
                .filter(|member| member.path == entry.path && member.member.is_some())
                .all(|member| {
                    completed.contains_key(&(member.path.clone(), member.member.clone()))
                });
            if let Some(record) = completed
                .get(&(entry.path.clone(), None))
                .filter(|_| all_members_resumed)
            {
                done += 1;
                saved += i128::from(record.input_size) - i128::from(record.output_size);
                let _ = events.send(Event::FileResult {
                    path: entry.path.clone(),
                    outcome: record.outcome.clone(),
                    done,
                    total,
                    bytes_saved: saved,
                });
                continue;
            }
            let _ = events.send(Event::FileStart {
                path: entry.path.clone(),
                done,
                total,
                bytes_saved: saved,
            });
            let mut replacements = BTreeMap::new();
            for (member_index, member) in plan
                .entries
                .iter()
                .enumerate()
                .filter(|(_, m)| m.path == entry.path && m.member.is_some())
            {
                if latest.entries.iter().any(|record| {
                    record.path == member.path
                        && record.member == member.member
                        && record.outcome.starts_with("converted")
                }) {
                    replacements.insert(
                        member.member.clone().unwrap(),
                        metadata.join("members").join(member_index.to_string()),
                    );
                }
            }
            let temporary = metadata.join("tmp").join(format!("{index}.pack"));
            let source = plan.input_root.0.join(&entry.path.0);
            let destination = output.join(&entry.path.0);
            ensure_output_parent(&output, &entry.path.0)?;
            let (outcome, failure) = if replacements.is_empty() {
                copy::stage_copy_entry(&plan.input_root.0, &output, &entry.path.0, &temporary)?;
                ("no member converted".to_string(), false)
            } else {
                match pack_dirs[&entry.path]
                    .0
                    .rebuild(&source, &temporary, &replacements)
                {
                    Ok(()) => ("rebuilt".to_string(), false),
                    Err(error) => {
                        let _ = std::fs::remove_file(&temporary);
                        copy::stage_copy_entry(
                            &plan.input_root.0,
                            &output,
                            &entry.path.0,
                            &temporary,
                        )?;
                        (format!("rebuild failed: {error}"), true)
                    }
                }
            };
            if cancel.is_cancelled() {
                let _ = std::fs::remove_file(&temporary);
                break;
            }
            std::fs::rename(&temporary, &destination)?;
            let (size, hash) = hash_input(&destination)?;
            let record = JournalEntry {
                path: entry.path.clone(),
                member: None,
                plan_hash: plan_hash.clone(),
                input_hash: entry.hash.clone(),
                output_hash: Some(hash),
                input_size: entry.size,
                output_size: size,
                outcome: outcome.clone(),
                failure,
            };
            journal
                .lock()
                .map_err(|_| io::Error::other("journal lock poisoned"))?
                .append(&record)?;
            done += 1;
            saved += i128::from(record.input_size) - i128::from(record.output_size);
            let _ = events.send(Event::FileResult {
                path: entry.path.clone(),
                outcome,
                done,
                total,
                bytes_saved: saved,
            });
        }
    }
    if first_error.is_none() && !cancel.is_cancelled() {
        for (index, entry) in plan
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.action, Action::Assemble { .. }))
        {
            let Action::Assemble {
                format,
                params,
                members,
            } = &entry.action
            else {
                unreachable!()
            };
            let handler = archives
                .get(format)
                .expect("preflight checked archive format");
            let parent = entry.path.0.parent().unwrap_or(Path::new(""));
            let gathered: Vec<_> = members.iter().map(|name| plan.entries.iter().find(|e| e.path.0 == parent.join(name) && matches!(&e.action, Action::Archive { archive } if archive == &entry.path)).expect("validated archive member")).collect();
            let inputs: Vec<_> = gathered
                .iter()
                .zip(members)
                .map(|(member, name)| ArchiveInput {
                    name: name.clone(),
                    path: plan.input_root.0.join(&member.path.0),
                    size: member.size,
                    hash: member.hash.clone(),
                })
                .collect();
            let latest = journal::make_manifest(plan.clone(), &records, false);
            let resumed = completed
                .get(&(entry.path.clone(), None))
                .filter(|record| record.outcome == "archived")
                .filter(|_| {
                    gathered.iter().all(|member| {
                        !output.join(&member.path.0).exists()
                            && latest.entries.iter().any(|record| {
                                record.path == member.path
                                    && record.member.is_none()
                                    && record.outcome == "archived"
                                    && record.input_hash == member.hash
                            })
                    })
                });
            if let Some(record) = resumed {
                for member in &gathered {
                    done += 1;
                    saved += i128::from(member.size);
                    let _ = events.send(Event::FileResult {
                        path: member.path.clone(),
                        outcome: "archived".into(),
                        done,
                        total,
                        bytes_saved: saved,
                    });
                }
                done += 1;
                saved -= i128::from(record.output_size);
                let _ = events.send(Event::FileResult {
                    path: entry.path.clone(),
                    outcome: record.outcome.clone(),
                    done,
                    total,
                    bytes_saved: saved,
                });
                continue;
            }
            let temporary = metadata.join("tmp").join(format!("{index}.archive"));
            ensure_output_parent(&output, &entry.path.0)?;
            match std::fs::remove_file(output.join(&entry.path.0)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let _ = events.send(Event::FileStart {
                path: entry.path.clone(),
                done,
                total,
                bytes_saved: saved,
            });
            let result = (|| -> io::Result<()> {
                handler.write(&inputs, &temporary, params, &cancel)?;
                let directory = handler.read_directory(&temporary)?;
                if directory.len() != inputs.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive directory does not match plan",
                    ));
                }
                for input in &inputs {
                    let member =
                        directory
                            .iter()
                            .find(|m| m.name == input.name)
                            .ok_or_else(|| {
                                io::Error::new(io::ErrorKind::InvalidData, "archive member missing")
                            })?;
                    let (size, hash) = handler.hash_member(&temporary, member)?;
                    if size != input.size || hash != input.hash {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "archive member mismatch",
                        ));
                    }
                }
                if std::fs::metadata(&temporary)?.len() >= entry.size {
                    return Err(io::Error::other("archive not smaller"));
                }
                if cancel.is_cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                ensure_output_parent(&output, &entry.path.0)?;
                std::fs::rename(&temporary, output.join(&entry.path.0))?;
                Ok(())
            })();
            if let Err(error) = result {
                let _ = std::fs::remove_file(&temporary);
                if cancel.is_cancelled() {
                    break;
                }
                if error.kind() == io::ErrorKind::InvalidData
                    && error.to_string().contains("stale archive member")
                {
                    first_error = Some(error);
                    break;
                }
                let not_smaller = error.to_string() == "archive not smaller";
                let outcome = if not_smaller {
                    "kept original: not smaller".to_owned()
                } else {
                    format!("archive failed: {error}")
                };
                for member in &gathered {
                    let source = plan.input_root.0.join(&member.path.0);
                    if hash_input(&source)? != (member.size, member.hash.clone()) {
                        first_error = Some(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("stale archive member: {}", member.path.0.display()),
                        ));
                        break;
                    }
                    ensure_output_parent(&output, &member.path.0)?;
                    copy::stage_copy_entry(
                        &plan.input_root.0,
                        &output,
                        &member.path.0,
                        &temporary,
                    )?;
                    if hash_input(&temporary)? != (member.size, member.hash.clone()) {
                        let _ = std::fs::remove_file(&temporary);
                        first_error = Some(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("stale archive member: {}", member.path.0.display()),
                        ));
                        break;
                    }
                    std::fs::rename(&temporary, output.join(&member.path.0))?;
                    let record = JournalEntry {
                        path: member.path.clone(),
                        member: None,
                        plan_hash: plan_hash.clone(),
                        input_hash: member.hash.clone(),
                        output_hash: Some(member.hash.clone()),
                        input_size: member.size,
                        output_size: member.size,
                        outcome: "copied".into(),
                        failure: false,
                    };
                    journal
                        .lock()
                        .map_err(|_| io::Error::other("journal lock poisoned"))?
                        .append(&record)?;
                    done += 1;
                    let _ = events.send(Event::FileResult {
                        path: member.path.clone(),
                        outcome: record.outcome,
                        done,
                        total,
                        bytes_saved: saved,
                    });
                }
                if first_error.is_some() {
                    break;
                }
                let _ = std::fs::remove_file(output.join(&entry.path.0));
                let record = JournalEntry {
                    path: entry.path.clone(),
                    member: None,
                    plan_hash: plan_hash.clone(),
                    input_hash: entry.hash.clone(),
                    output_hash: None,
                    input_size: 0,
                    output_size: 0,
                    outcome: outcome.clone(),
                    failure: !not_smaller,
                };
                journal
                    .lock()
                    .map_err(|_| io::Error::other("journal lock poisoned"))?
                    .append(&record)?;
                done += 1;
                let _ = events.send(Event::FileResult {
                    path: entry.path.clone(),
                    outcome,
                    done,
                    total,
                    bytes_saved: saved,
                });
                continue;
            }
            for member in &gathered {
                ensure_output_parent(&output, &member.path.0)?;
                match std::fs::remove_file(output.join(&member.path.0)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            for member in gathered {
                let record = JournalEntry {
                    path: member.path.clone(),
                    member: None,
                    plan_hash: plan_hash.clone(),
                    input_hash: member.hash.clone(),
                    output_hash: None,
                    input_size: member.size,
                    output_size: 0,
                    outcome: "archived".into(),
                    failure: false,
                };
                journal
                    .lock()
                    .map_err(|_| io::Error::other("journal lock poisoned"))?
                    .append(&record)?;
                done += 1;
                saved += i128::from(member.size);
                let _ = events.send(Event::FileResult {
                    path: member.path.clone(),
                    outcome: record.outcome,
                    done,
                    total,
                    bytes_saved: saved,
                });
            }
            let (size, hash) = hash_input(&output.join(&entry.path.0))?;
            let record = JournalEntry {
                path: entry.path.clone(),
                member: None,
                plan_hash: plan_hash.clone(),
                input_hash: entry.hash.clone(),
                output_hash: Some(hash),
                input_size: 0,
                output_size: size,
                outcome: "archived".into(),
                failure: false,
            };
            journal
                .lock()
                .map_err(|_| io::Error::other("journal lock poisoned"))?
                .append(&record)?;
            done += 1;
            saved -= i128::from(size);
            let _ = events.send(Event::FileResult {
                path: entry.path.clone(),
                outcome: record.outcome,
                done,
                total,
                bytes_saved: saved,
            });
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
    if complete {
        // Every pack is rebuilt or restored, so staged member data is no longer needed.
        match std::fs::remove_dir_all(metadata.join("members")) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
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
        EngineInfo, MediaKind, ToolSpec,
    };
    use std::{collections::BTreeMap, os::unix::fs::MetadataExt};
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_validation_preserves_destinations() -> io::Result<()> {
        use std::{
            ffi::CString,
            os::fd::{AsRawFd, FromRawFd},
            os::unix::ffi::OsStrExt,
            time::{Duration, Instant},
        };
        let fixture = tempfile::tempdir()?;
        let input = fixture.path().join("input");
        std::fs::create_dir(&input)?;
        let data = input.join("large.dat");
        std::fs::File::create(&data)?.set_len(1024 * 1024 * 1024)?;
        let plan = Plan {
            plan_version: 5,
            input_root: crate::StoredPath(input.canonicalize()?),
            engine: EngineInfo {
                name: "test".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: crate::ResolvedSettings::default(),
            tools: vec![],
            entries: vec![crate::Entry {
                path: crate::StoredPath("large.dat".into()),
                member: None,
                size: 1024 * 1024 * 1024,
                hash: "unchanged".into(),
                action: Action::Copy {
                    reason: "test".into(),
                },
            }],
        };
        for existing in [false, true] {
            let output = fixture
                .path()
                .join(if existing { "existing" } else { "new" });
            let plan_bytes = serde_json::to_vec(&plan)?;
            let record = JournalEntry {
                path: crate::StoredPath("large.dat".into()),
                member: None,
                plan_hash: crate::journal::fingerprint(&plan),
                input_hash: "unchanged".into(),
                output_hash: None,
                input_size: 1024 * 1024 * 1024,
                output_size: 0,
                outcome: "pending".into(),
                failure: false,
            };
            let mut journal_bytes = serde_json::to_vec(&record)?;
            journal_bytes.push(b'\n');
            if existing {
                std::fs::create_dir_all(output.join(".compressor"))?;
                std::fs::write(output.join(".compressor/plan.json"), &plan_bytes)?;
                std::fs::write(output.join(".compressor/journal.jsonl"), &journal_bytes)?;
            }
            let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let watch = unsafe { std::fs::File::from_raw_fd(fd) };
            let name = CString::new(data.as_os_str().as_bytes()).map_err(io::Error::other)?;
            if unsafe { libc::inotify_add_watch(watch.as_raw_fd(), name.as_ptr(), libc::IN_OPEN) }
                < 0
            {
                return Err(io::Error::last_os_error());
            }
            let token = CancellationToken::new();
            let signal = token.clone();
            let watcher = tokio::task::spawn_blocking(move || -> io::Result<Instant> {
                let mut poll = libc::pollfd {
                    fd: watch.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut poll, 1, 5000) };
                if result != 1 {
                    return Err(io::Error::other("validation did not open the input"));
                }
                let started = Instant::now();
                signal.cancel();
                Ok(started)
            });
            let input_plan = plan.clone();
            let target = output.clone();
            let job = tokio::spawn(async move {
                let (events, _) = tokio::sync::broadcast::channel(16);
                run_job_with_archives(
                    &input_plan,
                    &target,
                    &TransformerRegistry::default(),
                    &PackRegistry::default(),
                    &EnvelopeRegistry::default(),
                    &ArchiveRegistry::default(),
                    1,
                    token,
                    CancellationToken::new(),
                    events,
                )
                .await
            });
            let cancelled_at = watcher.await.map_err(io::Error::other)??;
            let result = tokio::time::timeout(Duration::from_secs(2), job)
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?;
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
            assert!(cancelled_at.elapsed() < Duration::from_secs(2));
            if existing {
                assert_eq!(
                    std::fs::read(output.join(".compressor/plan.json"))?,
                    plan_bytes
                );
                assert_eq!(
                    std::fs::read(output.join(".compressor/journal.jsonl"))?,
                    journal_bytes
                );
                assert_eq!(
                    crate::journal::fingerprint(&crate::planning::read_plan(
                        &output.join(".compressor/plan.json")
                    )?),
                    crate::journal::fingerprint(&plan)
                );
            } else {
                assert!(!output.exists());
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_during_large_copy_is_prompt_and_resumable() -> io::Result<()> {
        use std::{
            ffi::CString,
            os::fd::{AsRawFd, FromRawFd},
            os::unix::ffi::OsStrExt,
            time::{Duration, Instant},
        };
        let fixture = tempfile::tempdir()?;
        let input = fixture.path().join("input");
        let output = fixture.path().join("output");
        std::fs::create_dir(&input)?;
        let source = input.join("large.dat");
        std::fs::File::create(&source)?.set_len(1024 * 1024 * 1024)?;
        let (size, hash) = crate::planning::hash_input(&source)?;
        let plan = Plan {
            plan_version: 5,
            input_root: crate::StoredPath(input.canonicalize()?),
            engine: EngineInfo {
                name: "test".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: crate::ResolvedSettings::default(),
            tools: vec![],
            entries: vec![crate::Entry {
                path: crate::StoredPath("large.dat".into()),
                member: None,
                size,
                hash,
                action: Action::Copy {
                    reason: "test".into(),
                },
            }],
        };
        let metadata = output.join(".compressor");
        std::fs::create_dir_all(metadata.join("tmp"))?;
        std::fs::write(metadata.join("plan.json"), serde_json::to_vec(&plan)?)?;
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let watch = unsafe { std::fs::File::from_raw_fd(fd) };
        let name =
            CString::new(metadata.join("tmp").as_os_str().as_bytes()).map_err(io::Error::other)?;
        if unsafe { libc::inotify_add_watch(watch.as_raw_fd(), name.as_ptr(), libc::IN_CREATE) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let token = CancellationToken::new();
        let signal = token.clone();
        let watcher = tokio::task::spawn_blocking(move || -> io::Result<Instant> {
            let mut poll = libc::pollfd {
                fd: watch.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, 15000) } != 1 {
                return Err(io::Error::other("copy stage was not created"));
            }
            let at = Instant::now();
            signal.cancel();
            Ok(at)
        });
        let target = output.clone();
        let job_plan = plan.clone();
        let job = tokio::spawn(async move {
            let (events, _) = tokio::sync::broadcast::channel(16);
            run_job_with_archives(
                &job_plan,
                &target,
                &TransformerRegistry::default(),
                &PackRegistry::default(),
                &EnvelopeRegistry::default(),
                &ArchiveRegistry::default(),
                1,
                token,
                CancellationToken::new(),
                events,
            )
            .await
        });
        let cancelled_at = watcher.await.map_err(io::Error::other)??;
        let result = tokio::time::timeout(Duration::from_secs(2), job)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        let manifest = crate::planning::read_manifest(&metadata.join("manifest.json"))?;
        assert!(!manifest.complete);
        assert_eq!(
            crate::journal::fingerprint(&manifest.plan),
            crate::journal::fingerprint(&plan)
        );
        assert!(!output.join("large.dat").exists());
        assert!(!metadata.join("tmp/0.stage").exists());
        Ok(())
    }

    struct FakeArchive(&'static str, Arc<AtomicUsize>);
    impl crate::registry::ArchiveFormat for FakeArchive {
        fn write(
            &self,
            members: &[ArchiveInput],
            output: &Path,
            _: &serde_json::Value,
            cancel: &CancellationToken,
        ) -> io::Result<()> {
            self.1.fetch_add(1, Ordering::SeqCst);
            let mut result = Vec::new();
            for member in members {
                let bytes = std::fs::read(&member.path)?;
                assert!(bytes.iter().all(|b| *b == bytes[0]));
                result.push(member.name.len() as u8);
                result.extend(member.name.as_bytes());
                result.extend((bytes.len() as u64).to_le_bytes());
                result.push(bytes[0]);
            }
            if self.0 == "inflate" {
                result.resize(150, 0);
            }
            std::fs::write(output, result)?;
            if self.0 == "cancel" {
                cancel.cancel();
            }
            Ok(())
        }
        fn read_directory(&self, archive: &Path) -> io::Result<Vec<crate::registry::PackMember>> {
            let bytes = std::fs::read(archive)?;
            let mut cursor = 0;
            let mut result = Vec::new();
            while cursor < bytes.len() {
                if bytes[cursor] == 0 {
                    break;
                }
                let len = bytes[cursor] as usize;
                cursor += 1;
                let name = String::from_utf8(bytes[cursor..cursor + len].to_vec()).unwrap();
                cursor += len;
                let size = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
                cursor += 8;
                result.push(crate::registry::PackMember {
                    name,
                    offset: cursor as u64,
                    size,
                });
                cursor += 1;
            }
            Ok(result)
        }
        fn hash_member(
            &self,
            archive: &Path,
            member: &crate::registry::PackMember,
        ) -> io::Result<(u64, String)> {
            let data = std::fs::read(archive)?;
            let bytes = vec![
                data[member.offset as usize] ^ u8::from(self.0 == "corrupt");
                member.size as usize
            ];
            Ok((member.size, blake3::hash(&bytes).to_hex().to_string()))
        }
    }
    #[tokio::test]
    async fn assembles_archive_and_records_members() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        std::fs::create_dir(&input)?;
        std::fs::write(input.join("level0"), vec![42; 100])?;
        let (size, hash) = hash_input(&input.join("level0"))?;
        let plan = Plan {
            plan_version: 5,
            input_root: StoredPath(input),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: Default::default(),
            tools: vec![],
            entries: vec![
                crate::Entry {
                    path: StoredPath("level0".into()),
                    member: None,
                    size,
                    hash: hash.clone(),
                    action: Action::Archive {
                        archive: StoredPath("data.arc".into()),
                    },
                },
                crate::Entry {
                    path: StoredPath("data.arc".into()),
                    member: None,
                    size,
                    hash: "synthetic".into(),
                    action: Action::Assemble {
                        format: "fake".into(),
                        params: serde_json::json!({}),
                        members: vec!["level0".into()],
                    },
                },
            ],
        };
        let mut archives = ArchiveRegistry::default();
        let writes = Arc::new(AtomicUsize::new(0));
        archives.register("fake", Arc::new(FakeArchive("success", writes.clone())));
        let (events, mut received) = broadcast::channel(16);
        let report = run_job_with_archives(
            &plan,
            &output,
            &TransformerRegistry::default(),
            &PackRegistry::default(),
            &EnvelopeRegistry::default(),
            &archives,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            events,
        )
        .await?;
        assert_eq!(
            report.output_bytes,
            std::fs::metadata(output.join("data.arc"))?.len()
        );
        assert_eq!(report.archives.len(), 1);
        assert_eq!(report.archives[0].member_count, 1);
        assert_eq!(
            report.savings_by_media["data"],
            i128::from(size) - i128::from(report.output_bytes)
        );
        let mut results = 0;
        while let Ok(event) = received.try_recv() {
            if matches!(event, Event::FileResult { .. }) {
                results += 1;
            }
        }
        assert_eq!(results, plan.entries.len());
        assert!(output.join("data.arc").exists());
        assert!(!output.join("level0").exists());
        let manifest = crate::planning::read_manifest(&output.join(".compressor/manifest.json"))?;
        assert_eq!(manifest.entries.len(), 2);
        assert!(manifest
            .entries
            .iter()
            .all(|entry| entry.outcome == "archived"));
        assert_eq!(
            manifest
                .entries
                .iter()
                .find(|e| e.path.0 == Path::new("level0"))
                .unwrap()
                .output_size,
            0
        );
        let (events, _) = broadcast::channel(16);
        run_job_with_archives(
            &plan,
            &output,
            &TransformerRegistry::default(),
            &PackRegistry::default(),
            &EnvelopeRegistry::default(),
            &archives,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            events,
        )
        .await?;
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        std::fs::write(output.join("data.arc"), b"tampered")?;
        std::fs::write(output.join("level0"), b"old fallback")?;
        let (events, _) = broadcast::channel(16);
        run_job_with_archives(
            &plan,
            &output,
            &TransformerRegistry::default(),
            &PackRegistry::default(),
            &EnvelopeRegistry::default(),
            &archives,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            events,
        )
        .await?;
        assert_eq!(writes.load(Ordering::SeqCst), 2);
        assert!(!output.join("level0").exists());
        let cancel_output = temp.path().join("cancel");
        let mut cancelling = ArchiveRegistry::default();
        cancelling.register("fake", Arc::new(FakeArchive("cancel", writes.clone())));
        let (events, _) = broadcast::channel(16);
        assert_eq!(
            run_job_with_archives(
                &plan,
                &cancel_output,
                &TransformerRegistry::default(),
                &PackRegistry::default(),
                &EnvelopeRegistry::default(),
                &cancelling,
                1,
                CancellationToken::new(),
                CancellationToken::new(),
                events
            )
            .await
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!cancel_output.join("data.arc").exists());
        let (events, _) = broadcast::channel(16);
        run_job_with_archives(
            &plan,
            &cancel_output,
            &TransformerRegistry::default(),
            &PackRegistry::default(),
            &EnvelopeRegistry::default(),
            &archives,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            events,
        )
        .await?;
        assert!(cancel_output.join("data.arc").exists());
        for mode in ["inflate", "corrupt"] {
            let output = temp.path().join(mode);
            let mut archives = ArchiveRegistry::default();
            archives.register("fake", Arc::new(FakeArchive(mode, writes.clone())));
            let (events, _) = broadcast::channel(16);
            let report = run_job_with_archives(
                &plan,
                &output,
                &TransformerRegistry::default(),
                &PackRegistry::default(),
                &EnvelopeRegistry::default(),
                &archives,
                1,
                CancellationToken::new(),
                CancellationToken::new(),
                events,
            )
            .await?;
            assert_eq!(std::fs::read(output.join("level0"))?, vec![42; 100]);
            assert!(!output.join("data.arc").exists());
            let manifest =
                crate::planning::read_manifest(&output.join(".compressor/manifest.json"))?;
            let record = manifest
                .entries
                .iter()
                .find(|e| e.path.0 == Path::new("data.arc"))
                .unwrap();
            if mode == "inflate" {
                assert_eq!(record.outcome, "kept original: not smaller");
            } else {
                assert_eq!(report.failures.len(), 1);
            }
        }
        std::fs::write(plan.input_root.0.join("level0"), vec![43; 100])?;
        let output = temp.path().join("stale");
        let (events, _) = broadcast::channel(16);
        assert!(run_job_with_archives(
            &plan,
            &output,
            &TransformerRegistry::default(),
            &PackRegistry::default(),
            &EnvelopeRegistry::default(),
            &archives,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            events
        )
        .await
        .is_err());
        assert!(!output.join("data.arc").exists());
        Ok(())
    }
    struct Adapter;
    impl EngineAdapter for Adapter {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            unreachable!()
        }
        fn classify(
            &self,
            _: &Path,
            path: &Path,
            _: &EngineInfo,
            profile: &crate::ResolvedSettings,
        ) -> io::Result<Action> {
            if path == Path::new("game/images/opaque.png") {
                Ok(Action::Convert {
                    kind: MediaKind::Image,
                    target: "webp".into(),
                    encoder: "fake".into(),
                    envelope: None,
                    settings: crate::ConvertSettings::Image(profile.image()),
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
            _: &'a crate::ConvertSettings,
            cancel: &'a CancellationToken,
        ) -> Operation<'a, Option<String>> {
            Box::pin(async move {
                if self.wait {
                    tokio::select! { _=cancel.cancelled()=>return Err(io::Error::new(io::ErrorKind::Interrupted,"cancelled")), _=tokio::time::sleep(std::time::Duration::from_secs(2))=>{} }
                }
                if self.fail {
                    return Err(io::Error::other("fake failure"));
                }
                tokio::fs::write(dest, b"tiny").await?;
                Ok(Some("fake note".into()))
            })
        }
        fn validate<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a crate::ConvertSettings,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    struct XorEnvelope;
    impl crate::registry::EnvelopeFormat for XorEnvelope {
        fn unwrap(&self, data: &[u8], _: &serde_json::Value) -> io::Result<Vec<u8>> {
            if !data.starts_with(b"X") {
                return Err(io::Error::other("bad header"));
            }
            Ok(data[1..].iter().map(|b| b ^ 42).collect())
        }
        fn wrap(&self, payload: &[u8], _: &serde_json::Value) -> io::Result<Vec<u8>> {
            Ok(std::iter::once(b'X')
                .chain(payload.iter().map(|b| b ^ 42))
                .collect())
        }
    }
    #[tokio::test]
    async fn envelope_wraps_and_resumes_and_preserves_failures() -> io::Result<()> {
        use crate::registry::EnvelopeFormat;
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input");
        std::fs::create_dir(&input)?;
        let params = serde_json::json!({});
        for (name, bytes) in [
            ("large", XorEnvelope.wrap(b"a long payload", &params)?),
            ("small", XorEnvelope.wrap(b"one", &params)?),
            ("bad", b"invalid".to_vec()),
        ] {
            std::fs::write(input.join(name), bytes)?;
        }
        let settings = crate::ResolvedSettings::default();
        let entries = ["large", "small", "bad"]
            .into_iter()
            .map(|name| {
                let (size, hash) = hash_input(&input.join(name)).unwrap();
                crate::Entry {
                    path: StoredPath(name.into()),
                    member: None,
                    size,
                    hash,
                    action: Action::Convert {
                        kind: MediaKind::Image,
                        target: "fake".into(),
                        encoder: "fake".into(),
                        settings: crate::ConvertSettings::Image(settings.image()),
                        envelope: Some(crate::Envelope {
                            format: "xor".into(),
                            params: params.clone(),
                        }),
                    },
                }
            })
            .collect();
        let plan = Plan {
            plan_version: 5,
            input_root: StoredPath(input.clone()),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings,
            tools: vec![],
            entries,
        };
        let output = dir.path().join("output");
        let mut transforms = TransformerRegistry::default();
        transforms.register(
            "fake",
            Arc::new(Fake {
                fail: false,
                wait: false,
            }),
        );
        let mut envelopes = EnvelopeRegistry::default();
        envelopes.register("xor", Arc::new(XorEnvelope));
        let (sender, _) = broadcast::channel(20);
        // Bind the registry so its lifetime covers the async job.
        let packs = PackRegistry::default();
        let report = run_job_with_envelopes(
            &plan,
            &output,
            &transforms,
            &packs,
            &envelopes,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            sender.clone(),
        )
        .await?;
        assert_eq!(
            XorEnvelope.unwrap(&std::fs::read(output.join("large"))?, &params)?,
            b"tiny"
        );
        assert_eq!(
            std::fs::read(output.join("small"))?,
            std::fs::read(input.join("small"))?
        );
        assert_eq!(std::fs::read(output.join("bad"))?, b"invalid");
        assert_eq!(report.failures.len(), 1);
        assert!(report
            .skips
            .iter()
            .any(|r| r.outcome == "kept original: not smaller"));
        let before = std::fs::read(output.join(".compressor/journal.jsonl"))?;
        run_job_with_envelopes(
            &plan,
            &output,
            &transforms,
            &packs,
            &envelopes,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            sender,
        )
        .await?;
        assert_eq!(
            before,
            std::fs::read(output.join(".compressor/journal.jsonl"))?
        );
        Ok(())
    }
    struct FakePack {
        fail: bool,
    }
    impl crate::registry::PackFormat for FakePack {
        fn read_directory(&self, _: &Path) -> io::Result<Vec<crate::registry::PackMember>> {
            Ok(vec![crate::registry::PackMember {
                name: "member".into(),
                offset: 0,
                size: 20,
            }])
        }
        fn read_member(&self, _: &Path, _: &crate::registry::PackMember) -> io::Result<Vec<u8>> {
            Ok(b"a long member source".to_vec())
        }
        fn rebuild(
            &self,
            _: &Path,
            output: &Path,
            replacements: &BTreeMap<String, std::path::PathBuf>,
        ) -> io::Result<()> {
            if self.fail {
                return Err(io::Error::other("fake rebuild failure"));
            }
            std::fs::copy(&replacements["member"], output)?;
            Ok(())
        }
    }
    #[test]
    fn stage_sources_and_reject_invalid_pack_parent() -> io::Result<()> {
        use crate::registry::EnvelopeFormat;
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        std::fs::write(root.join("loose"), b"loose bytes")?;
        std::fs::write(
            root.join("wrapped"),
            XorEnvelope.wrap(b"payload", &serde_json::json!({}))?,
        )?;
        std::fs::write(root.join("pack"), b"pack")?;
        let settings = crate::ResolvedSettings::default();
        let convert = |envelope| Action::Convert {
            kind: MediaKind::Image,
            target: "fake".into(),
            encoder: "fake".into(),
            settings: crate::ConvertSettings::Image(settings.image()),
            envelope,
        };
        let entry = |name: &str, member, action| crate::Entry {
            path: StoredPath(name.into()),
            member,
            size: 0,
            hash: String::new(),
            action,
        };
        let loose = entry("loose", None, convert(None));
        let wrapped = entry(
            "wrapped",
            None,
            convert(Some(crate::Envelope {
                format: "xor".into(),
                params: serde_json::json!({}),
            })),
        );
        let member = entry("pack", Some("member".into()), convert(None));
        let parent = entry(
            "pack",
            None,
            Action::Rebuild {
                format: "fake".into(),
            },
        );
        let mut plan = Plan {
            plan_version: 5,
            input_root: StoredPath(root.into()),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings,
            tools: vec![],
            entries: vec![parent, member.clone()],
        };
        let mut packs = PackRegistry::default();
        packs.register("fake", Arc::new(FakePack { fail: false }));
        let mut envelopes = EnvelopeRegistry::default();
        envelopes.register("xor", Arc::new(XorEnvelope));
        for (entry, expected) in [
            (&loose, &b"loose bytes"[..]),
            (&wrapped, &b"payload"[..]),
            (&member, &b"a long member source"[..]),
        ] {
            let staged =
                crate::planning::stage_entry_source(root, &plan, entry, &packs, &envelopes, root)?;
            assert_eq!(std::fs::read(&staged)?, expected);
            std::fs::remove_file(staged)?;
        }
        plan.entries.remove(0);
        assert_eq!(
            crate::planning::stage_entry_source(root, &plan, &member, &packs, &envelopes, root)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        plan.entries.push(entry(
            "pack",
            None,
            Action::Copy {
                reason: "test".into(),
            },
        ));
        assert_eq!(
            crate::planning::stage_entry_source(root, &plan, &member, &packs, &envelopes, root)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
    #[tokio::test]
    async fn packs_rebuild_or_preserve_original() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let input = tmp.path().join("input");
        std::fs::create_dir(&input)?;
        std::fs::write(input.join("game.pck"), b"original pack bytes")?;
        let (size, hash) = hash_input(&input.join("game.pck"))?;
        let member_bytes = b"a long member source";
        let settings = crate::ResolvedSettings::default();
        let path = StoredPath("game.pck".into());
        let plan = Plan {
            plan_version: 5,
            input_root: StoredPath(input.canonicalize()?),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: settings.clone(),
            tools: vec![],
            entries: vec![
                crate::Entry {
                    path: path.clone(),
                    member: None,
                    size,
                    hash,
                    action: Action::Rebuild {
                        format: "fake".into(),
                    },
                },
                crate::Entry {
                    path,
                    member: Some("member".into()),
                    size: member_bytes.len() as u64,
                    hash: blake3::hash(member_bytes).to_hex().to_string(),
                    action: Action::Convert {
                        kind: MediaKind::Image,
                        target: "fake".into(),
                        encoder: "fake".into(),
                        envelope: None,
                        settings: crate::ConvertSettings::Image(settings.image()),
                    },
                },
            ],
        };
        for (mode, encode_fail, rebuild_fail) in [
            ("ok", false, false),
            ("empty", true, false),
            ("fail", false, true),
        ] {
            let mut transforms = TransformerRegistry::default();
            transforms.register(
                "fake",
                Arc::new(Fake {
                    fail: encode_fail,
                    wait: false,
                }),
            );
            let mut packs = crate::registry::PackRegistry::default();
            packs.register("fake", Arc::new(FakePack { fail: rebuild_fail }));
            let (sender, mut receiver) = broadcast::channel(100);
            let out = tmp.path().join(mode);
            let report = run_job_with_packs(
                &plan,
                &out,
                &transforms,
                &packs,
                1,
                CancellationToken::new(),
                CancellationToken::new(),
                sender,
            )
            .await?;
            let result = std::fs::read(out.join("game.pck"))?;
            if mode == "ok" {
                assert_eq!(result, b"tiny");
                assert_eq!(report.input_bytes, b"original pack bytes".len() as u64);
                assert_eq!(report.output_bytes, 4);
                assert_eq!(
                    report.savings_by_media["image"],
                    member_bytes.len() as i128 - 4
                );
                let mut names = Vec::new();
                while let Ok(event) = receiver.try_recv() {
                    if let Event::FileResult { path, .. } = event {
                        names.push(path.0);
                    }
                }
                assert_eq!(names.len(), 2);
                assert!(names.contains(&std::path::PathBuf::from("game.pck::member")));
                assert!(names.contains(&std::path::PathBuf::from("game.pck")));
            } else {
                assert_eq!(result, b"original pack bytes");
            }
            assert_eq!(
                report.failures.len(),
                usize::from(encode_fail || rebuild_fail)
            );
        }
        Ok(())
    }
    struct CountingTransformer(Arc<std::sync::atomic::AtomicUsize>);
    impl Transformer for CountingTransformer {
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
            _: &'a crate::ConvertSettings,
            _: &'a CancellationToken,
        ) -> Operation<'a, Option<String>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                tokio::fs::write(dest, b"tiny").await?;
                Ok(None)
            })
        }
        fn validate<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a crate::ConvertSettings,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    #[tokio::test]
    async fn cancelled_before_rebuild_resumes_staged_member() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let input = tmp.path().join("input");
        std::fs::create_dir(&input)?;
        std::fs::write(input.join("game.pck"), b"original pack bytes")?;
        std::fs::write(input.join("slow"), b"slow file content")?;
        let (size, hash) = hash_input(&input.join("game.pck"))?;
        let (slow_size, slow_hash) = hash_input(&input.join("slow"))?;
        let member_bytes = b"a long member source";
        let settings = crate::ResolvedSettings::default();
        let pack = StoredPath("game.pck".into());
        let convert = |encoder: &str| Action::Convert {
            kind: MediaKind::Image,
            target: "fake".into(),
            encoder: encoder.into(),
            envelope: None,
            settings: crate::ConvertSettings::Image(settings.image()),
        };
        let plan = Plan {
            plan_version: 5,
            input_root: StoredPath(input.canonicalize()?),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: settings.clone(),
            tools: vec![],
            entries: vec![
                crate::Entry {
                    path: pack.clone(),
                    member: None,
                    size,
                    hash,
                    action: Action::Rebuild {
                        format: "fake".into(),
                    },
                },
                crate::Entry {
                    path: pack,
                    member: Some("member".into()),
                    size: member_bytes.len() as u64,
                    hash: blake3::hash(member_bytes).to_hex().to_string(),
                    action: convert("count"),
                },
                crate::Entry {
                    path: StoredPath("slow".into()),
                    member: None,
                    size: slow_size,
                    hash: slow_hash,
                    action: convert("slow"),
                },
            ],
        };
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut transforms = TransformerRegistry::default();
        transforms.register("count", Arc::new(CountingTransformer(count.clone())));
        transforms.register(
            "slow",
            Arc::new(Fake {
                fail: false,
                wait: true,
            }),
        );
        let mut packs = crate::registry::PackRegistry::default();
        packs.register("fake", Arc::new(FakePack { fail: false }));
        let token = CancellationToken::new();
        let (sender, mut receiver) = broadcast::channel(100);
        let trigger = token.clone();
        tokio::spawn(async move {
            while let Ok(event) = receiver.recv().await {
                if matches!(event, Event::FileResult { .. }) {
                    trigger.cancel();
                    break;
                }
            }
        });
        let out = tmp.path().join("out");
        assert_eq!(
            run_job_with_packs(
                &plan,
                &out,
                &transforms,
                &packs,
                1,
                token,
                CancellationToken::new(),
                sender
            )
            .await
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!out.join("game.pck").exists());
        let (sender, _) = broadcast::channel(100);
        run_job_with_packs(
            &plan,
            &out,
            &transforms,
            &packs,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            sender,
        )
        .await?;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(out.join("game.pck"))?, b"tiny");
        assert!(!out.join(".compressor/members").exists());
        // A finished job resumes from the rebuilt pack without staged members.
        let (sender, _) = broadcast::channel(100);
        run_job_with_packs(
            &plan,
            &out,
            &transforms,
            &packs,
            1,
            CancellationToken::new(),
            CancellationToken::new(),
            sender,
        )
        .await?;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(out.join("game.pck"))?, b"tiny");
        Ok(())
    }
    #[tokio::test]
    async fn removal_is_reported_and_repeated_on_resume() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        std::fs::create_dir(&input)?;
        std::fs::write(input.join("unused.m4a"), b"unused")?;
        let (size, hash) = hash_input(&input.join("unused.m4a"))?;
        let plan = Plan {
            plan_version: 5,
            input_root: StoredPath(input),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: crate::ResolvedSettings::default(),
            tools: vec![],
            entries: vec![crate::Entry {
                path: StoredPath("unused.m4a".into()),
                member: None,
                size,
                hash,
                action: Action::Remove {
                    reason: "unused on desktop".into(),
                },
            }],
        };
        let registry = TransformerRegistry::default();
        let (sender, _) = broadcast::channel(10);
        let report = run_job(
            &plan,
            &output,
            &registry,
            1,
            CancellationToken::new(),
            sender.clone(),
        )
        .await?;
        assert!(!output.join("unused.m4a").exists());
        assert_eq!((report.input_bytes, report.output_bytes), (6, 0));
        assert_eq!(report.removals[0].reason, "unused on desktop");
        let manifest = crate::planning::read_manifest(&output.join(".compressor/manifest.json"))?;
        assert_eq!(manifest.entries[0].outcome, "removed");
        assert!(manifest.entries[0].output_hash.is_none());
        std::fs::write(output.join("unused.m4a"), b"restored")?;
        run_job(
            &plan,
            &output,
            &registry,
            1,
            CancellationToken::new(),
            sender,
        )
        .await?;
        assert!(!output.join("unused.m4a").exists());
        Ok(())
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
            registry.register(
                "fake",
                Arc::new(Fake {
                    fail: mode == "failure",
                    wait: mode == "cancel",
                }),
            );
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
                crate::ResolvedSettings::default(),
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
                if mode == "success" {
                    let manifest =
                        crate::planning::read_manifest(&out.join(".compressor/manifest.json"))?;
                    let converted = manifest
                        .entries
                        .iter()
                        .find(|e| e.path.0 == Path::new("game/images/opaque.png"))
                        .unwrap();
                    assert_eq!(converted.outcome, "converted; fake note");
                    assert!(!report.skips.iter().any(|e| e.path == converted.path));
                }
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
