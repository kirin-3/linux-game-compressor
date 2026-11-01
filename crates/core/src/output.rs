use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

/// Resolve existing path components (including symlinks), even if the final path is new.
fn resolve_output(path: &Path) -> io::Result<PathBuf> {
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
                match fs::symlink_metadata(&resolved) {
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

/// Check output without creating or modifying anything.
pub fn check_output(input: &Path, output: &Path) -> io::Result<PathBuf> {
    let input = input.canonicalize()?;
    let output = resolve_output(output)?;
    if output.starts_with(&input) || input.starts_with(&output) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input and output must be separate directories",
        ));
    }
    if output.exists() {
        if !output.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output is not a directory",
            ));
        }
        // plan.json alone marks a job that stopped before writing its manifest.
        if fs::read_dir(&output)?.next().transpose()?.is_some()
            && !output.join(".compressor/manifest.json").is_file()
            && !output.join(".compressor/plan.json").is_file()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "non-empty output has no compressor manifest",
            ));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn rejects_overlapping_or_unrelated_nonempty_directories() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("game");
        fs::create_dir(&input)?;
        assert!(check_output(&input, &input).is_err());
        assert!(check_output(&input, &input.join("new/out")).is_err());
        assert!(check_output(&input, dir.path()).is_err());
        let alias = dir.path().join("alias");
        symlink(&input, &alias)?;
        assert!(check_output(&input, &alias.join("out")).is_err());
        let output = dir.path().join("out");
        fs::create_dir(&output)?;
        fs::write(output.join("existing"), b"keep")?;
        assert!(check_output(&input, &output).is_err());
        assert_eq!(fs::read(output.join("existing"))?, b"keep");
        fs::create_dir(output.join(".compressor"))?;
        fs::write(output.join(".compressor/plan.json"), b"{}")?;
        assert!(check_output(&input, &output).is_ok());
        fs::remove_file(output.join(".compressor/plan.json"))?;
        fs::write(output.join(".compressor/manifest.json"), b"{}")?;
        assert!(check_output(&input, &output).is_ok());
        assert!(check_output(&input, &dir.path().join("new-output")).is_ok());
        Ok(())
    }
}
