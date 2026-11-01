use crate::{
    registry::{EngineAdapter, TransformerRegistry},
    Action, EngineInfo, Entry, Plan, Profile, StoredPath, ToolSpec,
};
use rayon::prelude::*;
use std::{
    fs::{self, File},
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

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
    engine: EngineInfo,
    profile: Profile,
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
    let mut entries: Vec<Entry> = paths
        .par_iter()
        .map(|path| {
            let relative = path.strip_prefix(&root).map_err(io::Error::other)?;
            let (size, hash) = hash_input(path)?;
            let action = adapter.classify(&root, relative, &engine, &profile)?;
            Ok(Entry {
                path: StoredPath(relative.to_owned()),
                size,
                hash,
                action,
            })
        })
        .collect::<io::Result<_>>()?;
    entries.sort_by(|a, b| a.path.0.cmp(&b.path.0));
    let mut tools: Vec<ToolSpec> = Vec::new();
    for entry in &entries {
        if let Action::Convert { kind, .. } = &entry.action {
            let transformer = transformers.get(kind).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no transformer registered for planned media",
                )
            })?;
            for tool in transformer.required_tools() {
                if !tools.contains(&tool) {
                    tools.push(tool);
                }
            }
        }
    }
    Ok(Plan {
        plan_version: 1,
        input_root: StoredPath(root),
        engine,
        profile,
        tools,
        entries,
    })
}

/// Reject plans if their input tree or any recorded content has changed.
pub fn validate_plan(plan: &Plan, root: &Path) -> io::Result<()> {
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
        .map(|entry| root.join(&entry.path.0))
        .collect();
    expected.sort();
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plan input file set changed",
        ));
    }
    plan.entries.par_iter().try_for_each(|entry| {
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
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            unreachable!()
        }
        fn classify(&self, _: &Path, _: &Path, _: &EngineInfo, _: &Profile) -> io::Result<Action> {
            Ok(Action::Copy {
                reason: "fixture".into(),
            })
        }
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
        let profile = Profile {
            name: "test".into(),
            quality: 82,
            effort: 4,
            sharp_yuv: false,
            reencode_webp: false,
        };
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
        let profile = Profile {
            name: "test".into(),
            quality: 82,
            effort: 4,
            sharp_yuv: false,
            reencode_webp: false,
        };
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
