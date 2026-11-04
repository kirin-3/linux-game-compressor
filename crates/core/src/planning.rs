use crate::{
    registry::{EngineAdapter, TransformerRegistry},
    Action, EngineInfo, Entry, Plan, ResolvedSettings, StoredPath, ToolSpec,
};
use rayon::prelude::*;
use std::{
    fs::{self, File},
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

const PLAN_VERSION: u64 = 5;

fn versioned_json(path: &Path, manifest: bool) -> io::Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let plan = if manifest { &value["plan"] } else { &value };
    if plan.get("plan_version").and_then(serde_json::Value::as_u64) != Some(PLAN_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "written by an incompatible version; re-run plan",
        ));
    }
    Ok(value)
}

pub fn read_plan(path: &Path) -> io::Result<Plan> {
    serde_json::from_value(versioned_json(path, false)?).map_err(io::Error::from)
}

pub fn read_manifest(path: &Path) -> io::Result<crate::Manifest> {
    serde_json::from_value(versioned_json(path, true)?).map_err(io::Error::from)
}

/// Hash the bytes of a file or the link text itself, never a symlink's target.
pub fn hash_input(path: &Path) -> io::Result<(u64, String)> {
    hash_input_inner(path, None)
}

#[derive(Debug)]
pub(crate) struct EnvelopeUnwrapError(io::Error);
impl std::fmt::Display for EnvelopeUnwrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for EnvelopeUnwrapError {}

/// Materialize the bytes seen by a transformer, without writing to the game.
pub fn stage_entry_source(
    root: &Path,
    plan: &Plan,
    entry: &Entry,
    packs: &crate::registry::PackRegistry,
    envelopes: &crate::registry::EnvelopeRegistry,
    dir: &Path,
) -> io::Result<PathBuf> {
    let input = root.join(&entry.path.0);
    if entry.member.is_none()
        && !matches!(
            &entry.action,
            Action::Convert {
                envelope: Some(_),
                ..
            }
        )
    {
        let file = tempfile::Builder::new()
            .prefix("source-")
            .tempfile_in(dir)?;
        fs::copy(&input, file.path())?;
        crate::copy::preserve_metadata(&input, file.path())?;
        let (_, path) = file.keep().map_err(|e| e.error)?;
        return Ok(path);
    }
    let bytes = if let Some(name) = &entry.member {
        let parent = plan
            .entries
            .iter()
            .find(|e| e.path == entry.path && e.member.is_none());
        let Some(Action::Rebuild { format }) = parent.map(|e| &e.action) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "member has no rebuild entry",
            ));
        };
        let handler = packs.get(format).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing pack format: {format}"),
            )
        })?;
        let member = handler
            .read_directory(&input)?
            .into_iter()
            .find(|m| &m.name == name)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "member missing from pack")
            })?;
        let bytes = handler.read_member(&input, &member)?;
        if !entry.hash.is_empty()
            && (bytes.len() as u64 != entry.size
                || blake3::hash(&bytes).to_hex().as_str() != entry.hash)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stale pack member",
            ));
        }
        bytes
    } else {
        fs::read(&input)?
    };
    stage_bytes(bytes, &entry.action, envelopes, dir)
}

/// Write an entry's raw bytes to `dir`, unwrapping its envelope when it has one.
pub(crate) fn stage_bytes(
    bytes: Vec<u8>,
    action: &Action,
    envelopes: &crate::registry::EnvelopeRegistry,
    dir: &Path,
) -> io::Result<PathBuf> {
    let bytes = if let Action::Convert {
        envelope: Some(envelope),
        ..
    } = action
    {
        let handler = envelopes.get(&envelope.format).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing envelope format: {}", envelope.format),
            )
        })?;
        handler
            .unwrap(&bytes, &envelope.params)
            .map_err(|error| io::Error::new(error.kind(), EnvelopeUnwrapError(error)))?
    } else {
        bytes
    };
    let mut file = tempfile::Builder::new()
        .prefix("source-")
        .tempfile_in(dir)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    let (_, path) = file.keep().map_err(|e| e.error)?;
    Ok(path)
}

fn check_cancel(cancel: Option<&CancellationToken>) -> io::Result<()> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    Ok(())
}

