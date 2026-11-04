pub mod estimate;
pub mod sample_limiter;
use compressor_core::{
    planning::{build_plan, hash_input},
    registry::{
        ArchiveRegistry, EngineRegistry, EnvelopeRegistry, PackRegistry, TransformerRegistry,
    },
    Action, Plan,
};
use compressor_engine_renpy::RenpyAdapter;
use compressor_media::{audio::AudioTransformer, webp::WebpTransformer, MediaCount};
pub use estimate::{estimate, Estimate, EstimateOptions};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

/// Registries shared by front ends when preparing and running jobs.
pub struct Registries {
    pub transformers: TransformerRegistry,
    pub engines: EngineRegistry,
    pub packs: PackRegistry,
    pub archives: ArchiveRegistry,
    pub envelopes: EnvelopeRegistry,
    pub engine_specs: BTreeMap<String, Vec<compressor_core::OptionSpec>>,
}

pub fn registries(allow_unknown_version: bool) -> Registries {
    let engines = engine_registry(allow_unknown_version);
    let engine_specs = engine_specs(&engines);
    Registries {
        transformers: registry(),
        engines,
        packs: pack_registry(),
        archives: archive_registry(),
        envelopes: envelope_registry(),
        engine_specs,
    }
}

pub fn registry() -> TransformerRegistry {
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
pub fn engine_registry(allow: bool) -> EngineRegistry {
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
pub fn pack_registry() -> PackRegistry {
    let mut packs = PackRegistry::default();
    packs.register(
        "godot-pck",
        Arc::new(compressor_engine_godot::GodotPackFormat),
    );
    packs
}
pub fn archive_registry() -> ArchiveRegistry {
    let mut registry = ArchiveRegistry::default();
    registry.register("unity-fs", Arc::new(compressor_engine_unity::UnityFs));
    registry
}
pub fn envelope_registry() -> EnvelopeRegistry {
    let mut registry = EnvelopeRegistry::default();
    registry.register(
        "rpgmaker",
        Arc::new(compressor_engine_rpgm::RpgMakerEnvelope),
    );
    registry
}
pub fn engine_specs(
    engines: &EngineRegistry,
) -> BTreeMap<String, Vec<compressor_core::OptionSpec>> {
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
pub fn new_plan(
    game: &Path,
    profile: Option<&str>,
    set: &[String],
    allow: bool,
) -> io::Result<Plan> {
    let engines = engine_registry(allow);
    let (adapter, engine) = engines.detect(game)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no supported engine detected")
    })?;
    let specs = engine_specs(&engines);
    let settings = compressor_core::settings::resolve(profile, set, adapter.id(), &specs)?;
    build_plan(game, adapter.as_ref(), engine, settings, &registry())
}
pub fn plan_counts(plan: &Plan) -> BTreeMap<String, usize> {
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
pub struct PackInfo {
    pub file: compressor_core::StoredPath,
    pub location: String,
    pub format: u32,
    pub version: String,
    pub encrypted: bool,
    pub members: BTreeMap<String, MediaCount>,
    pub note: Option<String>,
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
pub struct RpgMakerInfo {
    pub content_root: compressor_core::StoredPath,
    pub encrypted_images: bool,
    pub encrypted_audio: bool,
    pub key_source: String,
    pub encrypted_count: u64,
    pub encrypted_bytes: u64,
    pub video_count: u64,
    pub video_bytes: u64,
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
pub struct InspectResult {
    pub engine: compressor_core::EngineInfo,
    pub rpgmaker: Option<RpgMakerInfo>,
    pub unity: Vec<UnityInfo>,
    pub media: BTreeMap<String, MediaCount>,
    /// Additional GUI classification; omitted from the existing CLI inspection output.
    #[serde(skip)]
    pub encrypted_media: BTreeMap<String, MediaCount>,
    pub archives: Vec<(String, u64)>,
    pub packs: Vec<PackInfo>,
    pub tools: Vec<(String, bool, BTreeMap<String, bool>)>,
    pub blockers: Vec<String>,
}
#[derive(serde::Serialize)]
pub struct UnityInfo {
    pub path: compressor_core::StoredPath,
    pub version: Option<String>,
    pub platform: String,
    pub compressed: bool,
    pub archive_files: MediaCount,
    pub streamed_resources: MediaCount,
    pub bundles: MediaCount,
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
pub fn inspect(game: &Path) -> io::Result<InspectResult> {
    let plan = new_plan(game, None, &[], false)?;
    let rpgmaker = inspect_rpgmaker(&plan)?;
    let unity = inspect_unity(&plan)?;
    let mut media = BTreeMap::<String, MediaCount>::new();
    let mut encrypted_media = BTreeMap::<String, MediaCount>::new();
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
        let encrypted_kind = if rpgmaker.is_some() {
            match ext.as_str() {
                "rpgmvp" | "png_" => Some("encrypted image"),
                "rpgmvo" | "rpgmvm" | "ogg_" | "m4a_" => Some("encrypted audio"),
                _ => None,
            }
        } else {
            None
        };
        if let Some(kind) = encrypted_kind {
            let count = encrypted_media.entry(kind.into()).or_default();
            count.count += 1;
            count.bytes += entry.size;
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
        encrypted_media,
        archives,
        packs: inspect_packs(game)?,
        tools,
        blockers,
    })
}
#[derive(serde::Serialize)]
pub struct VerifyResult {
    pub checked: usize,
    pub errors: Vec<String>,
}
pub async fn verify(output: &Path) -> io::Result<VerifyResult> {
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

// Resolve existing components, including symlinks, before comparing paths.
fn resolve_output(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::RootDir => resolved.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(value) => {
                resolved.push(value);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = resolved.canonicalize()?,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported path prefix",
                ))
            }
        }
    }
    Ok(resolved)
}

/// Check that a user-selected output is separate from its input and queued outputs.
pub fn validate_output_path(game: &Path, output: &Path, taken: &[PathBuf]) -> io::Result<()> {
    let input = game.canonicalize()?;
    let resolved = resolve_output(output)?;
    if resolved.starts_with(&input) || input.starts_with(&resolved) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input and output must be separate directories",
        ));
    }
    for other in taken {
        if resolve_output(other)? == resolved {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output is already assigned to another game",
            ));
        }
    }
    Ok(())
}

