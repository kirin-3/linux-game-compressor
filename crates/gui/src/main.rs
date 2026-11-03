mod queue;

use eframe::egui;

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

#[derive(Debug)]
enum Command {
    Ping,
    ProbeTools,
    Add(std::path::PathBuf),
    Forget(std::path::PathBuf),
    CheckDestination(
        std::path::PathBuf,
        std::path::PathBuf,
        String,
        String,
        std::collections::BTreeMap<String, serde_json::Value>,
        u64,
    ),
    OpenOutput(std::path::PathBuf),
    PickOutputBase,
    PickRowOutput(std::path::PathBuf, Vec<std::path::PathBuf>),
    AssignOutputs(
        u64,
        Vec<(std::path::PathBuf, Option<std::path::PathBuf>)>,
        Option<std::path::PathBuf>,
    ),
    Pick,
    Save(
        Preferences,
        std::path::PathBuf,
        Option<std::sync::mpsc::Sender<()>>,
    ),
}
enum Update {
    Ready,
    ToolWarnings(Vec<String>),
    Added(std::path::PathBuf),
    Warning(String),
    Checked(std::path::PathBuf, Result<Box<Inspection>, String>),
    Destination(
        std::path::PathBuf,
        u64,
        compressor_app::DestinationState,
        Option<Box<compressor_core::Plan>>,
    ),
    OutputBase(std::path::PathBuf),
    RowOutput(std::path::PathBuf, Result<std::path::PathBuf, String>),
    Outputs(
        u64,
        Vec<(std::path::PathBuf, Result<std::path::PathBuf, String>)>,
    ),
}

struct Worker {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    updates: std::sync::mpsc::Receiver<Update>,
}

