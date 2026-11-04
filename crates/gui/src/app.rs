use crate::{queue, style};
use eframe::egui;
#[path = "details.rs"]
mod details;
#[path = "estimate.rs"]
mod estimate;
#[path = "labels.rs"]
mod labels;
#[path = "settings_view.rs"]
mod settings_view;
#[path = "table.rs"]
mod table;
#[path = "toasts.rs"]
mod toasts;
#[path = "toolbar.rs"]
mod toolbar;
#[path = "worker.rs"]
mod worker;
#[cfg(test)]
use worker::check_game;
use worker::{missing_tools, start_worker, Command, Inspection, Update, Worker};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Preferences {
    output_dir: Option<std::path::PathBuf>,
    default_profile: String,
    verify: bool,
    window_size: [f32; 2],
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            output_dir: None,
            default_profile: "balanced".into(),
            verify: true,
            window_size: [1080.0, 680.0],
        }
    }
}
impl Preferences {
    fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }
    fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)
    }
}
fn preferences_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "compressor").map(|p| p.config_dir().join("gui.toml"))
}

/// Byte counts in binary units, e.g. "1.29 GiB"; negative values keep their sign.
fn size(bytes: i128) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.unsigned_abs() as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let sign = if bytes < 0 { "-" } else { "" };
    if unit == 0 {
        format!("{sign}{value} B")
    } else {
        format!("{sign}{value:.2} {}", UNITS[unit])
    }
}

fn verification_label(result: &Result<compressor_app::VerifyResult, String>) -> String {
    match result {
        Ok(result) if result.errors.is_empty() => "Verified".into(),
        _ => "Verify failed".into(),
    }
}

struct Row {
    path: std::path::PathBuf,
    inspection: Option<Box<Inspection>>,
    output: Option<std::path::PathBuf>,
    custom_output: bool,
    profile: String,
    overrides: std::collections::BTreeMap<String, serde_json::Value>,
    verify: bool,
    destination: Option<compressor_app::DestinationState>,
    plan: Option<Box<compressor_core::Plan>>,
    generation: u64,
    status: String,
    running: bool,
    finished: bool,
    report: Option<Box<compressor_core::journal::Report>>,
    progress: queue::Progress,
    verifying: bool,
    verification: Option<Result<compressor_app::VerifyResult, String>>,
    estimates: std::collections::BTreeMap<String, compressor_app::Estimate>,
    estimate_generation: u64,
    estimating: bool,
}

struct App {
    wayland: bool,
    _runtime: tokio::runtime::Runtime,
    worker: Worker,
    queue_worker: queue::Queue,
    estimator: estimate::Worker,
    starting: bool,
    queue_running: bool,
    queue_completed: usize,
    prefs: Preferences,
    prefs_path: Option<std::path::PathBuf>,
    queue: Vec<Row>,
    toasts: toasts::Toasts,
    tool_warnings: Vec<String>,
    selected: Option<std::path::PathBuf>,
    output_generation: u64,
}

