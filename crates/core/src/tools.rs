use crate::{Plan, StoredPath, ToolSpec, ToolVersion};
use std::{
    env,
    ffi::OsStr,
    io,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

/// Resolve and check tools before output creation; returns paths and exact versions.
pub fn check_tools(plan: &Plan) -> io::Result<Vec<ToolVersion>> {
    check_tools_with_path(
        &plan.tools,
        env::var_os("PATH").as_deref().unwrap_or(OsStr::new("")),
    )
}

fn check_tools_with_path(tools: &[ToolSpec], path: &OsStr) -> io::Result<Vec<ToolVersion>> {
    let mut versions = Vec::new();
    for tool in tools {
        let executable = env::split_paths(path)
            .map(|dir| dir.join(&tool.name))
            .find(|path| fs_executable(path))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing {} (install {})", tool.name, tool.package),
                )
            })?;
        let output = Command::new(&executable)
            .arg("-version")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                io::Error::new(e.kind(), format!("{} ({}): {e}", tool.name, tool.package))
            })?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "{} from {} is not runnable",
                tool.name, tool.package
            )));
        }
        for decoder in [false, true] {
            let selected: Vec<_> = tool
                .features
                .iter()
                .filter_map(|feature| {
                    if decoder {
                        feature.strip_prefix("decoder:")
                    } else if feature.starts_with("decoder:") {
                        None
                    } else {
                        Some(feature.as_str())
                    }
                })
                .collect();
            if selected.is_empty() {
                continue;
            }
            let kind = if decoder { "decoder" } else { "encoder" };
            let listing = Command::new(&executable)
                .args([
                    "-hide_banner",
                    if decoder { "-decoders" } else { "-encoders" },
                ])
                .stdin(Stdio::null())
                .output()?;
            if !listing.status.success() {
                return Err(io::Error::other(format!(
                    "{} from {} could not list {kind}s",
                    tool.name, tool.package
                )));
            }
            let text = String::from_utf8_lossy(&listing.stdout);
            for feature in selected {
                let found = text.lines().any(|line| {
                    let mut parts = line.split_whitespace();
                    let flags = parts.next().unwrap_or("");
                    flags.len() == 6 && parts.next() == Some(feature)
                });
                if !found {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "{} missing {kind} {} (install {})",
                            tool.name, feature, tool.package
                        ),
                    ));
                }
            }
        }
        let bytes = if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        };
        versions.push(ToolVersion {
            path: StoredPath(executable.canonicalize()?),
            version: String::from_utf8_lossy(bytes).trim().to_owned(),
        });
    }
    Ok(versions)
}
fn fs_executable(path: &Path) -> bool {
    path.is_file() && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_path_in_subprocess() -> io::Result<()> {
        if std::env::var_os("COMPRESSOR_EMPTY_PATH_TEST").is_some() {
            let plan = Plan {
                plan_version: 5,
                input_root: StoredPath("/unused".into()),
                engine: crate::EngineInfo {
                    name: "fake".into(),
                    version: None,
                    excluded_paths: vec![],
                    notes: vec![],
                    version_check_overridden: false,
                },
                settings: crate::ResolvedSettings::default(),
                tools: vec![ToolSpec {
                    name: "cwebp".into(),
                    package: "libwebp-utils".into(),
                    features: vec![],
                }],
                entries: vec![],
            };
            let err = check_tools(&plan).unwrap_err();
            println!("{err}");
            return Ok(());
        }
        let tmp = tempfile::tempdir()?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "tools::tests::empty_path_in_subprocess",
                "--nocapture",
            ])
            .env("PATH", "")
            .env("COMPRESSOR_EMPTY_PATH_TEST", "1")
            .current_dir(tmp.path())
            .output()?;
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("cwebp") && stdout.contains("libwebp-utils"));
        assert_eq!(std::fs::read_dir(tmp.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn missing_encoder_names_package_without_creating_output() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("output");
        let err = check_tools_with_path(
            &[ToolSpec {
                name: "cwebp".into(),
                package: "libwebp-utils".into(),
                features: vec![],
            }],
            OsStr::new(""),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cwebp"));
        assert!(err.to_string().contains("libwebp-utils"));
        assert!(!output.exists());
        Ok(())
    }

    #[test]
    fn checks_required_ffmpeg_encoders() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let ffmpeg = dir.path().join("ffmpeg");
        let write = |encoders: &str| -> io::Result<()> {
            std::fs::write(&ffmpeg, format!("#!/bin/sh\nif [ \"$1\" = -version ]; then echo ffmpeg-test; else printf '%s\\n' '{encoders}'; fi\n"))?;
            std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755))
        };
        let tools = [ToolSpec {
            name: "ffmpeg".into(),
            package: "ffmpeg".into(),
            features: vec!["libopus".into()],
        }];
        write(" A..... libopus Opus encoder")?;
        assert!(check_tools_with_path(&tools, dir.path().as_os_str()).is_ok());
        write(" A..... libvorbis libopus in description")?;
        let err = check_tools_with_path(&tools, dir.path().as_os_str())
            .unwrap_err()
            .to_string();
        assert!(err.contains("libopus") && err.contains("ffmpeg"), "{err}");
        Ok(())
    }
}