fn hash_input_inner(path: &Path, cancel: Option<&CancellationToken>) -> io::Result<(u64, String)> {
    check_cancel(cancel)?;
    let metadata = fs::symlink_metadata(path)?;
    let mut hasher = blake3::Hasher::new();
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(path)?;
        hasher.update(link.as_os_str().as_bytes());
    } else if metadata.is_file() {
        let mut file = File::open(path)?;
        let mut buffer = [0u8; 65536];
        loop {
            check_cancel(cancel)?;
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file or symlink",
        ));
    }
    check_cancel(cancel)?;
    Ok((metadata.len(), hasher.finalize().to_hex().to_string()))
}

pub fn build_plan(
    root: &Path,
    adapter: &dyn EngineAdapter,
    engine: EngineInfo,
    settings: ResolvedSettings,
    transformers: &TransformerRegistry,
) -> io::Result<Plan> {
    plan_inner(root, adapter, engine, settings, transformers, true)
}

/// Build a non-executable preview without hashing loose input files.
/// Pack adapters may still hash members while expanding their directories.
pub fn preview_plan(
    root: &Path,
    adapter: &dyn EngineAdapter,
    engine: EngineInfo,
    settings: ResolvedSettings,
    transformers: &TransformerRegistry,
) -> io::Result<Plan> {
    plan_inner(root, adapter, engine, settings, transformers, false)
}

fn plan_inner(
    root: &Path,
    adapter: &dyn EngineAdapter,
    mut engine: EngineInfo,
    settings: ResolvedSettings,
    transformers: &TransformerRegistry,
    hash_files: bool,
) -> io::Result<Plan> {
    let root = root.canonicalize()?;
    let archives = adapter.archives(&root, &engine, &settings)?;
    let paths: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| match e {
            Ok(e) if e.file_type().is_dir() => None,
            Ok(e) => Some(Ok(e.into_path())),
            Err(e) => Some(Err(io::Error::other(e))),
        })
        .collect::<io::Result<_>>()?;
    let planned: Vec<Vec<Entry>> = paths
        .par_iter()
        .map(|path| {
            let relative = path.strip_prefix(&root).map_err(io::Error::other)?;
            let (size, hash) = if hash_files {
                hash_input(path)?
            } else {
                let metadata = fs::symlink_metadata(path)?;
                if !metadata.is_file() && !metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "not a regular file or symlink",
                    ));
                }
                (metadata.len(), String::new())
            };
            let mut action = adapter.classify(&root, relative, &engine, &settings)?;
            let members = adapter.expand(&root, relative, &engine, &settings)?;
            let mut expanded = Vec::new();
            if let Some(members) = members {
                if members
                    .iter()
                    .any(|member| matches!(member.action, Action::Convert { .. }))
                {
                    if !matches!(action, Action::Rebuild { .. }) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "expandable pack has no rebuild format",
                        ));
                    }
                    expanded.extend(members.into_iter().map(|member| Entry {
                        path: StoredPath(relative.to_owned()),
                        member: Some(member.member),
                        size: member.size,
                        hash: member.hash,
                        action: member.action,
                    }));
                } else {
                    action = Action::Copy {
                        reason: "no member converted".into(),
                    };
                }
            }
            expanded.insert(
                0,
                Entry {
                    path: StoredPath(relative.to_owned()),
                    member: None,
                    size,
                    hash,
                    action,
                },
            );
            Ok(expanded)
        })
        .collect::<io::Result<_>>()?;
    let mut entries: Vec<Entry> = planned.into_iter().flatten().collect();
    for archive in archives {
        if archive.members.is_empty() {
            continue;
        }
        let mut members = Vec::new();
        let mut metadata = Vec::new();
        for path in &archive.members {
            let entry = entries
                .iter()
                .find(|e| e.member.is_none() && e.path.0 == *path)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive member missing from input",
                    )
                })?;
            if !matches!(&entry.action, Action::Archive { archive: target } if target.0 == archive.path)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "archive members disagree with classification",
                ));
            }
            let name = path
                .strip_prefix(archive.path.parent().unwrap_or(Path::new("")))
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive member outside archive directory",
                    )
                })?
                .to_str()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive member name is not UTF-8",
                    )
                })?
                .to_owned();
            members.push(name.clone());
            metadata.push((name, entry.size, entry.hash.clone()));
        }
        let classified = entries.iter().filter(|e| matches!(&e.action, Action::Archive { archive: target } if target.0 == archive.path)).count();
        if classified != archive.members.len()
            || metadata
                .iter()
                .map(|m| &m.0)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != metadata.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive members disagree with classification",
            ));
        }
        metadata.sort();
        let size = metadata
            .iter()
            .try_fold(0u64, |sum, (_, size, _)| sum.checked_add(*size))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "archive size overflow"))?;
        let hash = blake3::hash(&serde_json::to_vec(&metadata)?)
            .to_hex()
            .to_string();
        entries.push(Entry {
            path: StoredPath(archive.path),
            member: None,
            size,
            hash,
            action: Action::Assemble {
                format: archive.format,
                params: archive.params,
                members,
            },
        });
    }
    if entries.iter().any(|entry| matches!(&entry.action, Action::Archive { archive } if !entries.iter().any(|candidate| candidate.path == *archive && matches!(candidate.action, Action::Assemble { .. })))) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "archive classification has no archive plan"));
    }
    entries.sort_by(|a, b| a.path.0.cmp(&b.path.0).then(a.member.cmp(&b.member)));
    let notes = adapter.plan_notes(&root, &engine, &settings, &entries);
    engine.notes.extend(notes);
    let mut tools: Vec<ToolSpec> = Vec::new();
    for entry in &entries {
        if let Action::Convert {
            encoder, settings, ..
        } = &entry.action
        {
            let transformer = transformers.get(encoder).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no transformer registered for planned media",
                )
            })?;
            for tool in transformer.required_tools_for(settings) {
                if let Some(existing) = tools.iter_mut().find(|t| t.name == tool.name) {
                    for feature in tool.features {
                        if !existing.features.contains(&feature) {
                            existing.features.push(feature);
                        }
                    }
                } else {
                    tools.push(tool);
                }
            }
        }
    }
    Ok(Plan {
        plan_version: 5,
        input_root: StoredPath(root),
        engine,
        settings,
        tools,
        entries,
    })
}

