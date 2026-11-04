use filetime::FileTime;
use std::{
    fs,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::fs::symlink},
    path::Path,
};
use tokio_util::sync::CancellationToken;

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
    stage_copy_entry_cancellable(root, output, relative, temporary, &CancellationToken::new())
}

/// Like [`stage_copy_entry`], but a large copy stops within one chunk of
/// `cancel` being cancelled, returning `Interrupted` and removing `temporary`.
pub fn stage_copy_entry_cancellable(
    root: &Path,
    output: &Path,
    relative: &Path,
    temporary: &Path,
    cancel: &CancellationToken,
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
        if let Err(error) = copy_file(&source, temporary, cancel) {
            let _ = fs::remove_file(temporary);
            return Err(error);
        }
        preserve_metadata(&source, temporary)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported file type",
        ));
    }
    Ok(CopyOutcome::Copied)
}

const CHUNK: usize = 16 * 1024 * 1024;

/// Clones the file when the filesystem supports it, otherwise copies it in
/// chunks (in the kernel where possible), checking `cancel` between chunks.
fn copy_file(source: &Path, destination: &Path, cancel: &CancellationToken) -> io::Result<()> {
    if reflink_copy::reflink(source, destination).is_ok() {
        return Ok(());
    }
    let mut from = fs::File::open(source)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination)?;
    let mut in_kernel = true;
    let mut buffer = Vec::new();
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let copied = if in_kernel {
            // SAFETY: both descriptors are open for the duration of the call,
            // and null offsets use and advance the files' own positions.
            let n = unsafe {
                libc::copy_file_range(
                    from.as_raw_fd(),
                    std::ptr::null_mut(),
                    to.as_raw_fd(),
                    std::ptr::null_mut(),
                    CHUNK,
                    0,
                )
            };
            if n < 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(
                        libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM,
                    ) => {
                        in_kernel = false;
                        continue;
                    }
                    Some(libc::EINTR) => continue,
                    _ => return Err(error),
                }
            }
            n as usize
        } else {
            buffer.resize(CHUNK, 0);
            let n = from.read(&mut buffer)?;
            to.write_all(&buffer[..n])?;
            n
        };
        if copied == 0 {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_test_fixtures::Fixture;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn cancelled_copy_stops_and_removes_the_stage() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("big"), vec![7u8; CHUNK + 1])?;
        let stage = dir.path().join("stage");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result =
            stage_copy_entry_cancellable(dir.path(), dir.path(), Path::new("big"), &stage, &cancel);
        // A filesystem that clones files finishes instantly instead.
        match result {
            Ok(CopyOutcome::Copied) => assert_eq!(fs::read(&stage)?.len(), CHUNK + 1),
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Interrupted);
                assert!(!stage.exists());
            }
            Ok(other) => panic!("unexpected {other:?}"),
        }
        let copy = dir.path().join("copy");
        copy_file(&dir.path().join("big"), &copy, &CancellationToken::new())?;
        assert_eq!(fs::read(&copy)?, fs::read(dir.path().join("big"))?);
        Ok(())
    }
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
