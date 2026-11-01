use crate::{
    registry::{EngineAdapter, TransformerRegistry},
    Action, EngineInfo, Entry, Plan, ResolvedSettings, StoredPath, ToolSpec,
};
use rayon::prelude::*;
use std::{
    fs::{self, File},
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

const PLAN_VERSION: u64 = 3;

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
    let metadata = fs::symlink_metadata(path)?;
    let mut hasher = blake3::Hasher::new();
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(path)?;
        hasher.update(link.as_os_str().as_bytes());
    } else if metadata.is_file() {
        let mut file = File::open(path)?;
        let mut buffer = [0u8; 65536];
        loop {
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
    Ok((metadata.len(), hasher.finalize().to_hex().to_string()))
}

pub fn build_plan(
    root: &Path,
    adapter: &dyn EngineAdapter,
    mut engine: EngineInfo,
    settings: ResolvedSettings,
    transformers: &TransformerRegistry,
) -> io::Result<Plan> {
    let root = root.canonicalize()?;
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
            let (size, hash) = hash_input(path)?;
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
        plan_version: 3,
        input_root: StoredPath(root),
        engine,
        settings,
        tools,
        entries,
    })
}

/// Reject plans if their input tree or any recorded content has changed.
pub fn validate_plan(plan: &Plan, root: &Path) -> io::Result<()> {
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
    let mut actual: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_dir() => None,
            Ok(entry) => Some(Ok(entry.into_path())),
            Err(err) => Some(Err(io::Error::other(err))),
        })
        .collect::<io::Result<_>>()?;
    actual.sort();
    let mut expected: Vec<PathBuf> = plan
        .entries
        .iter()
        .filter(|entry| entry.member.is_none())
        .map(|entry| root.join(&entry.path.0))
        .collect();
    expected.sort();
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plan input file set changed",
        ));
    }
    plan.entries
        .par_iter()
        .filter(|entry| entry.member.is_none())
        .try_for_each(|entry| {
            let (size, hash) = hash_input(&root.join(&entry.path.0))?;
            if size != entry.size || hash != entry.hash {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("stale plan: {}", entry.path.0.display()),
                ))
            } else {
                Ok(())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_test_fixtures::Fixture;
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