/// Reject plans if their input tree or any recorded content has changed.
pub fn validate_plan(plan: &Plan, root: &Path) -> io::Result<()> {
    validate_plan_inner(plan, root, None)
}

/// Validate a plan without writing output, stopping promptly when cancelled.
pub fn validate_plan_with_cancel(
    plan: &Plan,
    root: &Path,
    cancel: &CancellationToken,
) -> io::Result<()> {
    validate_plan_inner(plan, root, Some(cancel))
}

fn validate_plan_inner(
    plan: &Plan,
    root: &Path,
    cancel: Option<&CancellationToken>,
) -> io::Result<()> {
    check_cancel(cancel)?;
    if u64::from(plan.plan_version) != PLAN_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "written by an incompatible version; re-run plan",
        ));
    }
    let root = root.canonicalize()?;
    if root != plan.input_root.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plan input root differs",
        ));
    }
    for entry in &plan.entries {
        check_cancel(cancel)?;
        if entry.path.0.as_os_str().is_empty()
            || !entry
                .path
                .0
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsafe plan path",
            ));
        }
        match &entry.action {
            Action::Assemble { members, .. } => {
                if entry.member.is_some() || fs::symlink_metadata(root.join(&entry.path.0)).is_ok()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive path already exists in input",
                    ));
                }
                let mut expected = members.clone();
                expected.sort();
                let mut actual: Vec<String> = plan
                    .entries
                    .iter()
                    .filter_map(|member| {
                        if let Action::Archive { archive } = &member.action {
                            if archive == &entry.path && member.member.is_none() {
                                return member
                                    .path
                                    .0
                                    .strip_prefix(entry.path.0.parent().unwrap_or(Path::new("")))
                                    .ok()?
                                    .to_str()
                                    .map(str::to_owned);
                            }
                        }
                        None
                    })
                    .collect();
                actual.sort();
                if expected.is_empty()
                    || expected != actual
                    || expected.windows(2).any(|pair| pair[0] == pair[1])
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive members disagree with plan",
                    ));
                }
            }
            Action::Archive { archive }
                if entry.member.is_some()
                    || plan
                        .entries
                        .iter()
                        .filter(|candidate| {
                            candidate.path == *archive
                                && candidate.member.is_none()
                                && matches!(candidate.action, Action::Assemble { .. })
                        })
                        .count()
                        != 1 =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "archive member has no unique assemble entry",
                ));
            }
            _ => {}
        }
    }
    let mut actual: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| {
            if let Err(err) = check_cancel(cancel) {
                return Some(Err(err));
            }
            match entry {
                Ok(entry) if entry.file_type().is_dir() => None,
                Ok(entry) => Some(Ok(entry.into_path())),
                Err(err) => Some(Err(io::Error::other(err))),
            }
        })
        .collect::<io::Result<_>>()?;
    actual.sort();
    let mut expected: Vec<PathBuf> = plan
        .entries
        .iter()
        .filter(|entry| entry.member.is_none() && !matches!(entry.action, Action::Assemble { .. }))
        .map(|entry| root.join(&entry.path.0))
        .collect();
    expected.sort();
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plan input file set changed",
        ));
    }
    let result = plan
        .entries
        .par_iter()
        .filter(|entry| entry.member.is_none() && !matches!(entry.action, Action::Assemble { .. }))
        .try_for_each(|entry| {
            check_cancel(cancel)?;
            let (size, hash) = hash_input_inner(&root.join(&entry.path.0), cancel)?;
            if size != entry.size || hash != entry.hash {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("stale plan: {}", entry.path.0.display()),
                ))
            } else {
                Ok(())
            }
        });
    check_cancel(cancel)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_test_fixtures::Fixture;
    struct ArchiveAdapter(bool);
    impl EngineAdapter for ArchiveAdapter {
        fn id(&self) -> &'static str {
            "archive"
        }
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            unreachable!()
        }
        fn classify(
            &self,
            _: &Path,
            path: &Path,
            _: &EngineInfo,
            _: &ResolvedSettings,
        ) -> io::Result<Action> {
            if ["0", "1", "2"].iter().any(|n| path == Path::new(n)) {
                Ok(Action::Archive {
                    archive: StoredPath("data.arc".into()),
                })
            } else {
                Ok(Action::Copy {
                    reason: "test".into(),
                })
            }
        }
        fn archives(
            &self,
            _: &Path,
            _: &EngineInfo,
            _: &ResolvedSettings,
        ) -> io::Result<Vec<crate::registry::ArchivePlan>> {
            let mut members = vec![PathBuf::from("0"), PathBuf::from("1"), PathBuf::from("2")];
            if self.0 {
                members.pop();
            }
            Ok(vec![crate::registry::ArchivePlan {
                path: "data.arc".into(),
                format: "fake".into(),
                params: serde_json::json!({}),
                members,
            }])
        }
    }
    #[test]
    fn plans_archives_and_rejects_mismatched_classification() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        for n in 0..5 {
            fs::write(temp.path().join(n.to_string()), b"test")?;
        }
        let engine = EngineInfo {
            name: "fake".into(),
            version: None,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        };
        let build = |adapter: &ArchiveAdapter| {
            build_plan(
                temp.path(),
                adapter,
                engine.clone(),
                ResolvedSettings::default(),
                &TransformerRegistry::default(),
            )
        };
        let plan = build(&ArchiveAdapter(false))?;
        assert_eq!(plan.entries.len(), 6);
        let archive = plan
            .entries
            .iter()
            .find(|e| e.path.0 == Path::new("data.arc"))
            .unwrap();
        assert_eq!(archive.size, 12);
        assert!(
            matches!(&archive.action, Action::Assemble { members, .. } if members == &["0", "1", "2"])
        );
        validate_plan(&plan, temp.path())?;
        let mut invalid = plan.clone();
        invalid
            .entries
            .iter_mut()
            .find(|e| matches!(e.action, Action::Assemble { .. }))
            .unwrap()
            .path = StoredPath("3".into());
        assert!(validate_plan(&invalid, temp.path()).is_err());
        let mut invalid = plan.clone();
        invalid
            .entries
            .retain(|e| !matches!(e.action, Action::Assemble { .. }));
        assert!(validate_plan(&invalid, temp.path()).is_err());
        let mut invalid = plan.clone();
        if let Action::Assemble { members, .. } = &mut invalid
            .entries
            .iter_mut()
            .find(|e| matches!(e.action, Action::Assemble { .. }))
            .unwrap()
            .action
        {
            members.pop();
        }
        assert!(validate_plan(&invalid, temp.path()).is_err());
        assert!(!temp.path().join("data.arc").exists());
        assert_eq!(
            build(&ArchiveAdapter(true)).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
    struct Fake;
    impl EngineAdapter for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            unreachable!()
        }
        fn classify(
            &self,
            _: &Path,
            _: &Path,
            _: &EngineInfo,
            _: &ResolvedSettings,
        ) -> io::Result<Action> {
            Ok(Action::Copy {
                reason: "fixture".into(),
            })
        }
    }
    struct PackAdapter(bool);
    impl EngineAdapter for PackAdapter {
        fn id(&self) -> &'static str {
            "pack"
        }
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            unreachable!()
        }
        fn classify(
            &self,
            _: &Path,
            _: &Path,
            _: &EngineInfo,
            _: &ResolvedSettings,
        ) -> io::Result<Action> {
            Ok(Action::Rebuild {
                format: "fake-pack".into(),
            })
        }
        fn expand(
            &self,
            _: &Path,
            _: &Path,
            _: &EngineInfo,
            settings: &ResolvedSettings,
        ) -> io::Result<Option<Vec<crate::registry::MemberPlan>>> {
            let action = if self.0 {
                Action::Convert {
                    kind: crate::MediaKind::Image,
                    target: "webp".into(),
                    encoder: "fake".into(),
                    envelope: None,
                    settings: crate::ConvertSettings::Image(settings.image()),
                }
            } else {
                Action::Copy {
                    reason: "test".into(),
                }
            };
            Ok(Some(vec![
                crate::registry::MemberPlan {
                    member: "a".into(),
                    size: 1,
                    hash: "a".into(),
                    action,
                },
                crate::registry::MemberPlan {
                    member: "b".into(),
                    size: 1,
                    hash: "b".into(),
                    action: Action::Copy {
                        reason: "test".into(),
                    },
                },
            ]))
        }
    }
    struct FakeTransformer;
    impl crate::registry::Transformer for FakeTransformer {
        fn kind(&self) -> crate::MediaKind {
            crate::MediaKind::Image
        }
        fn required_tools(&self) -> Vec<ToolSpec> {
            vec![]
        }
        fn transform<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a crate::ConvertSettings,
            _: &'a tokio_util::sync::CancellationToken,
        ) -> crate::registry::Operation<'a, Option<String>> {
            Box::pin(async { unreachable!() })
        }
        fn validate<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a crate::ConvertSettings,
            _: &'a tokio_util::sync::CancellationToken,
        ) -> crate::registry::Operation<'a, ()> {
            Box::pin(async { unreachable!() })
        }
    }
    #[test]
    fn expands_only_packs_with_a_conversion() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("game.pck"), b"pack")?;
        let engine = EngineInfo {
            name: "fake".into(),
            version: None,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        };
        let mut registry = TransformerRegistry::default();
        registry.register("fake", std::sync::Arc::new(FakeTransformer));
        let plan = build_plan(
            temp.path(),
            &PackAdapter(true),
            engine.clone(),
            ResolvedSettings::default(),
            &registry,
        )?;
        assert_eq!(plan.entries.len(), 3);
        assert!(matches!(plan.entries[0].action, Action::Rebuild { .. }));
        assert_eq!(
            plan.entries.iter().filter(|e| e.member.is_some()).count(),
            2
        );
        validate_plan(&plan, temp.path())?;
        let plan = build_plan(
            temp.path(),
            &PackAdapter(false),
            engine,
            ResolvedSettings::default(),
            &registry,
        )?;
        assert_eq!(plan.entries.len(), 1);
        assert!(
            matches!(&plan.entries[0].action, Action::Copy { reason } if reason == "no member converted")
        );
        Ok(())
    }
    #[test]
    fn saved_plan_rejects_changed_content() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let engine = EngineInfo {
            name: "fake".into(),
            version: None,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        };
        let profile = ResolvedSettings::default();
        let plan = build_plan(
            fixture.root(),
            &Fake,
            engine,
            profile,
            &TransformerRegistry::default(),
        )?;
        let saved = serde_json::to_vec(&plan)?;
        let plan: Plan = serde_json::from_slice(&saved)?;
        validate_plan(&plan, fixture.root())?;
        let path = fixture.root().join("game/script.rpyc");
        let mut bytes = fs::read(&path)?;
        bytes[0] ^= 1;
        fs::write(path, bytes)?;
        assert_eq!(
            validate_plan(&plan, fixture.root()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
    #[test]
    fn plan_accounts_for_files_and_does_not_follow_links() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let engine = EngineInfo {
            name: "fake".into(),
            version: None,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        };
        let profile = ResolvedSettings::default();
        let plan = build_plan(
            fixture.root(),
            &Fake,
            engine,
            profile,
            &TransformerRegistry::default(),
        )?;
        let expected = WalkDir::new(fixture.root())
            .follow_links(false)
            .into_iter()
            .map(Result::unwrap)
            .filter(|e| !e.file_type().is_dir())
            .count();
        assert_eq!(plan.entries.len(), expected);
        assert_eq!(
            plan.entries
                .iter()
                .filter(|e| e.path.0 == Path::new("game/outside-link"))
                .count(),
            1
        );
        assert!(!plan
            .entries
            .iter()
            .any(|e| e.path.0 == Path::new("outside.txt")));
        Ok(())
    }
}
