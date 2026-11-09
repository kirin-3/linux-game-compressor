//! Finding Unreal projects and reading the engine version from the executable.

use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

/// A project folder whose `Content/Paks` holds IoStore containers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    /// Project folder relative to the game directory.
    pub project: PathBuf,
    /// `<project>/Content/Paks`, relative to the game directory.
    pub paks: PathBuf,
    /// The folder `Engine` beside the project, relative to the game directory.
    pub engine: PathBuf,
}

fn child_dirs(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
    }
    names.sort();
    Ok(names)
}

fn has_toc(paks: &Path) -> bool {
    std::fs::read_dir(paks).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.path().extension().is_some_and(|x| x == "utoc")
                && e.file_type().is_ok_and(|t| t.is_file())
        })
    })
}

/// Projects are the game directory's children, or one folder further down, that have
/// `Content/Paks/*.utoc` and an `Engine` folder beside them.
pub fn find_projects(root: &Path) -> io::Result<Vec<Project>> {
    let mut found = Vec::new();
    let mut parents = vec![PathBuf::new()];
    for name in child_dirs(root)? {
        if name != "Engine" {
            parents.push(PathBuf::from(name));
        }
    }
    for parent in parents {
        let dir = root.join(&parent);
        let Ok(names) = child_dirs(&dir) else {
            continue;
        };
        if !names.iter().any(|n| n == "Engine") {
            continue;
        }
        for name in names.iter().filter(|n| *n != "Engine") {
            let paks = parent.join(name).join("Content").join("Paks");
            if has_toc(&root.join(&paks)) {
                found.push(Project {
                    project: parent.join(name),
                    paks,
                    engine: parent.join("Engine"),
                });
            }
        }
    }
    Ok(found)
}

fn exes(dir: &Path, shipping_only: bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.to_ascii_lowercase().ends_with(".exe")
                && (!shipping_only || name.ends_with("-Shipping.exe"))
        })
        .collect();
    files.sort();
    files
}

/// Executables to scan: the project's own first, else the engine's shipping executable.
pub fn executables(root: &Path, project: &Project) -> Vec<PathBuf> {
    let own = exes(&root.join(&project.project).join("Binaries/Win64"), false);
    if own.is_empty() {
        exes(&root.join(&project.engine).join("Binaries/Win64"), true)
    } else {
        own
    }
}

/// Keep only the bytes at `parity`, `parity + 2`, ... that are followed by a zero byte,
/// so UTF-16LE text reads as ASCII.
fn narrow(window: &[u8], parity: usize) -> Vec<u8> {
    (parity..window.len().saturating_sub(1))
        .step_by(2)
        .map(|i| if window[i + 1] == 0 { window[i] } else { 0xFF })
        .collect()
}

fn digits(bytes: &[u8], mut at: usize) -> Option<(u32, usize)> {
    let start = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() && at - start < 4 {
        at += 1;
    }
    let value = std::str::from_utf8(&bytes[start..at]).ok()?.parse().ok()?;
    (at > start).then_some((value, at))
}

/// Find `[patch-]++UE<major>+Release-<major>.<minor>` and return `major.minor[.patch]`.
fn parse_build_string(text: &[u8]) -> Option<String> {
    const MARK: &[u8] = b"++UE";
    let mut from = 0;
    while let Some(found) = text[from..].windows(MARK.len()).position(|w| w == MARK) {
        let at = from + found;
        from = at + 1;
        let Some((_, after)) = digits(text, at + MARK.len()) else {
            continue;
        };
        let release = b"+Release-";
        if !text[after..].starts_with(release) {
            continue;
        }
        let Some((major, dot)) = digits(text, after + release.len()) else {
            continue;
        };
        if text.get(dot) != Some(&b'.') {
            continue;
        }
        let Some((minor, _)) = digits(text, dot + 1) else {
            continue;
        };
        // A patch number sits just before the marker: `5.7.4-++UE5...`.
        let mut patch = None;
        if at >= 2 && text[at - 1] == b'-' {
            let end = at - 1;
            let mut start = end;
            while start > 0 && text[start - 1].is_ascii_digit() {
                start -= 1;
            }
            let prefix = format!("{major}.{minor}.");
            if start >= prefix.len()
                && &text[start - prefix.len()..start] == prefix.as_bytes()
                && start < end
            {
                patch = std::str::from_utf8(&text[start..end])
                    .ok()
                    .map(str::to_owned);
            }
        }
        return Some(match patch {
            Some(patch) => format!("{major}.{minor}.{patch}"),
            None => format!("{major}.{minor}"),
        });
    }
    None
}

