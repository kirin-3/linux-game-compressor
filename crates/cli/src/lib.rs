use clap::{Parser, Subcommand};
use compressor_app::{
    archive_registry, engine_registry, engine_specs, envelope_registry, inspect, new_plan,
    pack_registry, plan_counts, registry, verify,
};
use compressor_core::{
    job::{run_job_with_archives, Event},
    planning::validate_plan,
    Action,
};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal},
    path::PathBuf,
};
use tokio_util::sync::CancellationToken;

mod install;

#[derive(Parser)]
#[command(
    name = "linux-game-compressor",
    version,
    about = "Make Ren'Py, Godot, Unity and RPG Maker games smaller.",
    after_help = "Run without arguments to open the desktop app."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Show effective settings for a game or engine
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
    /// Detect a game's engine and list its media
    Inspect {
        game: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Show or save what compressing a game would do
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
    /// Compress a game into a separate output folder
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
    /// Check a compressed output against its manifest
    Verify {
        output: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Install this program for the current user, with an app menu entry
    Install,
    /// Remove what `install` added
    Uninstall,
    /// Print this program's license and third-party licenses
    Licenses,
}
fn print_json<T: serde::Serialize>(value: &T) -> io::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
/// Runs the command line from the process arguments and returns the exit code.
pub fn main() -> i32 {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let result = runtime.block_on(run(cli));
    // Don't wait for detached blocking work; the process is about to exit.
    runtime.shutdown_background();
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            if e.kind() == io::ErrorKind::Interrupted {
                130
            } else {
                2
            }
        }
    }
}
async fn run(cli: Cli) -> io::Result<i32> {
    match cli.command {
        Commands::Install => install::install().map(|()| 0),
        Commands::Uninstall => install::uninstall().map(|()| 0),
        Commands::Licenses => {
            print!("{}", install::LICENSES);
            Ok(0)
        }
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
                if let Some(rpg) = &result.rpgmaker {
                    println!(
                        "RPG Maker content root: {}",
                        if rpg.content_root.0.as_os_str().is_empty() {
                            ".".into()
                        } else {
                            rpg.content_root.0.display().to_string()
                        }
                    );
                    println!(
                        "Encryption: images {}, audio {}; {}",
                        rpg.encrypted_images, rpg.encrypted_audio, rpg.key_source
                    );
                    println!(
                        "Encrypted: {} files, {} bytes; video: {} files, {} bytes",
                        rpg.encrypted_count, rpg.encrypted_bytes, rpg.video_count, rpg.video_bytes
                    );
                }
                for unity in &result.unity {
                    println!(
                        "Unity data: {} ({}; {}; {})",
                        unity.path.0.display(),
                        unity.version.as_deref().unwrap_or("unknown"),
                        unity.platform,
                        if unity.compressed {
                            "compressed"
                        } else {
                            "not compressed"
                        }
                    );
                    println!("  to archive: {} files, {} bytes; streamed resources: {} files, {} bytes; bundles: {} files, {} bytes", unity.archive_files.count, unity.archive_files.bytes, unity.streamed_resources.count, unity.streamed_resources.bytes, unity.bundles.count, unity.bundles.bytes);
                }
                for (name, count) in result.media {
                    println!("{name}: {} files, {} bytes", count.count, count.bytes);
                }
                for (name, size) in result.archives {
                    println!("archive: {name} ({size} bytes)");
                }
                for pack in result.packs {
                    println!(
                        "pack: {} ({}, format {}, Godot {}, {})",
                        pack.file.0.display(),
                        pack.location,
                        pack.format,
                        pack.version,
                        if pack.encrypted {
                            "encrypted"
                        } else {
                            "unencrypted"
                        }
                    );
                    for (kind, count) in pack.members {
                        println!("  {kind}: {} members, {} bytes", count.count, count.bytes);
                    }
                    if let Some(note) = pack.note {
                        println!("  {note}");
                    }
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
                let mut removals = BTreeMap::<String, (usize, u64)>::new();
                for entry in &plan.entries {
                    if let Action::Remove { reason } = &entry.action {
                        let row = removals.entry(reason.clone()).or_default();
                        row.0 += 1;
                        row.1 += entry.size;
                    }
                }
                for (reason, (count, bytes)) in removals {
                    println!("remove: {count} files, {bytes} bytes ({reason})");
                }
                println!(
                    "packs rebuilt: {}",
                    plan.entries
                        .iter()
                        .filter(|e| matches!(e.action, Action::Rebuild { .. }))
                        .count()
                );
                let archives: Vec<_> = plan
                    .entries
                    .iter()
                    .filter_map(|e| match &e.action {
                        Action::Assemble { members, .. } => Some((members.len(), e.size)),
                        _ => None,
                    })
                    .collect();
                println!(
                    "archives to write: {} ({} files, {} bytes)",
                    archives.len(),
                    archives.iter().map(|(count, _)| count).sum::<usize>(),
                    archives.iter().map(|(_, size)| size).sum::<u64>()
                );
                let mut by_media = BTreeMap::<String, usize>::new();
                for entry in &plan.entries {
                    let media = match &entry.action {
                        Action::Convert { kind, .. } => format!("{kind:?}").to_lowercase(),
                        _ if entry.member.is_some() => {
                            let name = entry.member.as_deref().unwrap_or("").to_ascii_lowercase();
                            if name.ends_with(".ctex") {
                                "image".into()
                            } else if [".oggvorbisstr", ".mp3str", ".sample"]
                                .iter()
                                .any(|suffix| name.ends_with(suffix))
                            {
                                "audio".into()
                            } else {
                                "other".into()
                            }
                        }
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
                        Action::Remove { reason } => format!("remove: {reason}"),
                        Action::Copy { reason } => format!("copy: {reason}"),
                        Action::Skip { reason } => format!("skip: {reason}"),
                        Action::Rebuild { format } => format!("rebuild: {format}"),
                        Action::Archive { archive } => format!("archive: {}", archive.0.display()),
                        Action::Assemble {
                            format, members, ..
                        } => format!("assemble: {format} ({} files)", members.len()),
                    };
                    *by_media.entry(format!("{media} {action}")).or_default() += 1;
                }
                for (kind, count) in by_media {
                    println!("{kind}: {count}");
                }
                if list {
                    for e in &plan.entries {
                        let action = match &e.action {
                            Action::Archive { archive } => {
                                format!("archive: {}", archive.0.display())
                            }
                            Action::Assemble {
                                format, members, ..
                            } => format!("assemble: {format} ({} files)", members.len()),
                            _ => format!("{:?}", e.action),
                        };
                        println!(
                            "{}: {}",
                            compressor_core::journal::display_path(&e.path, e.member.as_deref())
                                .0
                                .display(),
                            action
                        );
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
            let result = run_job_with_archives(
                &plan,
                &output,
                &registry(),
                &pack_registry(),
                &envelope_registry(),
                &archive_registry(),
                jobs,
                token,
                force,
                sender,
            )
            .await;
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