impl App {
    fn queue_items(&self) -> Vec<queue::Item> {
        self.queue
            .iter()
            .map(|row| queue::Item {
                game: row.path.clone(),
                output: row.output.clone(),
                profile: row.profile.clone(),
                engine: row.inspection.as_ref().map(|info| info.engine_id.clone()),
                overrides: row.overrides.clone(),
                plan: row.plan.as_deref().cloned(),
                destination: row.destination.clone(),
                missing: row
                    .inspection
                    .as_ref()
                    .map_or_else(Vec::new, |i| missing_tools(&i.result)),
                finished: row.finished,
                verify: self.prefs.verify && row.verify,
            })
            .collect()
    }
    fn start_estimate(&mut self, path: &std::path::Path) {
        if let Some(row) = self.queue.iter_mut().find(|row| row.path == path) {
            if let (Some(info), Some(_)) = (&row.inspection, &row.output) {
                row.estimate_generation += 1;
                row.estimates.clear();
                row.estimating = true;
                self.estimator.start(
                    row.path.clone(),
                    row.estimate_generation,
                    info.engine_id.clone(),
                    row.profile.clone(),
                    row.overrides.clone(),
                );
            }
        }
    }
    fn sync_queue(&self) {
        if self.queue_running {
            let _ = self
                .queue_worker
                .commands
                .send(queue::Command::Sync(self.queue_items()));
        }
    }
    fn check_destination(&mut self, path: &std::path::Path) {
        if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
            row.generation += 1;
            row.plan = None;
            row.destination = None;
            if let Some(inspection) = &row.inspection {
                if let Some(output) = &row.output {
                    let _ = self.worker.commands.send(Command::CheckDestination(
                        row.path.clone(),
                        output.clone(),
                        row.profile.clone(),
                        inspection.engine_id.clone(),
                        row.overrides.clone(),
                        row.generation,
                    ));
                }
            }
        }
    }
    fn assign_outputs(&mut self, reset_custom: bool) {
        if reset_custom {
            for row in &mut self.queue {
                row.custom_output = false;
                row.finished = false;
                row.report = None;
                row.verification = None;
                row.status = if row.inspection.is_some() {
                    "Ready"
                } else {
                    "Checking…"
                }
                .into();
            }
        }
        self.output_generation += 1;
        for row in &mut self.queue {
            if !row.running {
                row.generation += 1;
                row.destination = None;
                row.plan = None;
            }
        }
        let rows = self
            .queue
            .iter()
            .map(|row| {
                (
                    row.path.clone(),
                    if row.custom_output || row.running {
                        row.output.clone()
                    } else {
                        None
                    },
                )
            })
            .collect();
        let _ = self.worker.commands.send(Command::AssignOutputs(
            self.output_generation,
            rows,
            self.prefs.output_dir.clone(),
        ));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let before = self.prefs.clone();
        let mut queue_changed = false;
        while let Ok(update) = self.queue_worker.updates.try_recv() {
            match update {
                queue::Update::Started(path) => {
                    if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
                        row.running = true;
                        row.status = "Running".into();
                    }
                }
                queue::Update::Cancelling(path, planning) => {
                    if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
                        row.status = if planning {
                            "Cancelling… (after planning)".into()
                        } else {
                            "Cancelling…".into()
                        };
                    }
                }
                queue::Update::Verifying(path) => {
                    if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
                        row.verifying = true;
                        row.status = "Verifying".into();
                    }
                }
                queue::Update::VerifyFinished(path, result) => {
                    if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
                        row.verifying = false;
                        row.status = verification_label(&result);
                        row.verification = Some(result);
                    }
                }
                queue::Update::Progress(path, progress) => {
                    if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
                        row.progress = progress;
                    }
                }
                queue::Update::Finished(path, result) => {
                    self.queue_completed += 1;
                    if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
                        row.running = false;
                        match result {
                            queue::Outcome::Complete(report, verification) => {
                                row.finished = report.complete;
                                row.verifying = false;
                                row.status = if !report.failures.is_empty() {
                                    "Finished with warnings".into()
                                } else {
                                    verification
                                        .as_ref()
                                        .map_or_else(|| "Finished".into(), verification_label)
                                };
                                row.verification = verification;
                                row.report = Some(report);
                            }
                            queue::Outcome::Skipped(reason) => {
                                row.status = format!("Skipped: {reason}")
                            }
                            queue::Outcome::Failed(reason) => {
                                row.status = format!("Failed: {reason}")
                            }
                            queue::Outcome::Cancelled => {
                                row.finished = false;
                                row.status = "Cancelled".into();
                            }
                        }
                    }
                    if self
                        .queue
                        .iter()
                        .any(|row| row.path == path && row.output.is_some())
                    {
                        self.check_destination(&path);
                        queue_changed = true;
                    }
                }
                queue::Update::QueueFinished => {
                    self.estimator.restore();
                    self.starting = false;
                    self.queue_running = false;
                    self.toasts
                        .push(toasts::Kind::Info, "Queue finished".into());
                }
            }
        }
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            let size = rect.size();
            if size.x > 0.0 && size.y > 0.0 {
                self.prefs.window_size = [size.x, size.y];
            }
        }
        while let Ok(update) = self.worker.updates.try_recv() {
            queue_changed = true;
            match update {
                Update::Added(path) => {
                    if self.selected.is_none() {
                        self.selected = Some(path.clone());
                    }
                    self.queue.push(Row {
                        path,
                        inspection: None,
                        output: None,
                        custom_output: false,
                        profile: self.prefs.default_profile.clone(),
                        overrides: Default::default(),
                        verify: true,
                        destination: None,
                        plan: None,
                        generation: 0,
                        status: "Checking…".into(),
                        running: false,
                        finished: false,
                        report: None,
                        progress: queue::Progress::default(),
                        verifying: false,
                        verification: None,
                        estimates: Default::default(),
                        estimate_generation: 0,
                        estimating: false,
                    });
                    self.assign_outputs(false);
                }
                Update::Destination(game, generation, state, plan) => {
                    if let Some(row) = self
                        .queue
                        .iter_mut()
                        .find(|r| r.path == game && r.generation == generation)
                    {
                        row.destination = Some(state);
                        row.plan = plan;
                    }
                }
                Update::OutputBase(path) => {
                    self.prefs.output_dir = Some(path);
                    self.assign_outputs(true);
                }
                Update::RowOutput(game, result) => match result {
                    Ok(output) => {
                        if let Some(row) = self.queue.iter_mut().find(|r| r.path == game) {
                            row.output = Some(output);
                            row.custom_output = true;
                            row.finished = false;
                            row.report = None;
                            row.verification = None;
                            row.status = "Ready".into();
                            self.assign_outputs(false);
                        }
                    }
                    Err(reason) => self.toasts.push(toasts::Kind::Error, reason),
                },
                Update::Outputs(generation, outputs) => {
                    if generation == self.output_generation {
                        for (game, result) in outputs {
                            if let Some(row) = self.queue.iter_mut().find(|r| r.path == game) {
                                match result {
                                    Ok(path) => row.output = Some(path),
                                    Err(reason) => {
                                        row.output = None;
                                        row.destination =
                                            Some(compressor_app::DestinationState::Blocked(
                                                reason.clone(),
                                            ));
                                        self.toasts.push(
                                            toasts::Kind::Error,
                                            format!("{}: {reason}", game.display()),
                                        );
                                    }
                                }
                            }
                            if self
                                .queue
                                .iter()
                                .any(|row| row.path == game && !row.running && row.output.is_some())
                            {
                                self.check_destination(&game);
                            }
                        }
                    }
                }
                Update::Checked(path, Ok(inspection)) => {
                    if let Some(row) = self.queue.iter_mut().find(|r| r.path == path) {
                        row.inspection = Some(inspection);
                        row.status = "Ready".into();
                    }
                    if self
                        .queue
                        .iter()
                        .any(|row| row.path == path && row.output.is_some())
                    {
                        self.check_destination(&path);
                    }
                }
                Update::Checked(path, Err(reason)) => {
                    self.queue.retain(|r| r.path != path);
                    let _ = self.worker.commands.send(Command::Forget(path.clone()));
                    self.toasts
                        .push(toasts::Kind::Error, format!("{}: {reason}", path.display()));
                    self.assign_outputs(false);
                }
                Update::Warning(message) => self.toasts.push(toasts::Kind::Warning, message),
                Update::Error(message) => self.toasts.push(toasts::Kind::Error, message),
                Update::ToolWarnings(warnings) => self.tool_warnings = warnings,
                Update::Ready => {}
            }
        }
        let pending_estimates: Vec<_> = self
            .queue
            .iter()
            .filter(|row| {
                row.inspection.is_some() && row.output.is_some() && row.estimate_generation == 0
            })
            .map(|row| row.path.clone())
            .collect();
        for path in pending_estimates {
            self.start_estimate(&path);
        }
        while let Ok(update) = self.estimator.updates.try_recv() {
            match update {
                estimate::Update::Result(path, generation, key, result) => {
                    if let Some(row) = self
                        .queue
                        .iter_mut()
                        .find(|row| row.path == path && row.estimate_generation == generation)
                    {
                        if let Ok(value) = result {
                            row.estimates.insert(key.clone(), value);
                        }
                        let last = if row.overrides.is_empty() {
                            "small"
                        } else {
                            "custom"
                        };
                        if key == last {
                            row.estimating = false;
                        }
                    }
                }
                estimate::Update::StartReady(generation)
                    if self.starting && generation == self.estimator.start_generation =>
                {
                    self.starting = false;
                    self.queue_running = true;
                    self.queue_completed = 0;
                    let _ = self
                        .queue_worker
                        .commands
                        .send(queue::Command::Start(self.queue_items()));
                }
                estimate::Update::StartReady(_) => {}
            }
        }
        for path in ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect::<Vec<_>>()
        }) {
            let _ = self.worker.commands.send(Command::Add(path));
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            let rect = ctx.input(|i| i.screen_rect());
            painter.rect_filled(rect, 0, style::Palette::current(ctx).overlay);
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop game folders",
                egui::FontId::proportional(28.0),
                style::Palette::current(ctx).overlay_text,
            );
        }
        toolbar::show(self, ctx);
        self.toasts.show(ctx);
        let row_action = table::show(
            ctx,
            &self.queue,
            &mut self.selected,
            &self.worker.commands,
            &self.queue_worker.commands,
        );
        if let Some((index, action)) = row_action {
            if action == 0 {
                let row = self.queue.remove(index);
                self.estimator.forget(&row.path);
                let _ = self.worker.commands.send(Command::Forget(row.path.clone()));
                if self.selected.as_ref() == Some(&row.path) {
                    self.selected = None;
                }
                self.assign_outputs(false);
            } else {
                let other = (index as i32 + action) as usize;
                self.queue.swap(index, other);
            }
            queue_changed = true;
        }
        let settings_changed = details::show(self, ctx);
        if let Some(path) = settings_changed {
            if let Some(row) = self.queue.iter_mut().find(|row| row.path == path) {
                row.finished = false;
                row.report = None;
                row.verification = None;
                row.status = "Ready".into();
            }
            self.check_destination(&path);
            self.start_estimate(&path);
            queue_changed = true;
        }
        if queue_changed {
            self.sync_queue();
        }
        // The window size changes every frame while resizing; it is saved on exit only.
        let resized_only = Preferences {
            window_size: self.prefs.window_size,
            ..before.clone()
        } == self.prefs;
        if self.prefs != before && !resized_only {
            if let Some(path) = &self.prefs_path {
                let _ = self.worker.commands.send(Command::Save(
                    self.prefs.clone(),
                    path.clone(),
                    None,
                ));
            }
        }
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(path) = &self.prefs_path {
            let (done, wait) = std::sync::mpsc::channel();
            if self
                .worker
                .commands
                .send(Command::Save(self.prefs.clone(), path.clone(), Some(done)))
                .is_ok()
            {
                let _ = wait.recv_timeout(std::time::Duration::from_secs(5));
            }
        }
    }
}

