//! Engine adapter: which IoStore containers are rewritten and which are copied.

use crate::{
    detect::{self, Project},
    toc::{self, Toc, TocError},
};
use compressor_core::{
    registry::{EngineAdapter, RewritePlan},
    Action, EngineInfo, OptionKind, OptionSpec, ResolvedSettings,
};
use std::{
    io,
    path::{Path, PathBuf},
};

pub const FORMAT: &str = "unreal-iostore";
pub const OPTION: &str = "compress_containers";

/// What happens to one container (a `.utoc` with its `.ucas`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Compress,
    Copy(&'static str),
}

/// A container in a project's Paks folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// File stem, such as `EscapeFrom-Windows`.
    pub name: String,
    pub utoc: PathBuf,
    pub ucas: PathBuf,
    pub verdict: Verdict,
}

pub fn compress_enabled(settings: &ResolvedSettings) -> bool {
    settings
        .engine_options
        .get(OPTION)
        .and_then(|v| v.value.as_bool())
        != Some(false)
}

/// Decide a container from its files. `utoc` and `ucas` are absolute paths.
pub fn judge(utoc: &Path, ucas: &Path, enabled: bool) -> (Verdict, Option<Toc>) {
    let parsed = std::fs::read(utoc)
        .map_err(|_| TocError::Unreadable("cannot read file".into()))
        .and_then(|bytes| Toc::parse(&bytes));
    // A symbolic link is not a regular file here, so it is copied as a link.
    let ucas_len = std::fs::symlink_metadata(ucas)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len());
    let is_global = utoc.file_stem().is_some_and(|s| s == "global");
    let reason = if !enabled {
        Some("compression disabled")
    } else if matches!(parsed, Err(TocError::Unreadable(_))) || ucas_len.is_none() {
        Some("unreadable container")
    } else if is_global {
        Some("engine global container")
    } else {
        match &parsed {
            Err(_) => Some("unsupported container version"),
            Ok(t) if t.flags & toc::FLAG_ENCRYPTED != 0 => Some("encrypted container"),
            Ok(t) if t.flags & toc::FLAG_SIGNED != 0 => Some("signed container"),
            Ok(t) if t.is_compressed() => Some("already compressed container"),
            Ok(t)
                if t.partition_count != 1
                    || t.flags & toc::FLAG_ON_DEMAND != 0
                    || t.has_memory_mapped_chunk()
                    || !t.is_raw_and_contiguous(ucas_len.unwrap_or(0)) =>
            {
                Some("unsupported container layout")
            }
            Ok(_) => None,
        }
    };
    (reason.map_or(Verdict::Compress, Verdict::Copy), parsed.ok())
}