fn start_worker(runtime: &tokio::runtime::Runtime, ctx: egui::Context) -> Worker {
    let (commands, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let (outgoing, updates) = std::sync::mpsc::channel();
    // Pickers stay open for as long as the user likes; they run beside the loop and send
    // their result back, so drops and destination checks are handled meanwhile.
    let picked = commands.clone();
    runtime.spawn(async move {
        let mut queued = std::collections::HashSet::new();
        let mut planning =
            std::collections::HashMap::<std::path::PathBuf, tokio::task::JoinHandle<()>>::new();
        while let Some(command) = incoming.recv().await {
            match command {
                Command::OpenOutput(path) => {
                    let outgoing = outgoing.clone();
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            std::process::Command::new("xdg-open").arg(path).status()
                        })
                        .await;
                        let error = match result {
                            Ok(Ok(status)) if status.success() => None,
                            Ok(Ok(status)) => {
                                Some(format!("Could not open output folder: {status}"))
                            }
                            Ok(Err(error)) => {
                                Some(format!("Could not open output folder: {error}"))
                            }
                            Err(error) => Some(format!("Could not open output folder: {error}")),
                        };
                        if let Some(error) = error {
                            let _ = outgoing.send(Update::Warning(error));
                            ctx.request_repaint();
                        }
                    });
                }
                Command::ProbeTools => {
                    let outgoing = outgoing.clone();
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        if let Ok(warnings) = tokio::task::spawn_blocking(probe_tools).await {
                            let _ = outgoing.send(Update::ToolWarnings(warnings));
                            ctx.request_repaint();
                        }
                    });
                }
                Command::CheckDestination(
                    game,
                    output,
                    profile,
                    engine_id,
                    overrides,
                    generation,
                ) => {
                    if let Some(old) = planning.remove(&game) {
                        old.abort();
                    }
                    let outgoing = outgoing.clone();
                    let ctx = ctx.clone();
                    let key = game.clone();
                    let task = tokio::spawn(async move {
                        let output_check = output.clone();
                        let state = tokio::task::spawn_blocking(move || {
                            compressor_app::destination_state(&output_check, None)
                        })
                        .await;
                        let Ok(state) = state else {
                            return;
                        };
                        if state != compressor_app::DestinationState::NeedsPlan {
                            let _ =
                                outgoing.send(Update::Destination(game, generation, state, None));
                            ctx.request_repaint();
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let game_check = game.clone();
                        let planned = tokio::task::spawn_blocking(move || {
                            let specs = compressor_app::engine_specs(
                                &compressor_app::engine_registry(false),
                            );
                            let (sets, _) = compressor_app::resolve_settings(
                                &profile, &overrides, &engine_id, &specs,
                            )?;
                            let plan = compressor_app::new_plan(
                                &game_check,
                                Some(&profile),
                                &sets,
                                false,
                            )?;
                            let state = compressor_app::destination_state(&output, Some(&plan));
                            Ok::<_, std::io::Error>((state, plan))
                        })
                        .await;
                        let (state, plan) = match planned {
                            Ok(Ok((state, plan))) => (state, Some(Box::new(plan))),
                            Ok(Err(e)) => (
                                compressor_app::DestinationState::Blocked(e.to_string()),
                                None,
                            ),
                            Err(e) => (
                                compressor_app::DestinationState::Blocked(e.to_string()),
                                None,
                            ),
                        };
                        let _ = outgoing.send(Update::Destination(game, generation, state, plan));
                        ctx.request_repaint();
                    });
                    planning.insert(key, task);
                }
                Command::PickOutputBase => {
                    let outgoing = outgoing.clone();
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        if let Ok(Some(path)) =
                            tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder())
                                .await
                        {
                            let _ = outgoing.send(Update::OutputBase(path));
                            ctx.request_repaint();
                        }
                    });
                }
                Command::PickRowOutput(game, taken) => {
                    let outgoing = outgoing.clone();
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            rfd::FileDialog::new()
                                .pick_folder()
                                .map(|path| {
                                    compressor_app::validate_output_path(&game, &path, &taken)
                                        .map(|()| path)
                                        .map_err(|e| e.to_string())
                                })
                                .map(|r| (game, r))
                        })
                        .await;
                        if let Ok(Some((game, result))) = result {
                            let _ = outgoing.send(Update::RowOutput(game, result));
                            ctx.request_repaint();
                        }
                    });
                }
                Command::AssignOutputs(generation, rows, base) => {
                    let result = tokio::task::spawn_blocking(move || {
                        let mut taken = Vec::new();
                        rows.into_iter()
                            .map(|(game, custom)| {
                                let output = match custom {
                                    Some(path) => {
                                        compressor_app::validate_output_path(&game, &path, &taken)
                                            .map(|()| path)
                                    }
                                    None => {
                                        compressor_app::output_path(&game, base.as_deref(), &taken)
                                    }
                                };
                                if let Ok(path) = &output {
                                    taken.push(path.clone());
                                }
                                (game, output.map_err(|e| e.to_string()))
                            })
                            .collect()
                    })
                    .await;
                    if let Ok(outputs) = result {
                        let _ = outgoing.send(Update::Outputs(generation, outputs));
                        ctx.request_repaint();
                    }
                }
                Command::Pick => {
                    let picked = picked.clone();
                    tokio::spawn(async move {
                        let paths =
                            tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folders())
                                .await
                                .ok()
                                .flatten()
                                .unwrap_or_default();
                        for path in paths {
                            let _ = picked.send(Command::Add(path));
                        }
                    });
                }
                Command::Add(path) => add_folder(path, &mut queued, &outgoing, &ctx).await,
                Command::Forget(path) => {
                    queued.remove(&path);
                    if let Some(task) = planning.remove(&path) {
                        task.abort();
                    }
                }
                Command::Save(prefs, path, done) => {
                    let _ = tokio::task::spawn_blocking(move || prefs.save(&path)).await;
                    if let Some(done) = done {
                        let _ = done.send(());
                    }
                    ctx.request_repaint();
                }
                Command::Ping => {
                    if outgoing.send(Update::Ready).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            }
        }
    });
    Worker { commands, updates }
}