pub(crate) fn run() -> eframe::Result {
    let wayland = std::env::var("COMPRESSOR_GUI_BACKEND").is_ok_and(|v| v == "wayland")
        || std::env::var_os("DISPLAY").is_none_or(|v| v.is_empty());
    let prefs_path = preferences_path();
    let prefs = prefs_path
        .as_deref()
        .map(Preferences::load)
        .unwrap_or_default();
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size(prefs.window_size),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    {
        use winit::platform::{wayland::EventLoopBuilderExtWayland, x11::EventLoopBuilderExtX11};
        options.event_loop_builder = Some(Box::new(move |builder| {
            if wayland {
                builder.with_wayland();
            } else {
                builder.with_x11();
            }
        }));
    }
    let runtime = tokio::runtime::Runtime::new().expect("create background runtime");
    eframe::run_native(
        "Compressor",
        options,
        Box::new(move |cc| {
            style::apply(&cc.egui_ctx);
            let worker = start_worker(&runtime, cc.egui_ctx.clone());
            let queue_worker = queue::start(&runtime, cc.egui_ctx.clone());
            let estimator = estimate::Worker::new(&runtime, cc.egui_ctx.clone());
            let _ = worker.commands.send(Command::Ping);
            let _ = worker.commands.send(Command::ProbeTools);
            Ok(Box::new(App {
                wayland,
                _runtime: runtime,
                worker,
                queue_worker,
                estimator,
                starting: false,
                queue_running: false,
                queue_completed: 0,
                prefs,
                prefs_path,
                queue: Vec::new(),
                toasts: toasts::Toasts::default(),
                tool_warnings: Vec::new(),
                selected: None,
                output_generation: 0,
            }))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_round_trip_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/gui.toml");
        let prefs = Preferences {
            output_dir: Some("/out".into()),
            default_profile: "small".into(),
            verify: false,
            window_size: [900.0, 700.0],
        };
        prefs.save(&path).unwrap();
        assert_eq!(Preferences::load(&path), prefs);
        std::fs::write(&path, "not [ toml").unwrap();
        assert_eq!(Preferences::load(&path), Preferences::default());
    }
    #[test]
    fn checker_rejects_unsupported_and_accepts_supported_game() {
        let photos = tempfile::tempdir().unwrap();
        assert!(check_game(photos.path())
            .err()
            .unwrap()
            .to_string()
            .contains("no supported engine"));
        let unity = tempfile::tempdir().unwrap();
        std::fs::write(unity.path().join("Game.exe"), b"exe").unwrap();
        std::fs::create_dir(unity.path().join("Game_Data")).unwrap();
        let mut header = vec![0; 8];
        header.extend_from_slice(&22u32.to_be_bytes());
        header.extend_from_slice(&[0; 36]);
        header.extend_from_slice(b"2019.4.40f1\0");
        header.extend_from_slice(&19i32.to_le_bytes());
        std::fs::write(unity.path().join("Game_Data/globalgamemanagers"), header).unwrap();
        assert!(check_game(unity.path())
            .err()
            .unwrap()
            .to_string()
            .contains("2019.4.40f1"));
        let renpy = compressor_test_fixtures::Fixture::new().unwrap();
        let checked = check_game(renpy.root()).unwrap();
        assert_eq!(checked.result.engine.name, "Ren'Py");
        assert!(checked.bytes > 0);
    }
    #[test]
    fn worker_assigns_unique_destinations() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let worker = start_worker(&runtime, egui::Context::default());
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a/Game");
        let b = dir.path().join("b/Game");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let out = dir.path().join("out");
        worker
            .commands
            .send(Command::AssignOutputs(
                7,
                vec![(a.clone(), None), (b.clone(), None)],
                Some(out.clone()),
            ))
            .unwrap();
        let update = worker
            .updates
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        match update {
            Update::Outputs(7, paths) => {
                assert_eq!(paths[0].1.as_ref().unwrap(), &out.join("Game"));
                assert_eq!(paths[1].1.as_ref().unwrap(), &out.join("Game (2)"));
            }
            _ => panic!("unexpected update"),
        }
        worker
            .commands
            .send(Command::AssignOutputs(8, vec![(a.clone(), None)], None))
            .unwrap();
        match worker
            .updates
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
        {
            Update::Outputs(8, paths) => assert_eq!(
                paths[0].1.as_ref().unwrap(),
                &dir.path().join("a/Game-compressed")
            ),
            _ => panic!("unexpected update"),
        }
    }
    #[test]
    fn destination_planning_detects_resume_and_profile_mismatch() {
        let fixture = compressor_test_fixtures::Fixture::new().unwrap();
        let out = tempfile::tempdir().unwrap();
        let metadata = out.path().join(".compressor");
        std::fs::create_dir(&metadata).unwrap();
        let plan = compressor_app::new_plan(fixture.root(), Some("balanced"), &[], false).unwrap();
        std::fs::write(
            metadata.join("plan.json"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let worker = start_worker(&runtime, egui::Context::default());
        worker
            .commands
            .send(Command::CheckDestination(
                fixture.root().into(),
                out.path().into(),
                "balanced".into(),
                "renpy".into(),
                Default::default(),
                1,
            ))
            .unwrap();
        match worker
            .updates
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
        {
            Update::Destination(_, 1, compressor_app::DestinationState::Resumes, Some(_)) => {}
            _ => panic!("expected resume"),
        }
        worker
            .commands
            .send(Command::CheckDestination(
                fixture.root().into(),
                out.path().into(),
                "small".into(),
                "renpy".into(),
                Default::default(),
                2,
            ))
            .unwrap();
        match worker
            .updates
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
        {
            Update::Destination(_, 2, compressor_app::DestinationState::Blocked(_), _) => {}
            _ => panic!("expected mismatch"),
        }
    }
    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.50 KiB");
        assert_eq!(size(1_383_096_945), "1.29 GiB");
        assert_eq!(size(-2048), "-2.00 KiB");
    }
    #[test]
    fn worker_exchanges_messages_without_window() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let worker = start_worker(&runtime, egui::Context::default());
        worker.commands.send(Command::Ping).unwrap();
        assert!(matches!(
            worker
                .updates
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            Update::Ready
        ));
    }
}
