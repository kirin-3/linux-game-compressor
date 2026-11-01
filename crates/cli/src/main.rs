use clap::{Parser, Subcommand};
use compressor_core::{
    job::{run_job_with_force, Event},
    planning::{build_plan, hash_input, validate_plan},
    registry::{EngineAdapter, EngineRegistry, TransformerRegistry},
    Action, Plan,
};
use compressor_engine_renpy::RenpyAdapter;
use compressor_media::{audio::AudioTransformer, webp::WebpTransformer, MediaCount};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "compressor",
    version,
    about = "Safely compress loose Ren'Py images"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    Settings {
        #[arg(conflicts_with = "engine")]
        game: Option<PathBuf>,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        set: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Inspect {
        game: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Plan {
        game: PathBuf,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        set: Vec<String>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        save: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Compress {
        game: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, conflicts_with = "plan")]
        profile: Option<String>,
        #[arg(long, conflicts_with = "plan")]
        set: Vec<String>,
        #[arg(long)]
        plan: Option<PathBuf>,
        #[arg(long, default_value_t=std::thread::available_parallelism().map_or(1,usize::from))]
        jobs: usize,
        #[arg(long)]
        allow_unknown_version: bool,
    },
    Verify {
        output: PathBuf,
        #[arg(long)]
        json: bool,
    },
}
fn registry() -> TransformerRegistry {
    let mut r = TransformerRegistry::default();
    r.register(Arc::new(WebpTransformer));
    r.register(Arc::new(AudioTransformer));
    r
}
fn engine_registry(allow: bool) -> EngineRegistry {
    let mut engines = EngineRegistry::default();
    engines.register(Arc::new(RenpyAdapter {
        allow_unknown_version: allow,
    }));
    engines
}
fn engine_specs(engines: &EngineRegistry) -> BTreeMap<String, Vec<compressor_core::OptionSpec>> {
    engines
        .ids()
        .into_iter()
        .map(|id| {
            (
                id.to_owned(),
                engines.by_id(id).expect("registered engine").options(),
            )
        })
        .collect()
}
fn new_plan(game: &Path, profile: Option<&str>, set: &[String], allow: bool) -> io::Result<Plan> {
    let adapter = RenpyAdapter {
        allow_unknown_version: allow,
    };
    let engine = adapter.detect(game)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no supported engine detected")
    })?;
    let engines = engine_registry(allow);
    let specs = engine_specs(&engines);
    let settings = compressor_core::settings::resolve(profile, set, adapter.id(), &specs)?;
    build_plan(game, &adapter, engine, settings, &registry())
}
fn print_json<T: serde::Serialize>(value: &T) -> io::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn plan_counts(plan: &Plan) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for entry in &plan.entries {
        let key = match &entry.action {
            Action::Convert { .. } => "convert".to_owned(),
            Action::Copy { reason } => format!("copy: {reason}"),
            Action::Skip { reason } => format!("skip: {reason}"),
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}
#[derive(serde::Serialize)]
struct InspectResult {
    engine: compressor_core::EngineInfo,
    media: BTreeMap<String, MediaCount>,
    archives: Vec<(String, u64)>,
    tools: Vec<(String, bool, BTreeMap<String, bool>)>,
    blockers: Vec<String>,
}
fn inspect(game: &Path) -> io::Result<InspectResult> {
    let plan = new_plan(game, None, &[], false)?;
    let mut media = BTreeMap::<String, MediaCount>::new();
    let mut archives = Vec::new();
    for entry in &plan.entries {
        let path = plan.input_root.0.join(&entry.path.0);
        if !std::fs::symlink_metadata(&path)?.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ["rpa", "rpi"].contains(&ext.as_str()) {
            archives.push((
                format!("{} (contents not processed)", entry.path.0.display()),
                entry.size,
            ));
        }
        compressor_media::summarize_file(&mut media, &path, entry.size)?;
    }
    let tools = plan
        .tools
        .iter()
        .map(|tool| {
            let mut p = plan.clone();
            p.tools = vec![tool.clone()];
            let features = tool
                .features
                .iter()
                .map(|feature| {
                    let mut single = p.clone();
                    single.tools[0].features = vec![feature.clone()];
                    (
                        feature.clone(),
                        compressor_core::tools::check_tools(&single).is_ok(),
                    )
                })
                .collect();
            (
                tool.name.clone(),
                compressor_core::tools::check_tools(&p).is_ok(),
                features,
            )
        })
        .collect();
    let blockers = plan
        .entries
        .iter()
        .filter_map(|e| match &e.action {
            Action::Skip { reason } if reason.starts_with("engine version") => Some(reason.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(InspectResult {
        engine: plan.engine,
        media,
        archives,
        tools,
        blockers,
    })
}
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            if e.kind() == io::ErrorKind::Interrupted {
                130
            } else {
                2
            }
        }
    };
    std::process::exit(code);
}
async fn run(cli: Cli) -> io::Result<i32> {
    match cli.command {
        Commands::Settings {
            game,
            engine,
            profile,
            set,
            json,
        } => {
            let engines = engine_registry(false);
            let id = if let Some(game) = game {
                engines
                    .detect(&game)?
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "no supported engine detected")
                    })?
                    .0
                    .id()
                    .to_owned()
            } else {
                engine.unwrap_or_else(|| "renpy".into())
            };
            let adapter = engines.by_id(&id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unknown engine {id}; known engines: {}",
                        engines.ids().join(", ")
                    ),
                )
            })?;
            let settings = compressor_core::settings::resolve(
                profile.as_deref(),
                &set,
                &id,
                &engine_specs(&engines),
            )?;
            let options = adapter.options();
            if json {
                print_json(
                    &serde_json::json!({"engine": id, "settings": settings, "options": options}),
                )?;
            } else {
                for (key, value) in &settings.values {
                    println!("{key} = {} (source: {})", value.value, value.source);
                }
                for option in &options {
                    let value = &settings.engine_options[&option.name];
                    let allowed = match &option.kind {
                        compressor_core::OptionKind::Enum(v) => v.join(", "),
                        compressor_core::OptionKind::Bool => "true, false".into(),
                        compressor_core::OptionKind::Int { min, max } => format!("{min}–{max}"),
                    };
                    let display = value
                        .value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.value.to_string());
                    println!(
                        "engines.{id}.{} = {display} (source: {}; allowed: {allowed})",
                        option.name, value.source
                    );
                }
            }
            Ok(0)
        }
        Commands::Inspect { game, json } => {
            let result = inspect(&game)?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "{} {}",
                    result.engine.name,
                    result.engine.version.as_deref().unwrap_or("unknown")
                );
                for (name, count) in result.media {
                    println!("{name}: {} files, {} bytes", count.count, count.bytes);
                }
                for (name, size) in result.archives {
                    println!("archive: {name} ({size} bytes)");
                }
                for (name, available, features) in result.tools {
                    println!(
                        "{name}: {}",
                        if available { "available" } else { "missing" }
                    );
                    for (feature, available) in features {
                        println!(
                            "  {feature}: {}",
                            if available { "available" } else { "missing" }
                        );
                    }
                }
                for blocker in result.blockers {
                    println!("blocked: {blocker}");
                }
            }
            Ok(0)
        }
        Commands::Plan {
            game,
            profile,
            set,
            list,
            save,
            json,
        } => {
            let plan = new_plan(&game, profile.as_deref(), &set, false)?;
            if let Some(path) = save {
                std::fs::write(path, serde_json::to_vec_pretty(&plan)?)?;
            }
            if json {
                print_json(&plan)?;
            } else {
                for (kind, count) in plan_counts(&plan) {
                    println!("{kind}: {count}");
                }
                let mut by_media = BTreeMap::<String, usize>::new();
                for entry in &plan.entries {
                    let media = match &entry.action {
                        Action::Convert { kind, .. } => format!("{kind:?}").to_lowercase(),
                        _ => {
                            let path = plan.input_root.0.join(&entry.path.0);
                            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                                if compressor_media::detect(&path)
                                    .is_ok_and(|f| f != compressor_media::ImageFormat::Unsupported)
                                {
                                    "image".into()
                                } else if compressor_media::audio::sniff(&path).is_ok() {
                                    "audio".into()
                                } else {
                                    "other".into()
                                }
                            } else {
                                "other".into()
                            }
                        }
                    };
                    let action = match &entry.action {
                        Action::Convert { .. } => "convert".to_owned(),
                        Action::Copy { reason } => format!("copy: {reason}"),
                        Action::Skip { reason } => format!("skip: {reason}"),
                    };
                    *by_media.entry(format!("{media} {action}")).or_default() += 1;
                }
                for (kind, count) in by_media {
                    println!("{kind}: {count}");
                }
                if list {
                    for e in &plan.entries {
                        println!("{}: {:?}", e.path.0.display(), e.action);
                    }
                }
            }
            Ok(0)
        }
        Commands::Compress {
            game,
            output,
            profile,
            set,
            plan,
            jobs,
            allow_unknown_version,
        } => {
            let plan = if let Some(file) = plan {
                let plan = compressor_core::planning::read_plan(&file)?;
                validate_plan(&plan, &game)?;
                plan
            } else {
                new_plan(&game, profile.as_deref(), &set, allow_unknown_version)?
            };
            let (sender, mut receiver) = tokio::sync::broadcast::channel(1024);
            let is_tty = std::io::stderr().is_terminal();
            let progress = tokio::spawn(async move {
                let bar = is_tty.then(|| {
                    let bar = indicatif::ProgressBar::new(0);
                    if let Ok(style) =
                        indicatif::ProgressStyle::with_template("{wide_bar} {pos}/{len} {msg}")
                    {
                        bar.set_style(style);
                    }
                    bar
                });
                // Files currently being processed, shown after the bytes saved.
                let mut active = std::collections::BTreeSet::new();
                let show = |bar: &indicatif::ProgressBar,
                            active: &std::collections::BTreeSet<String>,
                            saved: i128| {
                    let current = active.iter().next().cloned().unwrap_or_default();
                    let more = active.len().saturating_sub(1);
                    let more = if more > 0 {
                        format!(" (+{more} more)")
                    } else {
                        String::new()
                    };
                    bar.set_message(format!("{saved} bytes saved | {current}{more}"));
                };
                loop {
                    let event = match receiver.recv().await {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    match event {
                        Event::JobStart { total } => {
                            if let Some(b) = &bar {
                                b.set_length(total as u64)
                            }
                        }
                        Event::FileStart {
                            path,
                            total,
                            bytes_saved,
                            ..
                        } => {
                            if let Some(b) = &bar {
                                active.insert(path.0.display().to_string());
                                b.set_length(total as u64);
                                show(b, &active, bytes_saved);
                            }
                        }
                        Event::FileResult {
                            path,
                            done,
                            total,
                            bytes_saved,
                            outcome,
                        } => {
                            if let Some(b) = &bar {
                                active.remove(&path.0.display().to_string());
                                b.set_length(total as u64);
                                b.set_position(done as u64);
                                show(b, &active, bytes_saved);
                            } else {
                                eprintln!("[{done}/{total}] {}: {outcome}", path.0.display());
                            }
                        }
                        Event::JobEnd { .. } => break,
                    }
                }
                if let Some(b) = bar {
                    b.finish_and_clear();
                }
            });
            let token = CancellationToken::new();
            let force = CancellationToken::new();
            let on_signal = token.clone();
            let force_signal = force.clone();
            let mut interrupts =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let signal = tokio::spawn(async move {
                if interrupts.recv().await.is_some() {
                    on_signal.cancel();
                }
                if interrupts.recv().await.is_some() {
                    force_signal.cancel();
                }
            });
            let result =
                run_job_with_force(&plan, &output, &registry(), jobs, token, force, sender).await;
            signal.abort();
            let _ = progress.await;
            let report = result?;
            let saved = i128::from(report.input_bytes) - i128::from(report.output_bytes);
            let percent = if report.input_bytes == 0 {
                0.0
            } else {
                saved as f64 * 100.0 / report.input_bytes as f64
            };
            println!("Input: {} bytes; output: {} bytes; saved: {saved} bytes ({percent:.2}%); skipped: {}; failures: {}",report.input_bytes,report.output_bytes,report.skips.len(),report.failures.len());
            Ok(if report.failures.is_empty() { 0 } else { 1 })
        }
        Commands::Verify { output, json } => {
            let result = verify(&output).await?;
            if json {
                print_json(&result)?;
            } else if result.errors.is_empty() {
                println!("All {} files verified", result.checked);
            } else {
                for error in &result.errors {
                    eprintln!("{error}");
                }
            }
            Ok(if result.errors.is_empty() { 0 } else { 1 })
        }
    }
}
#[derive(serde::Serialize)]
struct VerifyResult {
    checked: usize,
    errors: Vec<String>,
}
async fn verify(output: &Path) -> io::Result<VerifyResult> {
    let manifest =
        compressor_core::planning::read_manifest(&output.join(".compressor/manifest.json"))?;
    let mut result = VerifyResult {
        checked: 0,
        errors: vec![],
    };
    if !manifest.complete {
        result.errors.push("manifest marks job incomplete".into());
    }
    for entry in &manifest.plan.entries {
        if !manifest
            .entries
            .iter()
            .any(|record| record.path == entry.path && record.input_hash == entry.hash)
        {
            result.errors.push(format!(
                "{}: missing manifest record",
                entry.path.0.display()
            ));
        }
    }
    for entry in &manifest.entries {
        let Some(hash) = &entry.output_hash else {
            continue;
        };
        result.checked += 1;
        let path = output.join(&entry.path.0);
        match hash_input(&path) {
            Ok((_, actual)) if &actual == hash => {}
            _ => {
                result
                    .errors
                    .push(format!("{}: hash mismatch", entry.path.0.display()));
                continue;
            }
        }
        if entry.outcome.starts_with("converted") {
            let action = manifest
                .plan
                .entries
                .iter()
                .find(|planned| planned.path == entry.path)
                .map(|planned| &planned.action);
            let checked = match action {
                Some(Action::Convert {
                    settings: compressor_core::ConvertSettings::Image(_),
                    ..
                }) => WebpTransformer::validate_output(&path, &CancellationToken::new()).await,
                Some(Action::Convert {
                    settings: settings @ compressor_core::ConvertSettings::Audio(_),
                    ..
                }) => {
                    AudioTransformer::validate_output(
                        &path,
                        settings,
                        &CancellationToken::new(),
                        &CancellationToken::new(),
                    )
                    .await
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing conversion settings",
                )),
            };
            if let Err(e) = checked {
                result
                    .errors
                    .push(format!("{}: {e}", entry.path.0.display()));
            }
        }
    }
    Ok(result)
}