async fn add_folder(
    path: std::path::PathBuf,
    queued: &mut std::collections::HashSet<std::path::PathBuf>,
    outgoing: &std::sync::mpsc::Sender<Update>,
    ctx: &egui::Context,
) {
    let display = path.display().to_string();
    let checked = tokio::task::spawn_blocking(move || {
        let path = path.canonicalize()?;
        if !path.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "only folders can be added",
            ));
        }
        Ok(path)
    })
    .await;
    match checked {
        Ok(Ok(path)) if queued.insert(path.clone()) => {
            let _ = outgoing.send(Update::Added(path.clone()));
            ctx.request_repaint();
            let outgoing = outgoing.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let job_path = path.clone();
                let result = tokio::task::spawn_blocking(move || check_game(&job_path))
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|r| r.map(Box::new).map_err(|e| e.to_string()));
                let _ = outgoing.send(Update::Checked(path, result));
                ctx.request_repaint();
            });
        }
        Ok(Ok(_)) => {
            let _ = outgoing.send(Update::Warning(format!("{display}: already queued")));
            ctx.request_repaint();
        }
        Ok(Err(err)) => {
            let _ = outgoing.send(Update::Warning(format!("{display}: {err}")));
            ctx.request_repaint();
        }
        Err(err) => {
            let _ = outgoing.send(Update::Warning(format!("{display}: {err}")));
            ctx.request_repaint();
        }
    }
}

fn probe_tools() -> Vec<String> {
    let registry = compressor_app::registry();
    let mut tools = std::collections::BTreeMap::<String, compressor_core::ToolSpec>::new();
    for id in [
        "cwebp",
        "ffmpeg",
        "ffmpeg-video",
        "godot-ctex",
        "godot-audio",
    ] {
        if let Some(transformer) = registry.get(id) {
            for spec in transformer.required_tools() {
                let entry =
                    tools
                        .entry(spec.name.clone())
                        .or_insert_with(|| compressor_core::ToolSpec {
                            name: spec.name.clone(),
                            package: spec.package.clone(),
                            features: Vec::new(),
                        });
                for feature in spec.features {
                    if !entry.features.contains(&feature) {
                        entry.features.push(feature);
                    }
                }
            }
        }
    }
    if let Some(ffmpeg) = tools.get_mut("ffmpeg") {
        for feature in [
            "libopus",
            "libvorbis",
            "libmp3lame",
            "libvpx-vp9",
            "decoder:libvpx",
            "decoder:libvpx-vp9",
        ] {
            if !ffmpeg.features.iter().any(|f| f == feature) {
                ffmpeg.features.push(feature.into());
            }
        }
    }
    let mut plan = compressor_core::Plan {
        plan_version: 5,
        input_root: compressor_core::StoredPath(std::path::PathBuf::new()),
        engine: compressor_core::EngineInfo {
            name: "probe".into(),
            version: None,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        },
        settings: compressor_core::ResolvedSettings::default(),
        tools: vec![],
        entries: vec![],
    };
    let mut warnings = Vec::new();
    for (_, mut tool) in tools {
        let features = std::mem::take(&mut tool.features);
        plan.tools = vec![tool.clone()];
        if let Err(e) = compressor_core::tools::check_tools(&plan) {
            warnings.push(e.to_string());
            continue;
        }
        for feature in features {
            plan.tools[0].features = vec![feature];
            if let Err(e) = compressor_core::tools::check_tools(&plan) {
                warnings.push(e.to_string());
            }
        }
    }
    warnings
}
fn missing_tools(result: &compressor_app::InspectResult) -> Vec<String> {
    let mut missing = Vec::new();
    for (name, available, features) in &result.tools {
        if !available {
            missing.push(name.clone());
        }
        for (feature, present) in features {
            if !present {
                missing.push(format!("{name}: {feature}"));
            }
        }
    }
    missing
}