/// Choose a unique destination without modifying the filesystem.
pub fn output_path(
    game: &Path,
    chosen_dir: Option<&Path>,
    taken: &[PathBuf],
) -> io::Result<PathBuf> {
    let name = game
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "game has no folder name"))?;
    let parent = match chosen_dir {
        Some(dir) => dir,
        None => game
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "game has no parent"))?,
    };
    let mut base = name.to_os_string();
    if chosen_dir.is_none() {
        base.push("-compressed");
    }
    let input = game.canonicalize()?;
    let mut n = 1;
    loop {
        let mut candidate_name = base.clone();
        if n > 1 {
            candidate_name.push(format!(" ({n})"));
        }
        let candidate = parent.join(candidate_name);
        let resolved = resolve_output(&candidate)?;
        if resolved.starts_with(&input) || input.starts_with(&resolved) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input and output must be separate directories",
            ));
        }
        let mut collision = false;
        for path in taken {
            if resolve_output(path)? == resolved {
                collision = true;
                break;
            }
        }
        if !collision {
            return Ok(candidate);
        }
        n += 1;
    }
}

#[cfg(test)]
mod output_path_tests {
    use super::*;
    #[test]
    fn chooses_paths_and_rejects_overlapping_destinations() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let game = root.path().join("a/Game");
        std::fs::create_dir_all(&game)?;
        let out = root.path().join("out");
        assert_eq!(
            output_path(&game, None, &[])?,
            root.path().join("a/Game-compressed")
        );
        assert_eq!(output_path(&game, Some(&out), &[])?, out.join("Game"));
        assert_eq!(
            output_path(&game, Some(&out), &[out.join("Game"), out.join("Game (2)")])?,
            out.join("Game (3)")
        );
        assert!(output_path(&game, Some(&game.join("nested")), &[]).is_err());
        #[cfg(unix)]
        {
            let alias = root.path().join("alias");
            std::os::unix::fs::symlink(&game, &alias)?;
            assert!(output_path(&game, Some(&alias), &[]).is_err());
        }
        Ok(())
    }
}

/// Explain why an inspection cannot be queued without unknown-version overrides.
pub fn unsupported_reason(result: &InspectResult) -> Option<String> {
    if let Some(reason) = result.blockers.first() {
        return Some(reason.clone());
    }
    if !result.unity.is_empty()
        && !result.unity.iter().any(|folder| {
            folder.version.as_deref().is_some_and(|version| {
                compressor_engine_unity::archive_params(version, false).is_some()
            })
        })
    {
        let versions = result
            .unity
            .iter()
            .map(|folder| folder.version.as_deref().unwrap_or("unknown"))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!("unsupported Unity version: {versions}"));
    }
    None
}

