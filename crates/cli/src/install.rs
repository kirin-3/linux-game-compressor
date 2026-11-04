//! `install` and `uninstall`: put the single binary on the user's PATH and
//! add an app menu entry with an icon, following the XDG base directories.

use std::{
    io,
    path::{Path, PathBuf},
};

const NAME: &str = "linux-game-compressor";
const DESKTOP: &str = include_str!("../../../packaging/linux-game-compressor.desktop");
const ICON: &[u8] = include_bytes!("../../gui/assets/icon-256.png");

pub(crate) const LICENSES: &str = concat!(
    "Linux Game Compressor is licensed under the GNU General Public License v3.0.\n\n",
    include_str!("../../../LICENSE"),
    "\n\n",
    include_str!("../../../THIRD_PARTY.md"),
);

struct Paths {
    binary: PathBuf,
    desktop: PathBuf,
    icon: PathBuf,
}

fn paths() -> io::Result<Paths> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home.join(".local/share"), PathBuf::from);
    Ok(Paths {
        binary: home.join(".local/bin").join(NAME),
        desktop: data.join("applications").join(format!("{NAME}.desktop")),
        icon: data
            .join("icons/hicolor/256x256/apps")
            .join(format!("{NAME}.png")),
    })
}

/// Writes `bytes` to `path` through a temporary file, so a running copy of
/// the binary is replaced rather than overwritten in place.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let temp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::write(temp.path(), bytes)?;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(mode))?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn desktop_entry(binary: &Path) -> String {
    let exec = binary
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    DESKTOP
        .lines()
        .map(|line| {
            if line.starts_with("Exec=") {
                format!("Exec=\"{exec}\"")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

pub(crate) fn install() -> io::Result<()> {
    let paths = paths()?;
    let current = std::env::current_exe()?;
    let same = match (current.canonicalize(), paths.binary.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        write_atomic(&paths.binary, &std::fs::read(&current)?, 0o755)?;
    }
    write_atomic(&paths.icon, ICON, 0o644)?;
    write_atomic(
        &paths.desktop,
        desktop_entry(&paths.binary).as_bytes(),
        0o644,
    )?;
    println!("Installed {}", paths.binary.display());
    println!("Added Linux Game Compressor to the app menu.");
    let dir = paths.binary.parent().unwrap_or(Path::new(""));
    let on_path = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|entry| entry == dir));
    if !on_path {
        println!(
            "Add {} to your PATH to run `{NAME}` from a terminal.",
            dir.display()
        );
    }
    Ok(())
}

pub(crate) fn uninstall() -> io::Result<()> {
    let paths = paths()?;
    for path in [&paths.desktop, &paths.icon, &paths.binary] {
        match std::fs::remove_file(path) {
            Ok(()) => println!("Removed {}", path.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    println!("Preferences in ~/.config/compressor were kept.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_runs_the_installed_binary() {
        let entry = desktop_entry(Path::new("/home/a b/.local/bin/linux-game-compressor"));
        assert!(entry.contains("Exec=\"/home/a b/.local/bin/linux-game-compressor\"\n"));
        assert!(entry.contains("Icon=linux-game-compressor\n"));
        assert_eq!(entry.matches("Exec=").count(), 1);
    }
}
