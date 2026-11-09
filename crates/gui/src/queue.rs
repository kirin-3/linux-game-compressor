use compressor_core::{journal::Report, Plan};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::PathBuf,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Item {
    pub game: PathBuf,
    pub output: Option<PathBuf>,
    pub profile: String,
    pub engine: Option<String>,
    pub overrides: BTreeMap<String, serde_json::Value>,
    pub plan: Option<Plan>,
    pub destination: Option<compressor_app::DestinationState>,
    pub missing: Vec<String>,
    pub finished: bool,
    pub verify: bool,
}

pub enum Command {
    Start(Vec<Item>),
    Sync(Vec<Item>),
    Cancel(PathBuf),
    Stop,
    RunnerStarted(PathBuf),
    Verify(PathBuf, PathBuf),
    Finished(PathBuf, Outcome),
}
pub enum Outcome {
    Complete(
        Box<Report>,
        Option<Result<compressor_app::VerifyResult, String>>,
    ),
    Skipped(String),
    Failed(String),
    Cancelled,
}
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub bytes_saved: i128,
    pub active: BTreeSet<String>,
}
pub enum Update {
    Started(PathBuf),
    Cancelling(PathBuf, bool),
    Progress(PathBuf, Progress),
    Verifying(PathBuf),
    VerifyFinished(PathBuf, Result<compressor_app::VerifyResult, String>),
    Finished(PathBuf, Outcome),
    QueueFinished,
}

pub struct Queue {
    pub commands: tokio::sync::mpsc::UnboundedSender<Command>,
    pub updates: mpsc::Receiver<Update>,
}