#[cfg(test)]
mod unsupported_tests {
    use super::*;
    #[test]
    fn inspection_counts_encrypted_rpg_media_by_kind() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        for name in ["data", "js", "img", "audio"] {
            std::fs::create_dir(dir.path().join(name))?;
        }
        std::fs::write(
            dir.path().join("data/System.json"),
            r#"{"hasEncryptedImages":true,"hasEncryptedAudio":true,"encryptionKey":"00000000000000000000000000000000"}"#,
        )?;
        std::fs::write(
            dir.path().join("js/rpg_core.js"),
            "Utils.RPGMAKER_VERSION = '1.6.1';",
        )?;
        std::fs::write(dir.path().join("img/a.rpgmvp"), b"encrypted image")?;
        std::fs::write(dir.path().join("audio/b.rpgmvo"), b"encrypted audio")?;
        let result = inspect(dir.path())?;
        assert_eq!(result.encrypted_media["encrypted image"].count, 1);
        assert_eq!(result.encrypted_media["encrypted audio"].count, 1);
        assert!(serde_json::to_value(&result)?
            .get("encrypted_media")
            .is_none());
        Ok(())
    }
    #[test]
    fn preview_matches_real_plans_for_renpy_and_unity() -> io::Result<()> {
        let renpy = compressor_test_fixtures::Fixture::new()?;
        let unity = tempfile::tempdir()?;
        std::fs::write(unity.path().join("Game.exe"), b"player")?;
        let data = unity.path().join("Game_Data");
        std::fs::create_dir(&data)?;
        let mut header = vec![0; 8];
        header.extend_from_slice(&22u32.to_be_bytes());
        header.extend_from_slice(&[0; 36]);
        header.extend_from_slice(b"2021.3.45f2\0");
        header.extend_from_slice(&19i32.to_le_bytes());
        std::fs::write(data.join("globalgamemanagers"), header)?;
        std::fs::write(data.join("resources.assets"), b"assets")?;
        for root in [renpy.root(), unity.path()] {
            let engines = engine_registry(false);
            let (adapter, engine) = engines.detect(root)?.unwrap();
            let settings = compressor_core::settings::resolve(
                None,
                &[],
                adapter.id(),
                &engine_specs(&engines),
            )?;
            let actual = build_plan(
                root,
                adapter.as_ref(),
                engine.clone(),
                settings.clone(),
                &registry(),
            )?;
            let preview = compressor_core::planning::preview_plan(
                root,
                adapter.as_ref(),
                engine,
                settings,
                &registry(),
            )?;
            assert_eq!(actual.entries.len(), preview.entries.len());
            for (a, b) in actual.entries.iter().zip(&preview.entries) {
                assert_eq!(
                    (&a.path, &a.member, a.size, &a.action),
                    (&b.path, &b.member, b.size, &b.action)
                );
            }
        }
        Ok(())
    }
    #[test]
    fn detects_unknown_renpy_and_unsupported_unity() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        assert_eq!(unsupported_reason(&inspect(fixture.root())?), None);
        std::fs::write(fixture.root().join("renpy/vc_version.py"), "unknown")?;
        assert!(unsupported_reason(&inspect(fixture.root())?)
            .unwrap()
            .contains("engine version"));
        let unity = tempfile::tempdir()?;
        std::fs::write(unity.path().join("Game.exe"), b"exe")?;
        std::fs::create_dir(unity.path().join("Game_Data"))?;
        let mut header = vec![0; 8];
        header.extend_from_slice(&22u32.to_be_bytes());
        header.extend_from_slice(&[0; 36]);
        header.extend_from_slice(b"2019.4.40f1\0");
        header.extend_from_slice(&19i32.to_le_bytes());
        std::fs::write(unity.path().join("Game_Data/globalgamemanagers"), header)?;
        assert!(unsupported_reason(&inspect(unity.path())?)
            .unwrap()
            .contains("2019.4.40f1"));
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestinationState {
    New,
    Resumes,
    AlreadyCompressed,
    Blocked(String),
    NeedsPlan,
}

