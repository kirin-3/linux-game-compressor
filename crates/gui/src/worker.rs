use super::{egui, Preferences};

#[derive(Debug)]
pub(super) enum Command {
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
pub(super) enum Update {
    Ready,
    ToolWarnings(Vec<String>),
    Added(std::path::PathBuf),
    Warning(String),
    Error(String),
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

pub(super) struct Worker {
    pub(super) commands: tokio::sync::mpsc::UnboundedSender<Command>,
    pub(super) updates: std::sync::mpsc::Receiver<Update>,
}

pub(super) fn start_worker(runtime: &tokio::runtime::Runtime, ctx: egui::Context) -> Worker {
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
                            let _ = outgoing.send(Update::Error(error));
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
            let message = format!("{display}: {err}");
            let _ = outgoing.send(if err.kind() == std::io::ErrorKind::InvalidInput {
                Update::Warning(message)
            } else {
                Update::Error(message)
            });
            ctx.request_repaint();
        }
        Err(err) => {
            let _ = outgoing.send(Update::Error(format!("{display}: {err}")));
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
        plan_version: 6,
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
pub(super) fn missing_tools(result: &compressor_app::InspectResult) -> Vec<String> {
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

pub(super) struct Inspection {
    pub(super) result: compressor_app::InspectResult,
    pub(super) bytes: u64,
    pub(super) engine_id: String,
}
pub(super) fn check_game(path: &std::path::Path) -> std::io::Result<Inspection> {
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
