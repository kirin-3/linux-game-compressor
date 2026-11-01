use clap::{Parser, Subcommand};
use compressor_core::{
    job::{run_job_with_force, Event},
    planning::{build_plan, hash_input, validate_plan},
    profile::load_profile,
    registry::{EngineAdapter, TransformerRegistry},
    Action, Plan,
};
use compressor_engine_renpy::RenpyAdapter;
use compressor_media::{detect, webp::WebpTransformer};
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
    r
}
fn new_plan(game: &Path, profile: Option<&str>, allow: bool) -> io::Result<Plan> {
    let adapter = RenpyAdapter {
        allow_unknown_version: allow,
    };
    let engine = adapter.detect(game)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no supported engine detected")
    })?;
    build_plan(game, &adapter, engine, load_profile(profile)?, &registry())
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
struct MediaCount {
    count: u64,
    bytes: u64,
}
#[derive(serde::Serialize)]
struct InspectResult {
    engine: compressor_core::EngineInfo,
    media: BTreeMap<String, MediaCount>,
    archives: Vec<(String, u64)>,
    tools: Vec<(String, bool)>,
    blockers: Vec<String>,
}
fn inspect(game: &Path) -> io::Result<InspectResult> {
    let plan = new_plan(game, None, false)?;
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
        let kind = match detect(&path)? {
            compressor_media::ImageFormat::Png => "png",
            compressor_media::ImageFormat::AnimatedPng => "apng",
            compressor_media::ImageFormat::Jpeg => "jpeg",
            compressor_media::ImageFormat::Webp => "webp",
            compressor_media::ImageFormat::AnimatedWebp => "animated_webp",
            _ => "other",
        };
        let item = media
            .entry(kind.into())
            .or_insert(MediaCount { count: 0, bytes: 0 });
        item.count += 1;
        item.bytes += entry.size;
    }
    let tools = plan
        .tools
        .iter()
        .map(|tool| {
            let mut p = plan.clone();
            p.tools = vec![tool.clone()];
            (
                tool.name.clone(),
                compressor_core::tools::check_tools(&p).is_ok(),
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
                for (name, available) in result.tools {
                    println!(
                        "{name}: {}",
                        if available { "available" } else { "missing" }
                    );
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
            list,
            save,
            json,
        } => {
            let plan = new_plan(&game, profile.as_deref(), false)?;
            if let Some(path) = save {
                std::fs::write(path, serde_json::to_vec_pretty(&plan)?)?;
            }
            if json {
                print_json(&plan)?;
            } else {
                for (kind, count) in plan_counts(&plan) {
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
            plan,
            jobs,
            allow_unknown_version,
        } => {
            let plan = if let Some(file) = plan {
                let plan: Plan = serde_json::from_slice(&std::fs::read(file)?)?;
                validate_plan(&plan, &game)?;
                plan
            } else {
                new_plan(&game, profile.as_deref(), allow_unknown_version)?
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
    let manifest: compressor_core::Manifest =
        serde_json::from_slice(&std::fs::read(output.join(".compressor/manifest.json"))?)?;
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
            if let Err(e) = WebpTransformer::validate_output(&path, &CancellationToken::new()).await
            {
                result
                    .errors
                    .push(format!("{}: {e}", entry.path.0.display()));
            }
        }
    }
    Ok(result)
}