/// Predict the job runner's metadata and plan checks without changing the destination.
pub fn destination_state(output: &Path, plan: Option<&Plan>) -> DestinationState {
    fn check(output: &Path, plan: Option<&Plan>) -> io::Result<DestinationState> {
        let meta = match std::fs::metadata(output) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DestinationState::New),
            Err(e) => return Err(e),
        };
        if !meta.is_dir() {
            return Ok(DestinationState::Blocked(
                "output is not a directory".into(),
            ));
        }
        if std::fs::read_dir(output)?.next().transpose()?.is_none() {
            return Ok(DestinationState::New);
        }
        let metadata = output.join(".compressor");
        let manifest_path = metadata.join("manifest.json");
        let plan_path = metadata.join("plan.json");
        if !manifest_path.is_file() && !plan_path.is_file() {
            return Ok(DestinationState::Blocked(
                "non-empty output has no compressor manifest".into(),
            ));
        }
        let Some(plan) = plan else {
            return Ok(DestinationState::NeedsPlan);
        };
        compressor_core::output::check_output(&plan.input_root.0, output)?;
        let (existing, complete) = if manifest_path.exists() {
            let manifest = compressor_core::planning::read_manifest(&manifest_path)?;
            (manifest.plan, manifest.complete)
        } else {
            (compressor_core::planning::read_plan(&plan_path)?, false)
        };
        if compressor_core::journal::fingerprint(&existing)
            != compressor_core::journal::fingerprint(plan)
        {
            return Ok(DestinationState::Blocked(
                "output manifest belongs to a different plan".into(),
            ));
        }
        Ok(if complete {
            DestinationState::AlreadyCompressed
        } else {
            DestinationState::Resumes
        })
    }
    check(output, plan).unwrap_or_else(|error| DestinationState::Blocked(error.to_string()))
}

#[cfg(test)]
mod destination_tests {
    use super::*;
    #[test]
    fn checks_destinations_and_plan_metadata() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let plan = new_plan(fixture.root(), None, &[], false)?;
        let root = tempfile::tempdir()?;
        let out = root.path().join("out");
        assert_eq!(destination_state(&out, None), DestinationState::New);
        std::fs::create_dir(&out)?;
        assert_eq!(destination_state(&out, None), DestinationState::New);
        std::fs::write(out.join("other"), "keep")?;
        assert!(matches!(
            destination_state(&out, None),
            DestinationState::Blocked(_)
        ));
        std::fs::remove_file(out.join("other"))?;
        std::fs::create_dir(out.join(".compressor"))?;
        std::fs::write(
            out.join(".compressor/plan.json"),
            serde_json::to_vec(&plan)?,
        )?;
        assert_eq!(destination_state(&out, None), DestinationState::NeedsPlan);
        assert_eq!(
            destination_state(&out, Some(&plan)),
            DestinationState::Resumes
        );
        let manifest = compressor_core::journal::make_manifest(plan.clone(), &[], true);
        std::fs::write(
            out.join(".compressor/manifest.json"),
            serde_json::to_vec(&manifest)?,
        )?;
        assert_eq!(
            destination_state(&out, Some(&plan)),
            DestinationState::AlreadyCompressed
        );
        let mut different = plan.clone();
        different.settings.profile = "other".into();
        assert!(matches!(
            destination_state(&out, Some(&different)),
            DestinationState::Blocked(_)
        ));
        let file = root.path().join("file");
        std::fs::write(&file, "keep")?;
        assert!(matches!(
            destination_state(&file, None),
            DestinationState::Blocked(_)
        ));
        Ok(())
    }
}

/// Convert UI overrides to CLI-compatible arguments and resolve them with the CLI rules.
pub fn resolve_settings(
    profile: &str,
    overrides: &BTreeMap<String, serde_json::Value>,
    engine: &str,
    specs: &BTreeMap<String, Vec<compressor_core::OptionSpec>>,
) -> io::Result<(Vec<String>, compressor_core::ResolvedSettings)> {
    let mut sets = Vec::with_capacity(overrides.len());
    for (key, value) in overrides {
        if !matches!(
            value,
            serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_)
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{key}: expected a scalar setting"),
            ));
        }
        sets.push(format!("{key}={value}"));
    }
    let resolved = compressor_core::settings::resolve(Some(profile), &sets, engine, specs)?;
    Ok((sets, resolved))
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    #[test]
    fn overrides_resolve_as_cli_settings() -> io::Result<()> {
        let specs = engine_specs(&engine_registry(false));
        let overrides = BTreeMap::from([
            (
                "engines.unity.archive_data".into(),
                serde_json::json!(false),
            ),
            ("image.quality".into(), serde_json::json!(72)),
        ]);
        let (sets, settings) = resolve_settings("quality", &overrides, "unity", &specs)?;
        let cli = compressor_core::settings::resolve(Some("quality"), &sets, "unity", &specs)?;
        assert_eq!(serde_json::to_value(settings)?, serde_json::to_value(cli)?);
        assert!(sets.contains(&"engines.unity.archive_data=false".to_string()));
        Ok(())
    }
}
