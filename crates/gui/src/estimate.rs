use super::egui;
use compressor_app::{sample_limiter::SampleLimiter, Estimate, EstimateOptions};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{mpsc, Arc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(super) enum Update {
    Result(PathBuf, u64, String, Result<Estimate, String>),
    StartReady(u64),
}
pub(super) struct Worker {
    runtime: tokio::runtime::Handle,
    pub updates: mpsc::Receiver<Update>,
    sender: mpsc::Sender<Update>,
    ctx: egui::Context,
    limiter: Arc<SampleLimiter>,
    tokens: HashMap<PathBuf, CancellationToken>,
    options: HashMap<PathBuf, EstimateOptions>,
    pub start_generation: u64,
}

fn log_stuck_sample(path: &std::path::Path) {
    use std::io::Write;
    let message = format!(
        "BUG: estimate sample did not stop within 5 seconds: {}",
        path.display()
    );
    eprintln!("{message}");
    if let Some(project) = directories::ProjectDirs::from("", "", "compressor") {
        let dir = project
            .state_dir()
            .unwrap_or_else(|| project.data_local_dir());
        if std::fs::create_dir_all(dir).is_ok() {
            if let Ok(mut log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("gui.log"))
            {
                let _ = writeln!(log, "{message}");
            }
        }
    }
}
impl Worker {
    pub fn new(runtime: &tokio::runtime::Runtime, ctx: egui::Context) -> Self {
        let (sender, updates) = mpsc::channel();
        let permits = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .saturating_div(2)
            .max(1);
        Self {
            runtime: runtime.handle().clone(),
            updates,
            sender,
            ctx,
            limiter: SampleLimiter::new(permits),
            tokens: HashMap::new(),
            options: HashMap::new(),
            start_generation: 0,
        }
    }
    pub fn forget(&mut self, path: &PathBuf) {
        if let Some(token) = self.tokens.remove(path) {
            token.cancel();
        }
        self.options.remove(path);
    }
    pub fn start(
        &mut self,
        path: PathBuf,
        generation: u64,
        engine: String,
        profile: String,
        overrides: BTreeMap<String, serde_json::Value>,
    ) {
        if let Some(token) = self.tokens.remove(&path) {
            token.cancel();
        }
        let cancel = CancellationToken::new();
        self.tokens.insert(path.clone(), cancel.clone());
        let options = self
            .options
            .entry(path.clone())
            .or_insert_with(|| EstimateOptions {
                limiter: Some(self.limiter.clone()),
                ..Default::default()
            })
            .clone();
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let specs = compressor_app::engine_specs(&compressor_app::engine_registry(false));
            let mut profiles: Vec<(&str, &str)> = vec![
                ("balanced", "balanced"),
                ("quality", "quality"),
                ("small", "small"),
            ];
            if !overrides.is_empty() {
                profiles.push(("custom", profile.as_str()));
            }
            for (key, base) in profiles {
                if cancel.is_cancelled() {
                    break;
                }
                let empty = BTreeMap::new();
                let sets = if key == "custom" { &overrides } else { &empty };
                let result = match compressor_app::resolve_settings(base, sets, &engine, &specs) {
                    Ok((_, settings)) => {
                        compressor_app::estimate(&path, &settings, &options, &cancel, |_, _| {})
                            .await
                            .map_err(|e| e.to_string())
                    }
                    Err(e) => Err(e.to_string()),
                };
                if cancel.is_cancelled() {
                    break;
                }
                let _ = sender.send(Update::Result(path.clone(), generation, key.into(), result));
                ctx.request_repaint();
            }
        });
    }
    pub fn prepare_start(&mut self) {
        self.start_generation += 1;
        let generation = self.start_generation;
        let limiter = self.limiter.clone();
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let stuck = limiter.prepare_queue(Duration::from_secs(5)).await;
            for path in stuck {
                log_stuck_sample(&path);
            }
            let _ = sender.send(Update::StartReady(generation));
            ctx.request_repaint();
        });
    }
    pub fn restore(&mut self) {
        self.start_generation += 1;
        self.limiter.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_produces_profiles_and_discards_old_generation() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = compressor_test_fixtures::Fixture::new().unwrap();
        let mut worker = Worker::new(&runtime, egui::Context::default());
        let path = fixture.root().to_path_buf();
        worker.start(
            path.clone(),
            1,
            "renpy".into(),
            "balanced".into(),
            BTreeMap::new(),
        );
        worker.start(
            path.clone(),
            2,
            "renpy".into(),
            "balanced".into(),
            BTreeMap::new(),
        );
        let mut profiles = Vec::new();
        while profiles.len() < 3 {
            match worker
                .updates
                .recv_timeout(Duration::from_secs(30))
                .unwrap()
            {
                Update::Result(_, generation, key, result) => {
                    assert_eq!(generation, 2);
                    assert!(result.is_ok(), "{result:?}");
                    profiles.push(key);
                }
                Update::StartReady(_) => panic!("unexpected start"),
            }
        }
        assert_eq!(profiles, ["balanced", "quality", "small"]);
        worker.forget(&path);
    }
    #[test]
    fn start_handoff_limits_samples_and_finish_restores_permits() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut worker = Worker::new(&runtime, egui::Context::default());
        worker.limiter = SampleLimiter::new(3);
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("game");
        std::fs::create_dir_all(game.join("renpy")).unwrap();
        std::fs::create_dir_all(game.join("game")).unwrap();
        std::fs::write(
            game.join("renpy/vc_version.py"),
            "version = '8.5.0.25102101'\n",
        )
        .unwrap();
        std::fs::write(game.join("game/script.rpy"), "label start:\n    return\n").unwrap();
        let queue = super::super::queue::start(&runtime, egui::Context::default());
        let token = CancellationToken::new();
        let one = runtime
            .block_on(worker.limiter.acquire("first".into(), &token))
            .unwrap();
        let two = runtime
            .block_on(worker.limiter.acquire("second".into(), &token))
            .unwrap();
        worker.prepare_start();
        runtime.block_on(async {
            one.cancel.cancelled().await;
            two.cancel.cancelled().await;
        });
        drop((one, two));
        assert!(matches!(
            worker.updates.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::StartReady(_)
        ));
        queue
            .commands
            .send(super::super::queue::Command::Start(vec![
                super::super::queue::Item {
                    game,
                    output: Some(dir.path().join("out")),
                    profile: "balanced".into(),
                    engine: Some("renpy".into()),
                    overrides: BTreeMap::new(),
                    plan: None,
                    destination: Some(compressor_app::DestinationState::New),
                    missing: vec![],
                    finished: false,
                    verify: false,
                },
            ]))
            .unwrap();
        while !matches!(
            queue.updates.recv_timeout(Duration::from_secs(10)).unwrap(),
            super::super::queue::Update::Started(_)
        ) {}
        runtime.block_on(async {
            let active = worker
                .limiter
                .acquire("running".into(), &token)
                .await
                .unwrap();
            assert!(tokio::time::timeout(
                Duration::from_millis(50),
                worker.limiter.acquire("blocked".into(), &token)
            )
            .await
            .is_err());
            drop(active);
            while !matches!(
                queue.updates.recv_timeout(Duration::from_secs(10)).unwrap(),
                super::super::queue::Update::QueueFinished
            ) {}
            worker.restore();
            let a = worker.limiter.acquire("a".into(), &token).await.unwrap();
            let b = worker.limiter.acquire("b".into(), &token).await.unwrap();
            let c = worker.limiter.acquire("c".into(), &token).await.unwrap();
            drop((a, b, c));
        });
    }
}