/// All containers of a project, sorted by name.
pub fn containers(root: &Path, project: &Project, enabled: bool) -> io::Result<Vec<Container>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root.join(&project.paks))? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || path.extension().is_none_or(|x| x != "utoc") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let ucas = path.with_extension("ucas");
        let (verdict, _) = judge(&path, &ucas, enabled);
        found.push(Container {
            name: stem.to_owned(),
            utoc: project.paks.join(format!("{stem}.utoc")),
            ucas: project.paks.join(format!("{stem}.ucas")),
            verdict,
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// What `inspect` shows for one container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerInfo {
    pub name: String,
    pub utoc: PathBuf,
    /// Table-of-contents version, when the file has the IoStore magic.
    pub version: Option<u8>,
    pub compressed: bool,
    pub methods: Vec<String>,
    pub encrypted: bool,
    pub signed: bool,
    pub ucas_bytes: u64,
    pub compress: bool,
    pub reason: Option<&'static str>,
}

/// Describe every container of every project under `root`.
pub fn inspect(root: &Path, enabled: bool) -> io::Result<Vec<ContainerInfo>> {
    let mut all = Vec::new();
    for project in detect::find_projects(root)? {
        for container in containers(root, &project, enabled)? {
            let utoc = root.join(&container.utoc);
            let header = std::fs::read(&utoc).unwrap_or_default();
            let version = (header.len() >= 20 && &header[..16] == toc::MAGIC).then(|| header[16]);
            let parsed = Toc::parse(&header).ok();
            all.push(ContainerInfo {
                name: container.name,
                version,
                compressed: parsed.as_ref().is_some_and(Toc::is_compressed),
                methods: parsed
                    .as_ref()
                    .map(|t| t.method_names.clone())
                    .unwrap_or_default(),
                encrypted: parsed
                    .as_ref()
                    .is_some_and(|t| t.flags & toc::FLAG_ENCRYPTED != 0),
                signed: parsed
                    .as_ref()
                    .is_some_and(|t| t.flags & toc::FLAG_SIGNED != 0),
                ucas_bytes: std::fs::metadata(root.join(&container.ucas)).map_or(0, |m| m.len()),
                compress: container.verdict == Verdict::Compress,
                reason: match container.verdict {
                    Verdict::Copy(reason) => Some(reason),
                    Verdict::Compress => None,
                },
                utoc: container.utoc,
            });
        }
    }
    Ok(all)
}

#[derive(Default)]
pub struct UnrealAdapter;

impl UnrealAdapter {
    fn all_containers(root: &Path, settings: &ResolvedSettings) -> io::Result<Vec<Container>> {
        let enabled = compress_enabled(settings);
        let mut all = Vec::new();
        for project in detect::find_projects(root)? {
            all.extend(containers(root, &project, enabled)?);
        }
        Ok(all)
    }
}

impl EngineAdapter for UnrealAdapter {
    fn id(&self) -> &'static str {
        "unreal"
    }
    fn options(&self) -> Vec<OptionSpec> {
        vec![OptionSpec {
            name: OPTION.into(),
            kind: OptionKind::Bool,
            default: serde_json::json!(true),
        }]
    }
    fn detect(&self, root: &Path) -> io::Result<Option<EngineInfo>> {
        let projects = detect::find_projects(root)?;
        if projects.is_empty() {
            return Ok(None);
        }
        let version = projects
            .iter()
            .find_map(|project| detect::engine_version(root, project));
        Ok(Some(EngineInfo {
            name: "Unreal Engine".into(),
            version,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        }))
    }
    fn rewrites(
        &self,
        root: &Path,
        _: &EngineInfo,
        settings: &ResolvedSettings,
    ) -> io::Result<Vec<RewritePlan>> {
        Ok(Self::all_containers(root, settings)?
            .into_iter()
            .filter(|c| c.verdict == Verdict::Compress)
            .map(|c| RewritePlan {
                path: c.ucas,
                partner: c.utoc,
                format: FORMAT.into(),
                params: serde_json::json!({ "method": "Zlib" }),
            })
            .collect())
    }
    fn classify(
        &self,
        root: &Path,
        path: &Path,
        _: &EngineInfo,
        settings: &ResolvedSettings,
    ) -> io::Result<Action> {
        let other = || Action::Copy {
            reason: "other game file".into(),
        };
        let is_container_file = path.extension().is_some_and(|x| x == "utoc" || x == "ucas");
        if !is_container_file {
            return Ok(other());
        }
        for container in Self::all_containers(root, settings)? {
            let is_utoc = container.utoc == path;
            if !(is_utoc || container.ucas == path) {
                continue;
            }
            // An unpaired `.ucas` is not a container file.
            if !is_utoc && !root.join(&container.utoc).is_file() {
                break;
            }
            return Ok(Action::Copy {
                reason: match container.verdict {
                    Verdict::Copy(reason) => reason.into(),
                    Verdict::Compress => "container to rewrite".into(),
                },
            });
        }
        Ok(other())
    }
    fn plan_notes(
        &self,
        root: &Path,
        _: &EngineInfo,
        settings: &ResolvedSettings,
        _: &[compressor_core::Entry],
    ) -> Vec<String> {
        Self::all_containers(root, settings)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| match c.verdict {
                Verdict::Copy(reason) => Some(format!("{}: copied unchanged ({reason})", c.name)),
                Verdict::Compress => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_core::Resolved;
    use compressor_test_fixtures::iostore::{self, Chunk, Options};
    use std::fs;

    fn chunks() -> Vec<Chunk> {
        vec![
            Chunk {
                kind: 2,
                data: iostore::sample_bytes(1, 10_000),
            },
            Chunk {
                kind: 9,
                data: iostore::sample_bytes(2, 4096),
            },
        ]
    }

    /// A game with `Game/Engine` and one container named `name` built with `options`.
    fn game(name: &str, options: &Options) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let paks = temp.path().join("Game/P/Content/Paks");
        fs::create_dir_all(&paks).unwrap();
        fs::create_dir_all(temp.path().join("Game/Engine")).unwrap();
        let built = iostore::build(&chunks(), options);
        fs::write(paks.join(format!("{name}.utoc")), built.utoc).unwrap();
        fs::write(paks.join(format!("{name}.ucas")), built.ucas).unwrap();
        fs::write(paks.join("Other.pak"), b"pak").unwrap();
        temp
    }

    fn settings(compress: bool) -> ResolvedSettings {
        let mut settings = ResolvedSettings::default();
        settings.engine_options.insert(
            OPTION.into(),
            Resolved {
                value: serde_json::json!(compress),
                source: "test".into(),
            },
        );
        settings
    }

    fn verdict(root: &Path, compress: bool) -> Verdict {
        let project = &detect::find_projects(root).unwrap()[0];
        containers(root, project, compress)
            .unwrap()
            .remove(0)
            .verdict
    }

    #[test]
    fn raw_container_is_planned_as_a_pair() {
        let temp = game("Game-Windows", &Options::default());
        let info = UnrealAdapter.detect(temp.path()).unwrap().unwrap();
        assert_eq!(info.name, "Unreal Engine");
        assert_eq!(info.version, None);
        let pairs = UnrealAdapter
            .rewrites(temp.path(), &info, &settings(true))
            .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs[0].path,
            Path::new("Game/P/Content/Paks/Game-Windows.ucas")
        );
        assert_eq!(
            pairs[0].partner,
            Path::new("Game/P/Content/Paks/Game-Windows.utoc")
        );
        assert_eq!(pairs[0].format, FORMAT);
        assert!(UnrealAdapter
            .plan_notes(temp.path(), &info, &settings(true), &[])
            .is_empty());
    }

    #[test]
    fn each_copy_reason_applies_in_its_own_case() {
        let cases: Vec<(&str, &'static str, Options)> = vec![
            (
                "Game",
                "unsupported container version",
                Options {
                    version: 3,
                    ..Options::default()
                },
            ),
            (
                "Game",
                "encrypted container",
                Options {
                    flags: iostore::FLAG_ENCRYPTED,
                    ..Options::default()
                },
            ),
            (
                "Game",
                "signed container",
                Options {
                    flags: iostore::FLAG_SIGNED | iostore::FLAG_INDEXED,
                    ..Options::default()
                },
            ),
            (
                "Game",
                "already compressed container",
                Options {
                    flags: iostore::FLAG_COMPRESSED | iostore::FLAG_INDEXED,
                    method_names: vec!["Oodle".into()],
                    ..Options::default()
                },
            ),
            (
                "Game",
                "unsupported container layout",
                Options {
                    gap_before_block: Some(1),
                    ..Options::default()
                },
            ),
            (
                "Game",
                "unsupported container layout",
                Options {
                    partitions: 2,
                    ..Options::default()
                },
            ),
            (
                "Game",
                "unsupported container layout",
                Options {
                    memory_mapped_chunk: Some(0),
                    ..Options::default()
                },
            ),
            (
                "Game",
                "unsupported container layout",
                Options {
                    flags: iostore::FLAG_ON_DEMAND,
                    ..Options::default()
                },
            ),
            ("global", "engine global container", Options::default()),
        ];
        for (name, reason, options) in cases {
            let temp = game(name, &options);
            assert_eq!(
                verdict(temp.path(), true),
                Verdict::Copy(reason),
                "{reason}"
            );
            let info = UnrealAdapter.detect(temp.path()).unwrap().unwrap();
            assert!(UnrealAdapter
                .rewrites(temp.path(), &info, &settings(true))
                .unwrap()
                .is_empty());
            let notes = UnrealAdapter.plan_notes(temp.path(), &info, &settings(true), &[]);
            assert_eq!(notes, vec![format!("{name}: copied unchanged ({reason})")]);
            let action = UnrealAdapter
                .classify(
                    temp.path(),
                    Path::new(&format!("Game/P/Content/Paks/{name}.ucas")),
                    &info,
                    &settings(true),
                )
                .unwrap();
            assert!(matches!(action, Action::Copy { reason: r } if r == reason));
        }
    }

    #[test]
    fn damaged_and_unpaired_containers_are_unreadable() {
        let temp = game("Game", &Options::default());
        let paks = temp.path().join("Game/P/Content/Paks");
        let good = fs::read(paks.join("Game.utoc")).unwrap();
        for bad in [
            good[..60].to_vec(),
            b"not an iostore file at all, but long enough......".to_vec(),
        ] {
            fs::write(paks.join("Game.utoc"), bad).unwrap();
            assert_eq!(
                verdict(temp.path(), true),
                Verdict::Copy("unreadable container")
            );
        }
        fs::write(paks.join("Game.utoc"), good).unwrap();
        fs::remove_file(paks.join("Game.ucas")).unwrap();
        assert_eq!(
            verdict(temp.path(), true),
            Verdict::Copy("unreadable container")
        );
    }

    #[test]
    fn symlinked_ucas_is_not_rewritten() {
        let temp = game("Game", &Options::default());
        let paks = temp.path().join("Game/P/Content/Paks");
        fs::rename(paks.join("Game.ucas"), temp.path().join("real.ucas")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("real.ucas"), paks.join("Game.ucas")).unwrap();
        assert_eq!(
            verdict(temp.path(), true),
            Verdict::Copy("unreadable container")
        );
    }

    #[test]
    fn unusual_method_name_length_is_copied() {
        let temp = game("Game", &Options::default());
        let path = temp.path().join("Game/P/Content/Paks/Game.utoc");
        let mut bytes = fs::read(&path).unwrap();
        bytes[40..44].copy_from_slice(&0u32.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            verdict(temp.path(), true),
            Verdict::Copy("unsupported container layout")
        );
    }

    #[test]
    fn option_values_switch_compression() {
        let temp = game("Game", &Options::default());
        assert_eq!(verdict(temp.path(), true), Verdict::Compress);
        assert_eq!(
            verdict(temp.path(), false),
            Verdict::Copy("compression disabled")
        );
        assert!(compress_enabled(&settings(true)) && !compress_enabled(&settings(false)));
        assert!(
            compress_enabled(&ResolvedSettings::default()),
            "defaults to true"
        );
        let info = UnrealAdapter.detect(temp.path()).unwrap().unwrap();
        assert!(UnrealAdapter
            .rewrites(temp.path(), &info, &settings(false))
            .unwrap()
            .is_empty());
        assert_eq!(
            UnrealAdapter.plan_notes(temp.path(), &info, &settings(false), &[]),
            vec!["Game: copied unchanged (compression disabled)"]
        );
        assert_eq!(UnrealAdapter.options()[0].default, serde_json::json!(true));
    }

    #[test]
    fn other_files_are_copied_with_their_reason() {
        let temp = game("Game", &Options::default());
        let info = UnrealAdapter.detect(temp.path()).unwrap().unwrap();
        for file in [
            "Game/P/Content/Paks/Other.pak",
            "Game/P/Content/Movies/Intro.mp4",
        ] {
            let action = UnrealAdapter
                .classify(temp.path(), Path::new(file), &info, &settings(true))
                .unwrap();
            assert!(matches!(action, Action::Copy { reason } if reason == "other game file"));
        }
    }
}