pub fn start(runtime: &tokio::runtime::Runtime, ctx: eframe::egui::Context) -> Queue {
    let (commands, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let tx = commands.clone();
    let (outgoing, updates) = mpsc::channel();
    runtime.spawn(async move {
        let mut items: Vec<Item> = Vec::new();
        let mut processed = HashSet::new();
        let mut running = false;
        let mut active: Option<(PathBuf, CancellationToken, CancellationToken, bool)> = None;
        let mut stop_after_current = false;
        while let Some(command) = incoming.recv().await {
            match command {
                Command::Start(snapshot) => {
                    items = snapshot;
                    processed.clear();
                    running = true;
                    stop_after_current = false;
                }
                Command::Sync(snapshot) => items = snapshot,
                Command::Cancel(path) => {
                    if let Some((current, cancel, force, planning)) = &active {
                        if *current == path {
                            if cancel.is_cancelled() {
                                force.cancel();
                            } else {
                                cancel.cancel();
                            }
                            let _ = outgoing.send(Update::Cancelling(path, *planning));
                            ctx.request_repaint();
                        }
                    }
                }
                Command::Verify(game, output) => {
                    let outgoing = outgoing.clone();
                    let ctx = ctx.clone();
                    let _ = outgoing.send(Update::Verifying(game.clone()));
                    ctx.request_repaint();
                    tokio::spawn(async move {
                        let result = compressor_app::verify(&output)
                            .await
                            .map_err(|e| e.to_string());
                        let _ = outgoing.send(Update::VerifyFinished(game, result));
                        ctx.request_repaint();
                    });
                }
                Command::RunnerStarted(path) => {
                    if let Some((current, _, _, planning)) = &mut active {
                        if *current == path {
                            *planning = false;
                        }
                    }
                }
                Command::Stop => {
                    stop_after_current = true;
                    if active.is_none() {
                        running = false;
                        let _ = outgoing.send(Update::QueueFinished);
                        ctx.request_repaint();
                    }
                }
                Command::Finished(path, result) => {
                    active = None;
                    let _ = outgoing.send(Update::Finished(path, result));
                    if stop_after_current {
                        running = false;
                        let _ = outgoing.send(Update::QueueFinished);
                    }
                    ctx.request_repaint();
                }
            }
            while running && active.is_none() {
                let Some(item) = items
                    .iter()
                    .find(|item| !item.finished && !processed.contains(&item.game))
                    .cloned()
                else {
                    running = false;
                    let _ = outgoing.send(Update::QueueFinished);
                    ctx.request_repaint();
                    break;
                };
                if item.engine.is_none() || item.destination.is_none() {
                    break;
                }
                processed.insert(item.game.clone());
                let reason = if !item.missing.is_empty() {
                    Some(format!("missing tools: {}", item.missing.join(", ")))
                } else {
                    match item.destination.as_ref().unwrap() {
                        compressor_app::DestinationState::Blocked(reason) => Some(reason.clone()),
                        compressor_app::DestinationState::AlreadyCompressed => {
                            Some("already compressed".into())
                        }
                        compressor_app::DestinationState::NeedsPlan => {
                            processed.remove(&item.game);
                            break;
                        }
                        _ => None,
                    }
                };
                if let Some(reason) = reason {
                    let _ = outgoing.send(Update::Finished(item.game, Outcome::Skipped(reason)));
                    ctx.request_repaint();
                    continue;
                }
                if item.output.is_none() {
                    break;
                }
                let cancel = CancellationToken::new();
                let force = CancellationToken::new();
                active = Some((item.game.clone(), cancel.clone(), force.clone(), true));
                let _ = outgoing.send(Update::Started(item.game.clone()));
                ctx.request_repaint();
                let tx = tx.clone();
                let updates = outgoing.clone();
                let context = ctx.clone();
                tokio::spawn(async move {
                    let path = item.game.clone();
                    let outcome = run(item, updates, context, cancel, force, tx.clone()).await;
                    let _ = tx.send(Command::Finished(path, outcome));
                });
            }
        }
    });
    Queue { commands, updates }
}

async fn run(
    item: Item,
    outgoing: mpsc::Sender<Update>,
    ctx: eframe::egui::Context,
    cancel: CancellationToken,
    force: CancellationToken,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
) -> Outcome {
    let game_path = item.game.clone();
    let verify = item.verify;
    let output = item.output.clone().expect("ready output");
    let ready = tokio::task::spawn_blocking(move || {
        let plan = if let Some(plan) = item.plan {
            plan
        } else {
            let specs = compressor_app::engine_specs(&compressor_app::engine_registry(false));
            let (sets, _) = compressor_app::resolve_settings(
                &item.profile,
                &item.overrides,
                item.engine.as_deref().unwrap_or(""),
                &specs,
            )?;
            compressor_app::new_plan(&item.game, Some(&item.profile), &sets, false)?
        };
        let state = compressor_app::destination_state(&output, Some(&plan));
        Ok::<_, std::io::Error>((plan, output, state))
    })
    .await;
    if cancel.is_cancelled() {
        return Outcome::Cancelled;
    }
    let (plan, output, state) = match ready {
        Ok(Ok(ready)) => ready,
        Ok(Err(err)) => return Outcome::Failed(err.to_string()),
        Err(err) => return Outcome::Failed(err.to_string()),
    };
    if cancel.is_cancelled() {
        return Outcome::Cancelled;
    }
    match state {
        compressor_app::DestinationState::New | compressor_app::DestinationState::Resumes => {}
        compressor_app::DestinationState::AlreadyCompressed => {
            return Outcome::Skipped("already compressed".into())
        }
        compressor_app::DestinationState::Blocked(reason) => return Outcome::Skipped(reason),
        compressor_app::DestinationState::NeedsPlan => {
            return Outcome::Failed("destination requires a plan".into())
        }
    }
    if let Err(err) = compressor_core::tools::check_tools(&plan) {
        return Outcome::Skipped(err.to_string());
    }
    if cancel.is_cancelled() {
        return Outcome::Cancelled;
    }
    let _ = commands.send(Command::RunnerStarted(game_path.clone()));
    let registry = compressor_app::registries(false);
    let (events, receiver) = tokio::sync::broadcast::channel(4096);
    let progress = tokio::spawn(fold_events(
        game_path.clone(),
        receiver,
        outgoing.clone(),
        ctx.clone(),
    ));
    let jobs = std::thread::available_parallelism().map_or(1, usize::from);
    let result = compressor_core::job::run_job_with_rewrites(
        &plan,
        &output,
        &registry.transformers,
        &registry.packs,
        &registry.envelopes,
        &registry.archives,
        &registry.rewrites,
        jobs,
        cancel.clone(),
        force,
        events,
    )
    .await;
    let _ = progress.await;
    match result {
        Ok(report) => {
            let verification = if verify && report.complete {
                let _ = outgoing.send(Update::Verifying(game_path));
                ctx.request_repaint();
                let mut task = tokio::spawn(async move {
                    compressor_app::verify(&output)
                        .await
                        .map_err(|e| e.to_string())
                });
                tokio::select! {
                    result = &mut task => Some(result.unwrap_or_else(|e| Err(e.to_string()))),
                    _ = cancel.cancelled() => { task.abort(); Some(Err("verification cancelled".into())) },
                }
            } else {
                None
            };
            Outcome::Complete(Box::new(report), verification)
        }
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Outcome::Cancelled,
        Err(err) => Outcome::Failed(err.to_string()),
    }
}

async fn fold_events(
    game: PathBuf,
    mut receiver: tokio::sync::broadcast::Receiver<compressor_core::job::Event>,
    outgoing: mpsc::Sender<Update>,
    ctx: eframe::egui::Context,
) {
    use compressor_core::job::Event;
    let mut state = Progress::default();
    let mut dirty = false;
    let mut closed = false;
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(50));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = receiver.recv(), if !closed => match event {
                Ok(Event::JobStart { total }) => { state.total = total; dirty = true; }
                Ok(Event::FileStart { path, done, total, bytes_saved }) => { state.active.insert(path.0.display().to_string()); state.done = done; state.total = total; state.bytes_saved = bytes_saved; dirty = true; }
                Ok(Event::FileResult { path, done, total, bytes_saved, .. }) => { state.active.remove(&path.0.display().to_string()); state.done = done; state.total = total; state.bytes_saved = bytes_saved; dirty = true; }
                Ok(Event::JobEnd { done, total, bytes_saved, .. }) => { state.active.clear(); state.done = done; state.total = total; state.bytes_saved = bytes_saved; dirty = true; }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                Err(tokio::sync::broadcast::error::RecvError::Closed) => { closed = true; if !dirty { break; } }
            },
            _ = interval.tick(), if dirty => {
                if outgoing.send(Update::Progress(game.clone(), state.clone())).is_err() { break; }
                ctx.request_repaint(); dirty = false; if closed { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn game(root: &std::path::Path, name: &str, large: bool) -> Item {
        let path = root.join(name);
        std::fs::create_dir_all(path.join("renpy")).unwrap();
        std::fs::create_dir_all(path.join("game")).unwrap();
        std::fs::write(
            path.join("renpy/vc_version.py"),
            "version = '8.5.0.25102101'\n",
        )
        .unwrap();
        std::fs::write(path.join("game/script.rpy"), "label start:\n    return\n").unwrap();
        if large {
            std::fs::File::create(path.join("game/big.dat"))
                .unwrap()
                .set_len(64 * 1024 * 1024)
                .unwrap();
        }
        Item {
            output: Some(root.join(format!("{name}-out"))),
            game: path,
            profile: "balanced".into(),
            engine: Some("renpy".into()),
            overrides: BTreeMap::new(),
            plan: None,
            destination: Some(compressor_app::DestinationState::New),
            missing: vec![],
            finished: false,
            verify: false,
        }
    }
    #[test]
    fn verifies_completed_game_and_reports_manual_corruption() {
        let root = tempfile::tempdir().unwrap();
        let mut item = game(root.path(), "verify", false);
        item.verify = true;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let queue = start(&runtime, eframe::egui::Context::default());
        queue
            .commands
            .send(Command::Start(vec![item.clone()]))
            .unwrap();
        let mut saw_verifying = false;
        loop {
            match queue
                .updates
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
            {
                Update::Verifying(path) => {
                    assert_eq!(path, item.game);
                    saw_verifying = true;
                }
                Update::Finished(path, Outcome::Complete(report, Some(Ok(result)))) => {
                    assert_eq!(path, item.game);
                    assert!(saw_verifying && report.complete && result.errors.is_empty());
                    break;
                }
                Update::Finished(_, _) | Update::QueueFinished => {
                    panic!("automatic verification failed")
                }
                _ => {}
            }
        }
        let output = item.output.unwrap();
        std::fs::write(output.join("game/script.rpy"), b"corrupted").unwrap();
        queue
            .commands
            .send(Command::Verify(item.game.clone(), output))
            .unwrap();
        loop {
            match queue
                .updates
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
            {
                Update::VerifyFinished(path, Ok(result)) => {
                    assert_eq!(path, item.game);
                    assert!(!result.errors.is_empty());
                    break;
                }
                Update::VerifyFinished(_, Err(error)) => {
                    panic!("verification failed to run: {error}")
                }
                _ => {}
            }
        }
    }
    #[test]
    fn cancelled_job_resumes_and_stop_queue_leaves_later_game_ready() {
        let root = tempfile::tempdir().unwrap();
        let mut first = game(root.path(), "cancel", false);
        for i in 0..5000 {
            std::fs::write(
                first.game.join(format!("game/{i:04}.rpy")),
                "label x:\n    return\n",
            )
            .unwrap();
        }
        let plan = compressor_app::new_plan(&first.game, Some("balanced"), &[], false).unwrap();
        first.plan = Some(plan.clone());
        let later = game(root.path(), "later", false);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let queue = start(&runtime, eframe::egui::Context::default());
        queue
            .commands
            .send(Command::Start(vec![first.clone()]))
            .unwrap();
        let mut cancelled = false;
        loop {
            match queue
                .updates
                .recv_timeout(std::time::Duration::from_secs(15))
                .unwrap()
            {
                Update::Progress(path, progress)
                    if path == first.game && progress.done > 0 && !cancelled =>
                {
                    queue.commands.send(Command::Cancel(path)).unwrap();
                    cancelled = true;
                }
                Update::Finished(path, Outcome::Cancelled) => {
                    assert_eq!(path, first.game);
                    break;
                }
                Update::Finished(_, _) | Update::QueueFinished => {
                    panic!("job completed before cancellation")
                }
                _ => {}
            }
        }
        assert_eq!(
            compressor_app::destination_state(first.output.as_ref().unwrap(), Some(&plan)),
            compressor_app::DestinationState::Resumes
        );
        first.destination = Some(compressor_app::DestinationState::Resumes);
        queue
            .commands
            .send(Command::Start(vec![first.clone(), later.clone()]))
            .unwrap();
        let mut stopped = false;
        let mut completed = false;
        loop {
            match queue
                .updates
                .recv_timeout(std::time::Duration::from_secs(15))
                .unwrap()
            {
                Update::Started(path) => {
                    assert_eq!(path, first.game);
                    queue.commands.send(Command::Stop).unwrap();
                    stopped = true;
                }
                Update::Finished(path, Outcome::Complete(report, _)) => {
                    assert_eq!(path, first.game);
                    assert!(report.complete);
                    completed = true;
                }
                Update::QueueFinished if stopped => break,
                Update::Finished(_, _) => panic!("resume failed"),
                _ => {}
            }
        }
        assert!(completed);
        assert!(!later.output.unwrap().exists());
    }
    #[test]
    fn planning_cancel_is_reported_before_the_worker_moves_to_the_next_game() {
        let root = tempfile::tempdir().unwrap();
        let first = game(root.path(), "planning", true);
        let second = game(root.path(), "next", false);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let queue = start(&runtime, eframe::egui::Context::default());
        queue
            .commands
            .send(Command::Start(vec![first.clone(), second.clone()]))
            .unwrap();
        assert!(
            matches!(queue.updates.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), Update::Started(p) if p == first.game)
        );
        queue
            .commands
            .send(Command::Cancel(first.game.clone()))
            .unwrap();
        let mut notified = false;
        let mut finished = false;
        let mut next = false;
        loop {
            match queue
                .updates
                .recv_timeout(std::time::Duration::from_secs(15))
                .unwrap()
            {
                Update::Cancelling(path, true) if path == first.game => notified = true,
                Update::Finished(path, Outcome::Cancelled) if path == first.game => {
                    assert!(notified);
                    finished = true;
                }
                Update::Started(path) if path == second.game => {
                    assert!(finished);
                    next = true;
                }
                Update::QueueFinished => break,
                _ => {}
            }
        }
        assert!(notified && finished && next);
        assert!(!first.output.unwrap().exists());
    }
    #[test]
    fn progress_coalesces_large_event_bursts() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (events, receiver) = tokio::sync::broadcast::channel(4096);
        let (send, updates) = mpsc::channel();
        let task = runtime.spawn(fold_events(
            "game".into(),
            receiver,
            send,
            eframe::egui::Context::default(),
        ));
        for done in 1..=1000 {
            let _ = events.send(compressor_core::job::Event::FileResult {
                path: compressor_core::StoredPath("file".into()),
                outcome: "copied".into(),
                done,
                total: 1000,
                bytes_saved: done as i128,
            });
        }
        drop(events);
        runtime.block_on(task).unwrap();
        let results: Vec<_> = updates.try_iter().collect();
        assert!(results.len() <= 3);
        assert!(
            matches!(results.last(), Some(Update::Progress(_, p)) if p.done == 1000 && p.bytes_saved == 1000)
        );
    }
    #[test]
    fn continues_after_failure_skips_blocked_and_runs_added_game() {
        let root = tempfile::tempdir().unwrap();
        let first = game(root.path(), "first", true);
        let mut fail = game(root.path(), "fail", false);
        fail.game = root.path().join("missing");
        let mut blocked = game(root.path(), "blocked", false);
        blocked.destination = Some(compressor_app::DestinationState::Blocked("occupied".into()));
        let added = game(root.path(), "added", false);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let queue = start(&runtime, eframe::egui::Context::default());
        queue
            .commands
            .send(Command::Start(vec![
                first.clone(),
                fail.clone(),
                blocked.clone(),
            ]))
            .unwrap();
        match queue
            .updates
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
        {
            Update::Started(path) => assert_eq!(path, first.game),
            _ => panic!("first game did not start"),
        }
        queue
            .commands
            .send(Command::Sync(vec![
                first.clone(),
                fail.clone(),
                blocked.clone(),
                added.clone(),
            ]))
            .unwrap();
        let mut outcomes = Vec::new();
        loop {
            match queue
                .updates
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
            {
                Update::Started(_)
                | Update::Progress(_, _)
                | Update::Cancelling(_, _)
                | Update::Verifying(_)
                | Update::VerifyFinished(_, _) => {}
                Update::Finished(path, outcome) => outcomes.push((path, outcome)),
                Update::QueueFinished => break,
            }
        }
        assert!(matches!(&outcomes[0], (path, Outcome::Complete(_, _)) if *path == first.game));
        assert!(matches!(&outcomes[1], (path, Outcome::Failed(_)) if *path == fail.game));
        assert!(
            matches!(&outcomes[2], (path, Outcome::Skipped(reason)) if *path == blocked.game && reason == "occupied")
        );
        assert!(matches!(&outcomes[3], (path, Outcome::Complete(_, _)) if *path == added.game));
    }
}
