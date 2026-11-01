use filetime::FileTime;
use std::{fs, io, os::unix::fs::symlink, path::Path};

#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    Copied,
    Skipped(String),
}

/// Preserve mode and mtime after a copy or a successful conversion.
pub fn preserve_metadata(source: &Path, dest: &Path) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    fs::set_permissions(dest, metadata.permissions())?;
    filetime::set_file_times(
        dest,
        FileTime::from_last_access_time(&metadata),
        FileTime::from_last_modification_time(&metadata),
    )
}

/// Stage an entry on the output filesystem before atomically installing it.
pub fn stage_copy_entry(
    root: &Path,
    output: &Path,
    relative: &Path,
    temporary: &Path,
) -> io::Result<CopyOutcome> {
    let source = root.join(relative);
    let destination = output.join(relative);
    let meta = fs::symlink_metadata(&source)?;
    if meta.file_type().is_symlink() {
        let target = match source.canonicalize() {
            Ok(target) if target.starts_with(root) => target,
            _ => {
                return Ok(CopyOutcome::Skipped(
                    "symlink points outside game or cannot be resolved".into(),
                ))
            }
        };
        let final_parent = destination
            .parent()
            .ok_or_else(|| io::Error::other("missing parent"))?;
        let target = output.join(target.strip_prefix(root).map_err(io::Error::other)?);
        let relative_target = pathdiff::diff_paths(target, final_parent)
            .ok_or_else(|| io::Error::other("cannot make symlink relative"))?;
        symlink(relative_target, temporary)?;
        filetime::set_symlink_file_times(
            temporary,
            FileTime::from_last_access_time(&meta),
            FileTime::from_last_modification_time(&meta),
        )?;
    } else if meta.is_file() {
        reflink_copy::reflink_or_copy(&source, temporary)?;
        preserve_metadata(&source, temporary)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported file type",
        ));
    }
    Ok(CopyOutcome::Copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_test_fixtures::Fixture;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn copies_fixture_paths_metadata_and_safe_links() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.root().canonicalize()?;
        let dir = tempfile::tempdir()?;
        let out = dir.path().join("out");
        fs::create_dir(&out)?;
        for e in walkdir::WalkDir::new(&root).follow_links(false) {
            let e = e.map_err(io::Error::other)?;
            if e.file_type().is_dir() {
                continue;
            }
            let relative = e.path().strip_prefix(&root).unwrap();
            let dest = out.join(relative);
            fs::create_dir_all(dest.parent().unwrap())?;
            let staged = out.join(".staged");
            let result = stage_copy_entry(&root, &out, relative, &staged)?;
            if result == CopyOutcome::Copied {
                fs::rename(&staged, &dest)?;
            }
            if relative == Path::new("game/outside-link") {
                assert!(matches!(result, CopyOutcome::Skipped(_)));
                assert!(!out.join(relative).exists());
                continue;
            }
            assert_eq!(result, CopyOutcome::Copied);
            let source_meta = fs::symlink_metadata(e.path())?;
            let dest_meta = fs::symlink_metadata(out.join(relative))?;
            assert_eq!(
                FileTime::from_last_modification_time(&source_meta),
                FileTime::from_last_modification_time(&dest_meta)
            );
            if source_meta.is_file() {
                assert_eq!(
                    source_meta.permissions().mode(),
                    dest_meta.permissions().mode()
                );
                assert_eq!(fs::read(e.path())?, fs::read(out.join(relative))?);
            }
        }
        assert_eq!(
            fs::read(out.join("game/inside-link"))?,
            fs::read(root.join("game/images/opaque.png"))?
        );
        assert!(out.join("game/images/café.jpg").is_file());
        assert!(out.join("game/images/BG.PNG").is_file());
        assert!(out.join("game/images/bg.png").is_file());
        assert_ne!(
            fs::metadata(out.join("Game.sh"))?.permissions().mode() & 0o111,
            0
        );
        Ok(())
    }
}