/// Scan one executable, stopping at the first build string, in UTF-8 or UTF-16.
pub fn scan_version(path: &Path) -> io::Result<Option<String>> {
    const WINDOW: usize = 8 << 20;
    const OVERLAP: usize = 512;
    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; WINDOW + OVERLAP];
    let mut carried = 0;
    loop {
        let mut filled = carried;
        while filled < buffer.len() {
            let n = file.read(&mut buffer[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        let window = &buffer[..filled];
        if let Some(version) = parse_build_string(window)
            .or_else(|| parse_build_string(&narrow(window, 0)))
            .or_else(|| parse_build_string(&narrow(window, 1)))
        {
            return Ok(Some(version));
        }
        if filled < buffer.len() {
            return Ok(None);
        }
        buffer.copy_within(filled - OVERLAP..filled, 0);
        carried = OVERLAP;
    }
}

/// The engine version from the first executable that names one.
pub fn engine_version(root: &Path, project: &Project) -> Option<String> {
    executables(root, project)
        .iter()
        .find_map(|exe| scan_version(exe).ok().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn project_one_wrapper_folder_down() {
        let temp = tempfile::tempdir().unwrap();
        touch(temp.path(), "Game/Engine/Binaries/x.txt", b"");
        touch(
            temp.path(),
            "Game/EscapeFrom/Content/Paks/EscapeFrom-Windows.utoc",
            b"",
        );
        let found = find_projects(temp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].paks, Path::new("Game/EscapeFrom/Content/Paks"));
        assert_eq!(found[0].engine, Path::new("Game/Engine"));
    }

    #[test]
    fn executable_without_shipping_suffix_and_direct_child() {
        let temp = tempfile::tempdir().unwrap();
        touch(temp.path(), "Windows/Engine/x.txt", b"");
        touch(
            temp.path(),
            "Windows/prealpha/Binaries/Win64/prealpha.exe",
            b"MZ",
        );
        touch(
            temp.path(),
            "Windows/prealpha/Content/Paks/prealpha-Windows.utoc",
            b"",
        );
        let found = find_projects(temp.path()).unwrap();
        assert_eq!(found[0].paks, Path::new("Windows/prealpha/Content/Paks"));
        assert_eq!(
            executables(temp.path(), &found[0]),
            vec![temp
                .path()
                .join("Windows/prealpha/Binaries/Win64/prealpha.exe")]
        );
        let direct = tempfile::tempdir().unwrap();
        touch(direct.path(), "Engine/x.txt", b"");
        touch(direct.path(), "Proj/Content/Paks/a.utoc", b"");
        assert_eq!(
            find_projects(direct.path()).unwrap()[0].project,
            Path::new("Proj")
        );
    }

    #[test]
    fn paks_without_engine_folder_is_not_a_project() {
        let temp = tempfile::tempdir().unwrap();
        touch(temp.path(), "Game/Content/Paks/Game.utoc", b"");
        assert!(find_projects(temp.path()).unwrap().is_empty());
        let engine_only = tempfile::tempdir().unwrap();
        touch(engine_only.path(), "Engine/Content/Paks/a.utoc", b"");
        assert!(find_projects(engine_only.path()).unwrap().is_empty());
    }

    #[test]
    fn engine_executable_is_used_when_project_has_none() {
        let temp = tempfile::tempdir().unwrap();
        touch(
            temp.path(),
            "Game/Engine/Binaries/Win64/UnrealGame-Win64-Shipping.exe",
            b"MZ",
        );
        touch(temp.path(), "Game/Engine/Binaries/Win64/Other.exe", b"MZ");
        touch(temp.path(), "Game/P/Content/Paks/a.utoc", b"");
        let project = &find_projects(temp.path()).unwrap()[0];
        let found = executables(temp.path(), project);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("UnrealGame-Win64-Shipping.exe"));
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn reads_versions_in_utf8_and_utf16() {
        let temp = tempfile::tempdir().unwrap();
        let cases: [(Vec<u8>, Option<&str>); 6] = [
            (
                b"junk 5.7.4-++UE5+Release-5.7-CL-51494982 junk".to_vec(),
                Some("5.7.4"),
            ),
            (b"++UE5+Release-5.5-CL-40574608".to_vec(), Some("5.5")),
            (utf16("5.7.4-++UE5+Release-5.7-CL-1"), Some("5.7.4")),
            (utf16("++UE5+Release-5.5-CL-1"), Some("5.5")),
            (
                b"x".iter()
                    .chain(&utf16("++UE5+Release-5.4-CL-1"))
                    .copied()
                    .collect(),
                Some("5.4"),
            ),
            (b"no build string here".to_vec(), None),
        ];
        for (index, (bytes, expected)) in cases.into_iter().enumerate() {
            let path = temp.path().join(format!("{index}.exe"));
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                scan_version(&path).unwrap().as_deref(),
                expected,
                "case {index}"
            );
        }
    }

    #[test]
    fn build_string_across_a_window_boundary_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let mut bytes = vec![b'a'; (8 << 20) - 10];
        bytes.extend(b"5.6.1-++UE5+Release-5.6-CL-1");
        let path = temp.path().join("big.exe");
        fs::write(&path, bytes).unwrap();
        assert_eq!(scan_version(&path).unwrap().as_deref(), Some("5.6.1"));
    }

    #[test]
    fn reads_the_versions_of_both_example_games() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_games");
        for (prefix, expected) in [("UNREAL_1", "5.7.4"), ("UNREAL_2", "5.5")] {
            let Some(dir) = fs::read_dir(&root).ok().and_then(|d| {
                d.flatten()
                    .find(|e| e.file_name().to_string_lossy().starts_with(prefix))
            }) else {
                continue;
            };
            let projects = find_projects(&dir.path()).unwrap();
            assert_eq!(projects.len(), 1, "{prefix}");
            assert_eq!(
                engine_version(&dir.path(), &projects[0]).as_deref(),
                Some(expected)
            );
        }
    }
}