struct Inspection {
    result: compressor_app::InspectResult,
    bytes: u64,
    engine_id: String,
}
fn check_game(path: &std::path::Path) -> std::io::Result<Inspection> {
    let registry = compressor_app::engine_registry(false);
    let (engine, _) = registry.detect(path)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no supported engine detected",
        )
    })?;
    let engine_id = engine.id().to_owned();
    let result = compressor_app::inspect(path)?;
    if let Some(reason) = compressor_app::unsupported_reason(&result) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            reason,
        ));
    }
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(std::io::Error::other)?;
        if entry.file_type().is_file() {
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(Inspection {
        result,
        bytes,
        engine_id,
    })
}
fn setting_widget(
    ui: &mut egui::Ui,
    key: &str,
    kind: &compressor_core::OptionKind,
    resolved: &compressor_core::Resolved,
    overrides: &mut std::collections::BTreeMap<String, serde_json::Value>,
) -> bool {
    let mut value = resolved.value.clone();
    let mut changed = false;
    ui.horizontal(|ui| {
        match kind {
            compressor_core::OptionKind::Bool => {
                let mut v = value.as_bool().unwrap_or_default();
                if ui.checkbox(&mut v, key).changed() {
                    value = v.into();
                    changed = true;
                }
            }
            compressor_core::OptionKind::Int { min, max } => {
                let mut v = value.as_i64().unwrap_or(*min);
                if ui
                    .add(egui::Slider::new(&mut v, *min..=*max).text(key))
                    .changed()
                {
                    value = v.into();
                    changed = true;
                }
            }
            compressor_core::OptionKind::Enum(options) => {
                let mut v = value.as_str().unwrap_or_default().to_owned();
                egui::ComboBox::from_id_salt(key)
                    .selected_text(&v)
                    .show_ui(ui, |ui| {
                        for option in options {
                            changed |= ui
                                .selectable_value(&mut v, option.clone(), option)
                                .changed();
                        }
                    });
                ui.label(key);
                value = v.into();
            }
        }
        if overrides.contains_key(key) {
            ui.label("override");
            if ui.small_button("Reset").clicked() {
                overrides.remove(key);
                changed = true;
                return;
            }
        }
        ui.weak(format!("source: {}", resolved.source));
        if changed {
            overrides.insert(key.to_owned(), value.clone());
        }
    });
    changed
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
}

