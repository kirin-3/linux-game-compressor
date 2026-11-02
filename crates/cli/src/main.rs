use clap::{Parser, Subcommand};
use compressor_core::{
    job::{run_job_with_archives, Event},
    planning::{build_plan, hash_input, validate_plan},
    registry::{
        ArchiveRegistry, EngineRegistry, EnvelopeRegistry, PackRegistry, TransformerRegistry,
    },
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
    r.register("cwebp", Arc::new(WebpTransformer));
    r.register("ffmpeg", Arc::new(AudioTransformer));
    r.register(
        "ffmpeg-video",
        Arc::new(compressor_media::video::VideoTransformer),
    );
    r.register(
        "godot-ctex",
        Arc::new(compressor_engine_godot::texture_transform::GodotTextureTransformer),
    );
    r.register(
        "godot-audio",
        Arc::new(compressor_engine_godot::audio_transform::GodotAudioTransformer),
    );
    r
}
fn engine_registry(allow: bool) -> EngineRegistry {
    let mut engines = EngineRegistry::default();
    engines.register(Arc::new(RenpyAdapter {
        allow_unknown_version: allow,
    }));
    engines.register(Arc::new(compressor_engine_godot::adapter::GodotAdapter));
    engines.register(Arc::new(compressor_engine_rpgm::RpgMakerAdapter::default()));
    engines.register(Arc::new(compressor_engine_unity::UnityAdapter {
        allow_unknown_version: allow,
    }));
    engines
}
fn pack_registry() -> PackRegistry {
    let mut packs = PackRegistry::default();
    packs.register(
        "godot-pck",
        Arc::new(compressor_engine_godot::GodotPackFormat),
    );
    packs
}
fn archive_registry() -> ArchiveRegistry {
    let mut registry = ArchiveRegistry::default();
    registry.register("unity-fs", Arc::new(compressor_engine_unity::UnityFs));
    registry
}
fn envelope_registry() -> EnvelopeRegistry {
    let mut registry = EnvelopeRegistry::default();
    registry.register(
        "rpgmaker",
        Arc::new(compressor_engine_rpgm::RpgMakerEnvelope),
    );
    registry
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
    let engines = engine_registry(allow);
    let (adapter, engine) = engines.detect(game)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no supported engine detected")
    })?;
    let specs = engine_specs(&engines);
    let settings = compressor_core::settings::resolve(profile, set, adapter.id(), &specs)?;
    build_plan(game, adapter.as_ref(), engine, settings, &registry())
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
            Action::Remove { reason } => format!("remove: {reason}"),
            Action::Copy { reason } => format!("copy: {reason}"),
            Action::Skip { reason } => format!("skip: {reason}"),
            Action::Rebuild { format } => format!("rebuild: {format}"),
            Action::Archive { archive } => format!("archive: {}", archive.0.display()),
            Action::Assemble {
                format, members, ..
            } => format!("assemble: {format} ({} files)", members.len()),
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}
#[derive(serde::Serialize)]
struct PackInfo {
    file: compressor_core::StoredPath,
    location: String,
    format: u32,
    version: String,
    encrypted: bool,
    members: BTreeMap<String, MediaCount>,
    note: Option<String>,
}
fn inspect_packs(root: &Path) -> io::Result<Vec<PackInfo>> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(Some((start, len))) = compressor_engine_godot::locate_trailer(&path) else {
            continue;
        };
        let Ok(header) = compressor_engine_godot::read_header(&path, start) else {
            continue;
        };
        let encrypted = header.flags & 1 != 0;
        let mut members = BTreeMap::<String, MediaCount>::new();
        let pack = compressor_engine_godot::read_pack(&path, start, len);
        let note = pack
            .as_ref()
            .err()
            .map(|e| format!("{}; copied unchanged", e));
        if let Ok(pack) = pack {
            for member in pack.members {
                let lower = member.path.to_ascii_lowercase();
                let kind = if lower.ends_with(".ctex") {
                    "texture"
                } else if lower.ends_with(".oggvorbisstr") {
                    "ogg_vorbis"
                } else if lower.ends_with(".mp3str") {
                    "mp3"
                } else {
                    "other"
                };
                let count = members
                    .entry(kind.into())
                    .or_insert(MediaCount { count: 0, bytes: 0 });
                count.count += 1;
                count.bytes += member.size;
            }
        }
        result.push(PackInfo {
            file: compressor_core::StoredPath(entry.file_name().into()),
            location: if start == 0 { "standalone" } else { "embedded" }.into(),
            format: header.format,
            version: format!("{}.{}.{}", header.major, header.minor, header.patch),
            encrypted,
            members,
            note,
        });
    }
    result.sort_by(|a, b| a.file.0.cmp(&b.file.0));
    Ok(result)
}
#[derive(serde::Serialize)]
struct RpgMakerInfo {
    content_root: compressor_core::StoredPath,
    encrypted_images: bool,
    encrypted_audio: bool,
    key_source: String,
    encrypted_count: u64,
    encrypted_bytes: u64,
    video_count: u64,
    video_bytes: u64,
}
fn inspect_rpgmaker(plan: &Plan) -> io::Result<Option<RpgMakerInfo>> {
    if !plan.engine.name.starts_with("RPG Maker ") {
        return Ok(None);
    }
    let root = &plan.input_root.0;
    let content = if root.join("www/data/System.json").is_file() {
        Path::new("www")
    } else {
        Path::new("")
    };
    let system: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(content).join("data/System.json"))?)?;
    let mut info = RpgMakerInfo {
        content_root: compressor_core::StoredPath(content.to_owned()),
        encrypted_images: system["hasEncryptedImages"].as_bool().unwrap_or(false),
        encrypted_audio: system["hasEncryptedAudio"].as_bool().unwrap_or(false),
        key_source: plan
            .engine
            .notes
            .iter()
            .find(|s| s.starts_with("key ") || s.as_str() == "encryption key not found")
            .cloned()
            .unwrap_or_default(),
        encrypted_count: 0,
        encrypted_bytes: 0,
        video_count: 0,
        video_bytes: 0,
    };
    for entry in &plan.entries {
        if entry.member.is_some() {
            continue;
        }
        let Ok(local) = entry.path.0.strip_prefix(content) else {
            continue;
        };
        let extension = local
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            extension.as_str(),
            "rpgmvp" | "rpgmvo" | "rpgmvm" | "png_" | "ogg_" | "m4a_"
        ) {
            info.encrypted_count += 1;
            info.encrypted_bytes += entry.size;
        }
        if (local.starts_with("movies") || local.starts_with("videos"))
            && compressor_media::video::has_video_magic(&root.join(&entry.path.0)).unwrap_or(false)
        {
            info.video_count += 1;
            info.video_bytes += entry.size;
        }
    }
    Ok(Some(info))
}
#[derive(serde::Serialize)]
struct InspectResult {
    engine: compressor_core::EngineInfo,
    rpgmaker: Option<RpgMakerInfo>,
    unity: Vec<UnityInfo>,
    media: BTreeMap<String, MediaCount>,
    archives: Vec<(String, u64)>,
    packs: Vec<PackInfo>,
    tools: Vec<(String, bool, BTreeMap<String, bool>)>,
    blockers: Vec<String>,
}
#[derive(serde::Serialize)]
struct UnityInfo {
    path: compressor_core::StoredPath,
    version: Option<String>,
    platform: String,
    compressed: bool,
    archive_files: MediaCount,
    streamed_resources: MediaCount,
    bundles: MediaCount,
}
fn inspect_unity(plan: &Plan) -> io::Result<Vec<UnityInfo>> {
    if plan.engine.name != "Unity" {
        return Ok(vec![]);
    }
    let root = &plan.input_root.0;
    let mut result = Vec::new();
    for folder in compressor_engine_unity::data_folders(root)? {
        let header = compressor_engine_unity::read_player_header(
            &root.join(&folder).join("globalgamemanagers"),
        )
        .ok();
        let platform = header
            .as_ref()
            .map(|h| match h.platform {
                5 => "Windows 32-bit".into(),
                19 => "Windows 64-bit".into(),
                17 => "Linux 32-bit".into(),
                24 => "Linux 64-bit".into(),
                25 => "Linux universal".into(),
                n => format!("platform {n}"),
            })
            .unwrap_or_else(|| "unknown".into());
        let mut info = UnityInfo {
            path: compressor_core::StoredPath(folder.clone()),
            version: header.map(|h| h.version),
            platform,
            compressed: root.join(&folder).join("data.unity3d").is_file(),
            archive_files: MediaCount { count: 0, bytes: 0 },
            streamed_resources: MediaCount { count: 0, bytes: 0 },
            bundles: MediaCount { count: 0, bytes: 0 },
        };
        for entry in &plan.entries {
            if entry.member.is_some()
                || !entry.path.0.starts_with(&folder)
                || matches!(entry.action, Action::Assemble { .. })
            {
                continue;
            }
            if matches!(entry.action, Action::Archive { .. }) {
                info.archive_files.count += 1;
                info.archive_files.bytes += entry.size;
            }
            if entry
                .path
                .0
                .extension()
                .is_some_and(|ext| ext == "resource")
            {
                info.streamed_resources.count += 1;
                info.streamed_resources.bytes += entry.size;
            }
            use std::io::Read;
            if let Ok(mut file) = std::fs::File::open(root.join(&entry.path.0)) {
                let mut magic = [0u8; 8];
                if file.read_exact(&mut magic).is_ok()
                    && &magic == b"UnityFS\0"
                    && entry
                        .path
                        .0
                        .file_name()
                        .is_some_and(|name| name != "data.unity3d")
                {
                    info.bundles.count += 1;
                    info.bundles.bytes += entry.size;
                }
            }
        }
        result.push(info);
    }
    Ok(result)
}
fn inspect(game: &Path) -> io::Result<InspectResult> {
    let plan = new_plan(game, None, &[], false)?;
    let rpgmaker = inspect_rpgmaker(&plan)?;
    let unity = inspect_unity(&plan)?;
    let mut media = BTreeMap::<String, MediaCount>::new();
    let mut archives = Vec::new();
    for entry in &plan.entries {
        if entry.member.is_some() || matches!(entry.action, Action::Assemble { .. }) {
            continue;
        }
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
    let mut inspected_tools = plan.tools.clone();
    if rpgmaker.as_ref().is_some_and(|r| r.video_count > 0) {
        let tool = inspected_tools.iter_mut().find(|t| t.name == "ffmpeg");
        let features = [
            "libvpx-vp9",
            "libopus",
            "decoder:libvpx",
            "decoder:libvpx-vp9",
        ];
        if let Some(tool) = tool {
            for feature in features {
                if !tool.features.iter().any(|f| f == feature) {
                    tool.features.push(feature.into());
                }
            }
        } else {
            inspected_tools.push(compressor_core::ToolSpec {
                name: "ffmpeg".into(),
                package: "ffmpeg".into(),
                features: features.into_iter().map(str::to_owned).collect(),
            });
        }
    }
    let tools = inspected_tools
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
        rpgmaker,
        unity,
        media,
        archives,
        packs: inspect_packs(game)?,
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
        if !manifest.entries.iter().any(|record| {
            record.path == entry.path
                && record.member == entry.member
                && record.input_hash == entry.hash
        }) {
            result.errors.push(format!(
                "{}: missing manifest record",
                compressor_core::journal::display_path(&entry.path, entry.member.as_deref())
                    .0
                    .display()
            ));
        }
    }
    for archive in &manifest.plan.entries {
        let Action::Assemble {
            format, members, ..
        } = &archive.action
        else {
            continue;
        };
        if !manifest
            .entries
            .iter()
            .any(|record| record.path == archive.path && record.outcome == "archived")
        {
            continue;
        }
        let path = output.join(&archive.path.0);
        let handler = archive_registry().get(format);
        let directory = handler
            .as_ref()
            .ok_or_else(|| io::Error::other("missing archive format"))
            .and_then(|h| h.read_directory(&path));
        for name in members {
            let label = format!("{}::{name}", archive.path.0.display());
            let relative = archive.path.0.parent().unwrap_or(Path::new("")).join(name);
            if std::fs::symlink_metadata(output.join(&relative)).is_ok() {
                result.errors.push(format!(
                    "{}: archived file exists loose",
                    relative.display()
                ));
            }
            let planned = manifest.plan.entries.iter().find(|entry| entry.path.0 == relative && matches!(&entry.action, Action::Archive { archive: target } if target == &archive.path));
            let checked = match (&handler, &directory, planned) {
                (Some(handler), Ok(directory), Some(planned)) => directory
                    .iter()
                    .find(|item| &item.name == name)
                    .ok_or_else(|| io::Error::other("member missing"))
                    .and_then(|item| handler.hash_member(&path, item))
                    .and_then(|(size, hash)| {
                        if size == planned.size && hash == planned.hash {
                            Ok(())
                        } else {
                            Err(io::Error::other("hash mismatch"))
                        }
                    }),
                _ => Err(io::Error::other("archive or member missing")),
            };
            result.checked += 1;
            if let Err(error) = checked {
                result.errors.push(format!("{label}: {error}"));
            }
        }
    }
    let mut directories = std::collections::HashMap::new();
    for entry in &manifest.entries {
        if entry.outcome == "removed" && entry.member.is_none() {
            result.checked += 1;
            if std::fs::symlink_metadata(output.join(&entry.path.0)).is_ok() {
                result
                    .errors
                    .push(format!("{}: removed file exists", entry.path.0.display()));
            }
            continue;
        }
        let Some(hash) = &entry.output_hash else {
            continue;
        };
        result.checked += 1;
        let path = output.join(&entry.path.0);
        if let Some(member_name) = &entry.member {
            if !entry.outcome.starts_with("converted") {
                result.checked -= 1;
                continue;
            }
            let name = compressor_core::journal::display_path(&entry.path, Some(member_name));
            let pack_record = manifest
                .entries
                .iter()
                .find(|r| r.path == entry.path && r.member.is_none());
            if pack_record.is_some_and(|r| r.failure) {
                continue;
            }
            let format = manifest.plan.entries.iter().find_map(|p| {
                if p.path == entry.path && p.member.is_none() {
                    if let Action::Rebuild { format } = &p.action {
                        Some(format.as_str())
                    } else {
                        None
                    }
                } else {
                    None
                }
            });
            let Some(handler) = format.and_then(|format| pack_registry().get(format)) else {
                result
                    .errors
                    .push(format!("{}: missing pack format", name.0.display()));
                continue;
            };
            if !directories.contains_key(&entry.path) {
                match handler.read_directory(&path) {
                    Ok(directory) => {
                        directories.insert(entry.path.clone(), directory);
                    }
                    Err(error) => {
                        result.errors.push(format!("{}: {error}", name.0.display()));
                        continue;
                    }
                }
            }
            let bytes = directories[&entry.path]
                .iter()
                .find(|m| &m.name == member_name)
                .ok_or_else(|| io::Error::other("member missing"))
                .and_then(|member| handler.read_member(&path, member));
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(e) => {
                    result.errors.push(format!("{}: {e}", name.0.display()));
                    continue;
                }
            };
            if blake3::hash(&bytes).to_hex().as_str() != hash {
                result
                    .errors
                    .push(format!("{}: hash mismatch", name.0.display()));
                continue;
            }
            if entry.outcome.starts_with("converted") {
                let planned = manifest
                    .plan
                    .entries
                    .iter()
                    .find(|p| p.path == entry.path && p.member == entry.member);
                let checked = match planned.map(|p| &p.action) {
                    Some(Action::Convert {
                        encoder, settings, ..
                    }) => {
                        match registry().get(encoder) {
                            Some(transformer) => {
                                let file = tempfile::NamedTempFile::new()?;
                                std::fs::write(file.path(), bytes)?;
                                // A matching hash identifies the bytes already checked against their source at conversion time.
                                transformer
                                    .validate(
                                        file.path(),
                                        file.path(),
                                        settings,
                                        &CancellationToken::new(),
                                    )
                                    .await
                            }
                            None => Err(io::Error::other("missing transformer")),
                        }
                    }
                    _ => Err(io::Error::other("missing conversion settings")),
                };
                if let Err(e) = checked {
                    result.errors.push(format!("{}: {e}", name.0.display()));
                }
            }
            continue;
        }
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
                    encoder,
                    settings,
                    envelope: Some(envelope),
                    ..
                }) => {
                    match (
                        registry().get(encoder),
                        envelope_registry().get(&envelope.format),
                    ) {
                        (Some(transformer), Some(handler)) => {
                            let source =
                                std::fs::read(manifest.plan.input_root.0.join(&entry.path.0))
                                    .and_then(|b| handler.unwrap(&b, &envelope.params));
                            let result_bytes = std::fs::read(&path)
                                .and_then(|b| handler.unwrap(&b, &envelope.params));
                            match (source, result_bytes) {
                                (Ok(source), Ok(result_bytes)) => {
                                    let input_temp = tempfile::NamedTempFile::new()?;
                                    let output_temp = tempfile::NamedTempFile::new()?;
                                    std::fs::write(input_temp.path(), source)?;
                                    std::fs::write(output_temp.path(), result_bytes)?;
                                    transformer
                                        .validate(
                                            input_temp.path(),
                                            output_temp.path(),
                                            settings,
                                            &CancellationToken::new(),
                                        )
                                        .await
                                }
                                (Err(e), _) | (_, Err(e)) => Err(e),
                            }
                        }
                        _ => Err(io::Error::other("missing transformer or envelope")),
                    }
                }
                Some(Action::Convert {
                    encoder,
                    settings: settings @ compressor_core::ConvertSettings::Video(_),
                    ..
                }) => match registry().get(encoder) {
                    Some(transformer) => {
                        transformer
                            .validate(
                                &manifest.plan.input_root.0.join(&entry.path.0),
                                &path,
                                settings,
                                &CancellationToken::new(),
                            )
                            .await
                    }
                    None => Err(io::Error::other("missing transformer")),
                },
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