struct App {
    wayland: bool,
    _runtime: tokio::runtime::Runtime,
    worker: Worker,
    queue_worker: queue::Queue,
    queue_running: bool,
    queue_completed: usize,
    prefs: Preferences,
    prefs_path: Option<std::path::PathBuf>,
    queue: Vec<Row>,
    toasts: Vec<String>,
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
                                row.status = verification
                                    .as_ref()
                                    .map_or_else(|| "Finished".into(), verification_label);
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
                queue::Update::QueueFinished => self.queue_running = false,
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
                        status: "Ready".into(),
                        running: false,
                        finished: false,
                        report: None,
                        progress: queue::Progress::default(),
                        verifying: false,
                        verification: None,
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
                            self.assign_outputs(false);
                        }
                    }
                    Err(reason) => self.toasts.push(reason),
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
                                        self.toasts.push(format!("{}: {reason}", game.display()));
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
                    self.toasts.push(format!("{}: {reason}", path.display()));
                    self.assign_outputs(false);
                }
                Update::Warning(message) => self.toasts.push(message),
                Update::ToolWarnings(warnings) => self.tool_warnings = warnings,
                Update::Ready => {}
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
            painter.rect_filled(rect, 0, egui::Color32::from_black_alpha(190));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop game folders",
                egui::FontId::proportional(28.0),
                egui::Color32::WHITE,
            );
        }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("Compressor");
                ui.separator();
                ui.label(self.prefs.output_dir.as_ref().map_or_else(
                    || "Output: beside each game".into(),
                    |p| format!("Output: {}", p.display()),
                ));
                if ui
                    .add_enabled(!self.queue_running, egui::Button::new("Choose folder"))
                    .clicked()
                {
                    let _ = self.worker.commands.send(Command::PickOutputBase);
                }
                if self.prefs.output_dir.is_some()
                    && ui
                        .add_enabled(!self.queue_running, egui::Button::new("Clear"))
                        .clicked()
                {
                    self.prefs.output_dir = None;
                    self.assign_outputs(true);
                }
                egui::ComboBox::from_id_salt("default_profile")
                    .selected_text(&self.prefs.default_profile)
                    .show_ui(ui, |ui| {
                        for profile in ["balanced", "quality", "small"] {
                            ui.selectable_value(
                                &mut self.prefs.default_profile,
                                profile.into(),
                                profile,
                            );
                        }
                    });
                ui.checkbox(&mut self.prefs.verify, "Verify");
                if ui
                    .add_enabled(
                        !self.queue_running && !self.queue.is_empty(),
                        egui::Button::new("Start"),
                    )
                    .clicked()
                {
                    self.queue_running = true;
                    self.queue_completed = 0;
                    let _ = self
                        .queue_worker
                        .commands
                        .send(queue::Command::Start(self.queue_items()));
                }
                if ui
                    .add_enabled(self.queue_running, egui::Button::new("Stop queue"))
                    .clicked()
                {
                    let _ = self.queue_worker.commands.send(queue::Command::Stop);
                }
            });
            if !self.tool_warnings.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 145, 55),
                    format!("Missing tools: {}", self.tool_warnings.join("; ")),
                );
            }
            if self.wayland {
                ui.label("Wayland: drag and drop is unavailable; use Add folder instead.");
            }
        });
        egui::TopBottomPanel::bottom("toasts").show(ctx, |ui| {
            // Keep the panel short: older messages are dropped once there are more than five.
            let excess = self.toasts.len().saturating_sub(5);
            self.toasts.drain(..excess);
            if self.toasts.is_empty() {
                ui.label("Ready");
            } else {
                ui.horizontal(|ui| {
                    if ui.small_button("Dismiss").clicked() {
                        self.toasts.clear();
                    }
                });
            }
            for toast in &self.toasts {
                ui.label(toast);
            }
        });
        let mut row_action: Option<(usize, i32)> = None;
        egui::SidePanel::left("queue")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Queue");
                    if self.queue_running {
                        ui.label(format!(
                            "{} / {} games",
                            self.queue_completed,
                            self.queue.len().max(self.queue_completed)
                        ));
                    }
                    if ui.button("Add folder").clicked() {
                        let _ = self.worker.commands.send(Command::Pick);
                    }
                });
                ui.separator();
                if self.queue.is_empty() {
                    ui.label("Drop game folders here to get started.");
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (index, row) in self.queue.iter().enumerate() {
                        ui.group(|ui| {
                            let label = row
                                .path
                                .file_name()
                                .unwrap_or(row.path.as_os_str())
                                .to_string_lossy();
                            if ui
                                .selectable_label(self.selected.as_ref() == Some(&row.path), label)
                                .clicked()
                            {
                                self.selected = Some(row.path.clone());
                            }
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(
                                        !row.running
                                            && index > 0
                                            && !self.queue[index.saturating_sub(1)].running,
                                        egui::Button::new("↑"),
                                    )
                                    .clicked()
                                {
                                    row_action = Some((index, -1));
                                }
                                if ui
                                    .add_enabled(
                                        !row.running
                                            && index + 1 < self.queue.len()
                                            && !self
                                                .queue
                                                .get(index + 1)
                                                .is_some_and(|r| r.running),
                                        egui::Button::new("↓"),
                                    )
                                    .clicked()
                                {
                                    row_action = Some((index, 1));
                                }
                                if ui
                                    .add_enabled(!row.running, egui::Button::new("Remove"))
                                    .clicked()
                                {
                                    row_action = Some((index, 0));
                                }
                                ui.label(&row.status);
                                if row.running && ui.button("Cancel").clicked() {
                                    let _ = self
                                        .queue_worker
                                        .commands
                                        .send(queue::Command::Cancel(row.path.clone()));
                                }
                            });
                            if row.running {
                                let progress = &row.progress;
                                let fraction = if progress.total == 0 {
                                    0.0
                                } else {
                                    progress.done as f32 / progress.total as f32
                                };
                                ui.add(egui::ProgressBar::new(fraction).text(format!(
                                    "{} / {} files · {} saved",
                                    progress.done,
                                    progress.total,
                                    size(progress.bytes_saved)
                                )));
                            }
                            if let Some(report) = &row.report {
                                let saved = i128::from(report.input_bytes)
                                    - i128::from(report.output_bytes);
                                let percent = if report.input_bytes == 0 {
                                    0.0
                                } else {
                                    saved as f64 * 100.0 / report.input_bytes as f64
                                };
                                let failures = report.failures.len();
                                ui.label(format!(
                                    "Saved {} ({percent:.2}%) · {failures} {}",
                                    size(saved),
                                    if failures == 1 { "failure" } else { "failures" }
                                ));
                            }
                            if let Some(output) = &row.output {
                                ui.label(format!("Output: {}", output.display()));
                            }
                            ui.label(format!(
                                "{} · {}",
                                row.profile,
                                match &row.destination {
                                    Some(compressor_app::DestinationState::New) => "New".into(),
                                    Some(compressor_app::DestinationState::Resumes) =>
                                        "Resumes".into(),
                                    Some(compressor_app::DestinationState::AlreadyCompressed) =>
                                        "Already compressed".into(),
                                    Some(compressor_app::DestinationState::Blocked(reason)) =>
                                        format!("Blocked: {reason}"),
                                    _ => "Checking…".into(),
                                }
                            ));
                            if let Some(info) = &row.inspection {
                                ui.label(format!(
                                    "{} {} · {}",
                                    info.result.engine.name,
                                    info.result.engine.version.as_deref().unwrap_or("unknown"),
                                    size(i128::from(info.bytes))
                                ));
                                let counts = info
                                    .result
                                    .media
                                    .iter()
                                    .map(|(kind, count)| format!("{} {kind}", count.count))
                                    .collect::<Vec<_>>()
                                    .join(" · ");
                                ui.label(counts);
                                let missing = missing_tools(&info.result);
                                if !missing.is_empty() {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(220, 145, 55),
                                        format!("Needs: {}", missing.join(", ")),
                                    );
                                }
                            } else {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Checking…");
                                });
                            }
                        });
                    }
                });
            });
        if let Some((index, action)) = row_action {
            if action == 0 {
                let row = self.queue.remove(index);
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
        let mut settings_changed = None;
        let other_outputs: Vec<_> = self
            .queue
            .iter()
            .filter(|r| self.selected.as_ref() != Some(&r.path))
            .filter_map(|r| r.output.clone())
            .collect();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Game details");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(row) = self
                    .queue
                    .iter_mut()
                    .find(|r| self.selected.as_ref() == Some(&r.path))
                {
                    ui.label(format!("Game: {}", row.path.display()));
                    if let Some(report) = &row.report {
                        let saved =
                            i128::from(report.input_bytes) - i128::from(report.output_bytes);
                        let percent = if report.input_bytes == 0 {
                            0.0
                        } else {
                            saved as f64 * 100.0 / report.input_bytes as f64
                        };
                        ui.label(format!(
                            "Input: {} · Output: {}",
                            size(i128::from(report.input_bytes)),
                            size(i128::from(report.output_bytes))
                        ));
                        ui.label(format!(
                            "Saved: {} ({percent:.2}%) · {} skipped · {} failures",
                            size(saved),
                            report.skips.len(),
                            report.failures.len()
                        ));
                        for failure in &report.failures {
                            ui.colored_label(
                                egui::Color32::LIGHT_RED,
                                format!("{}: {}", failure.path.0.display(), failure.outcome),
                            );
                        }
                        if let Some(output) = &row.output {
                            if ui.button("Open output folder").clicked() {
                                let _ = self
                                    .worker
                                    .commands
                                    .send(Command::OpenOutput(output.clone()));
                            }
                        }
                    }
                    if (row.finished
                        || matches!(
                            row.destination,
                            Some(compressor_app::DestinationState::AlreadyCompressed)
                        ))
                        && !row.verifying
                        && ui.button("Verify output").clicked()
                    {
                        if let Some(output) = &row.output {
                            row.verifying = true;
                            row.status = "Verifying".into();
                            let _ = self
                                .queue_worker
                                .commands
                                .send(queue::Command::Verify(row.path.clone(), output.clone()));
                        }
                    }
                    if let Some(result) = &row.verification {
                        match result {
                            Ok(result) => {
                                if result.errors.is_empty() {
                                    ui.label(format!("Verified {} files", result.checked));
                                } else {
                                    for error in &result.errors {
                                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                                    }
                                }
                            }
                            Err(error) => {
                                ui.colored_label(egui::Color32::LIGHT_RED, error);
                            }
                        }
                    }
                    if row.running {
                        ui.strong("Processing");
                        for path in &row.progress.active {
                            ui.label(path);
                        }
                    }
                    ui.add_enabled_ui(!row.running, |ui| {
                        let previous_profile = row.profile.clone();
                        egui::ComboBox::from_id_salt("row_profile")
                            .selected_text(&row.profile)
                            .show_ui(ui, |ui| {
                                for name in ["balanced", "quality", "small"] {
                                    ui.selectable_value(&mut row.profile, name.into(), name);
                                }
                            });
                        if previous_profile != row.profile {
                            settings_changed = Some(row.path.clone());
                        }
                        ui.checkbox(&mut row.verify, "Verify this game");
                        if let Some(info) = &row.inspection {
                            let specs = compressor_app::engine_specs(
                                &compressor_app::engine_registry(false),
                            );
                            match compressor_app::resolve_settings(
                                &row.profile,
                                &row.overrides,
                                &info.engine_id,
                                &specs,
                            ) {
                                Ok((_, resolved)) => {
                                    ui.separator();
                                    ui.strong("Image and audio");
                                    let common = [
                                        (
                                            "image.quality",
                                            compressor_core::OptionKind::Int { min: 0, max: 100 },
                                        ),
                                        (
                                            "image.effort",
                                            compressor_core::OptionKind::Int { min: 0, max: 6 },
                                        ),
                                        ("image.sharp_yuv", compressor_core::OptionKind::Bool),
                                        ("image.reencode_webp", compressor_core::OptionKind::Bool),
                                        (
                                            "audio.vorbis_quality",
                                            compressor_core::OptionKind::Int { min: 0, max: 10 },
                                        ),
                                        (
                                            "audio.opus_kbps_per_channel",
                                            compressor_core::OptionKind::Int { min: 6, max: 256 },
                                        ),
                                        (
                                            "audio.mp3_kbps_per_channel",
                                            compressor_core::OptionKind::Int { min: 8, max: 160 },
                                        ),
                                    ];
                                    for (key, kind) in common {
                                        if let Some(value) = resolved.values.get(key) {
                                            if setting_widget(
                                                ui,
                                                key,
                                                &kind,
                                                value,
                                                &mut row.overrides,
                                            ) {
                                                settings_changed = Some(row.path.clone());
                                            }
                                        }
                                    }
                                    ui.separator();
                                    ui.strong(format!("{} options", info.result.engine.name));
                                    if let Some(options) = specs.get(&info.engine_id) {
                                        for option in options {
                                            if let Some(value) =
                                                resolved.engine_options.get(&option.name)
                                            {
                                                let key = format!(
                                                    "engines.{}.{}",
                                                    info.engine_id, option.name
                                                );
                                                if setting_widget(
                                                    ui,
                                                    &key,
                                                    &option.kind,
                                                    value,
                                                    &mut row.overrides,
                                                ) {
                                                    settings_changed = Some(row.path.clone());
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    ui.label(format!("Settings error: {error}"));
                                }
                            }
                        }
                        ui.label(row.output.as_ref().map_or_else(
                            || "Output: checking".into(),
                            |p| format!("Output: {}", p.display()),
                        ));
                        if ui.button("Change output folder").clicked() {
                            let taken = other_outputs.clone();
                            let _ = self
                                .worker
                                .commands
                                .send(Command::PickRowOutput(row.path.clone(), taken));
                        }
                    });
                } else {
                    ui.label("Select a game to view its settings and progress.");
                }
            });
        });
        if let Some(path) = settings_changed {
            if let Some(row) = self.queue.iter_mut().find(|row| row.path == path) {
                row.finished = false;
            }
            self.check_destination(&path);
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

fn main() -> eframe::Result {
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
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            for theme in [egui::Theme::Dark, egui::Theme::Light] {
                cc.egui_ctx.style_mut_of(theme, |style| {
                    style.spacing.item_spacing = egui::vec2(12.0, 10.0);
                    style.spacing.button_padding = egui::vec2(12.0, 7.0);
                    style.visuals.selection.bg_fill = egui::Color32::from_rgb(72, 122, 190);
                    style.visuals.window_corner_radius = egui::CornerRadius::same(10);
                    for widget in [
                        &mut style.visuals.widgets.inactive,
                        &mut style.visuals.widgets.hovered,
                        &mut style.visuals.widgets.active,
                    ] {
                        widget.corner_radius = egui::CornerRadius::same(7);
                    }
                });
            }
            let worker = start_worker(&runtime, cc.egui_ctx.clone());
            let queue_worker = queue::start(&runtime, cc.egui_ctx.clone());
            let _ = worker.commands.send(Command::Ping);
            let _ = worker.commands.send(Command::ProbeTools);
            Ok(Box::new(App {
                wayland,
                _runtime: runtime,
                worker,
                queue_worker,
                queue_running: false,
                queue_completed: 0,
                prefs,
                prefs_path,
                queue: Vec::new(),
                toasts: Vec::new(),
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
