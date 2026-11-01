//! Validation of profile files and engine-specific options.
use crate::{OptionKind, OptionSpec, Resolved, ResolvedSettings};
use std::{collections::BTreeMap, fs, io, path::Path};

fn invalid(path: &Path, key: &str, reason: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {key}: {reason}", path.display()),
    )
}

fn validate_value(
    path: &Path,
    key: &str,
    value: &toml::Value,
    options: &[OptionSpec],
) -> io::Result<()> {
    let short = key.rsplit('.').next().unwrap_or(key);
    if let Some(option) = options.iter().find(|o| o.name == short) {
        let valid = match &option.kind {
            OptionKind::Enum(values) => value
                .as_str()
                .is_some_and(|v| values.iter().any(|s| s == v)),
            OptionKind::Bool => value.is_bool(),
            OptionKind::Int { min, max } => value
                .as_integer()
                .is_some_and(|v| (*min..=*max).contains(&v)),
        };
        if valid {
            return Ok(());
        }
        let allowed = match &option.kind {
            OptionKind::Enum(values) => values.join(", "),
            OptionKind::Bool => "true, false".into(),
            OptionKind::Int { min, max } => format!("integers between {min} and {max}"),
        };
        return Err(invalid(
            path,
            key,
            format!("invalid value; allowed values: {allowed}"),
        ));
    }
    let bounds = match short {
        "quality" => Some((0, 100)),
        "effort" => Some((0, 6)),
        "vorbis_quality" => Some((0, 10)),
        "opus_kbps_per_channel" => Some((6, 256)),
        "mp3_kbps_per_channel" => Some((8, 160)),
        _ => None,
    };
    if let Some((min, max)) = bounds {
        return match value.as_integer() {
            Some(v) if (min..=max).contains(&v) => Ok(()),
            _ => Err(invalid(
                path,
                key,
                format!("must be an integer between {min} and {max}"),
            )),
        };
    }
    if matches!(short, "sharp_yuv" | "reencode_webp") {
        return if value.is_bool() {
            Ok(())
        } else {
            Err(invalid(path, key, "must be a boolean"))
        };
    }
    Err(invalid(path, key, "unknown key"))
}

fn unknown_engine(engines: &BTreeMap<String, Vec<OptionSpec>>) -> String {
    format!(
        "unknown engine; known engines: {}",
        engines.keys().cloned().collect::<Vec<_>>().join(", ")
    )
}

fn unknown_option(options: &[OptionSpec]) -> String {
    let names: Vec<_> = options.iter().map(|o| o.name.as_str()).collect();
    format!("unknown option; allowed: {}", names.join(", "))
}

/// Read a profile file, distinguishing a missing name or path from other I/O errors.
pub(crate) fn read_profile_file(path: &Path) -> io::Result<String> {
    fs::read_to_string(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(io::ErrorKind::InvalidInput, format!(
                "unknown profile or profile file not found: {}; built-in profiles: balanced, quality, small",
                path.display()
            ))
        } else {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        }
    })
}

fn section(path: &Path, prefix: &str, value: &toml::Value) -> io::Result<()> {
    let table = value
        .as_table()
        .ok_or_else(|| invalid(path, prefix, "must be a table"))?;
    for (key, value) in table {
        let dotted = format!("{prefix}.{key}");
        let valid_key = match prefix.rsplit('.').next() {
            Some("image") => matches!(
                key.as_str(),
                "quality" | "effort" | "sharp_yuv" | "reencode_webp"
            ),
            Some("audio") => matches!(
                key.as_str(),
                "opus_kbps_per_channel" | "vorbis_quality" | "mp3_kbps_per_channel"
            ),
            _ => false,
        };
        if !valid_key {
            return Err(invalid(path, &dotted, "unknown key"));
        }
        validate_value(path, &dotted, value, &[])?;
    }
    Ok(())
}

/// Parse and validate all sections, including sections for engines not in use.
pub fn parse_profile(
    path: &Path,
    engines: &BTreeMap<String, Vec<OptionSpec>>,
) -> io::Result<toml::Table> {
    let text = read_profile_file(path)?;
    let table: toml::Table = toml::from_str(&text).map_err(|e| invalid(path, "profile", e))?;
    for (key, value) in &table {
        match key.as_str() {
            "name" | "base" => {
                let s = value
                    .as_str()
                    .ok_or_else(|| invalid(path, key, "must be a string"))?;
                if key == "base" && crate::builtin_settings(s).is_none() {
                    return Err(invalid(
                        path,
                        key,
                        "unknown built-in profile; available: balanced, quality, small",
                    ));
                }
            }
            "image" | "audio" => section(path, key, value)?,
            "engines" => {
                let entries = value
                    .as_table()
                    .ok_or_else(|| invalid(path, key, "must be a table"))?;
                for (id, engine) in entries {
                    let prefix = format!("engines.{id}");
                    let options = engines
                        .get(id)
                        .ok_or_else(|| invalid(path, &prefix, unknown_engine(engines)))?;
                    let entries = engine
                        .as_table()
                        .ok_or_else(|| invalid(path, &prefix, "must be a table"))?;
                    for (name, value) in entries {
                        let dotted = format!("{prefix}.{name}");
                        if matches!(name.as_str(), "image" | "audio") {
                            section(path, &dotted, value)?;
                        } else if options.iter().any(|o| o.name == *name) {
                            validate_value(path, &dotted, value, options)?;
                        } else {
                            return Err(invalid(path, &dotted, unknown_option(options)));
                        }
                    }
                }
            }
            _ => return Err(invalid(path, key, "unknown key")),
        }
    }
    Ok(table)
}

/// Resolve validated file and command-line settings, preserving per-key provenance.
pub fn resolve(
    selection: Option<&str>,
    overrides: &[String],
    engine: &str,
    engines: &BTreeMap<String, Vec<OptionSpec>>,
) -> io::Result<ResolvedSettings> {
    let options = engines
        .get(engine)
        .ok_or_else(|| invalid(Path::new("--engine"), engine, unknown_engine(engines)))?;
    let selection = selection.unwrap_or("balanced");
    let (file, base, profile) = if crate::builtin_settings(selection).is_some() {
        (
            toml::Table::new(),
            selection.to_owned(),
            selection.to_owned(),
        )
    } else {
        let table = parse_profile(Path::new(selection), engines)?;
        let base = table
            .get("base")
            .and_then(toml::Value::as_str)
            .unwrap_or("balanced")
            .to_owned();
        let name = table
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or(selection)
            .to_owned();
        (table, base, name)
    };
    let (image, audio) = crate::builtin_settings(&base).expect("validated base");
    let mut result = ResolvedSettings {
        profile,
        values: BTreeMap::new(),
        engine_options: BTreeMap::new(),
    };
    for (prefix, data) in [
        ("image", serde_json::to_value(image).unwrap()),
        ("audio", serde_json::to_value(audio).unwrap()),
    ] {
        for (key, value) in data.as_object().unwrap() {
            result.values.insert(
                format!("{prefix}.{key}"),
                Resolved {
                    value: value.clone(),
                    source: format!("profile {base}"),
                },
            );
        }
    }
    for option in options {
        result.engine_options.insert(
            option.name.clone(),
            Resolved {
                value: option.default.clone(),
                source: "engine default".into(),
            },
        );
    }
    fn flatten(table: &toml::Table, prefix: &str, out: &mut BTreeMap<String, toml::Value>) {
        for (key, value) in table {
            let key = if prefix.is_empty() {
                key.to_owned()
            } else {
                format!("{prefix}.{key}")
            };
            if let Some(t) = value.as_table() {
                flatten(t, &key, out);
            } else {
                out.insert(key, value.clone());
            }
        }
    }
    let mut file_values = BTreeMap::new();
    flatten(&file, "", &mut file_values);
    let mut sets = BTreeMap::new();
    for pair in overrides {
        let (key, raw) = pair
            .split_once('=')
            .ok_or_else(|| invalid(Path::new("--set"), pair, "expected key=value"))?;
        let parsed = format!("value = {raw}")
            .parse::<toml::Table>()
            .ok()
            .and_then(|mut t| t.remove("value"))
            .filter(|v| !v.is_array() && !v.is_table())
            .unwrap_or_else(|| toml::Value::String(raw.to_owned()));
        let parts: Vec<_> = key.split('.').collect();
        match parts.as_slice() {
            [section_name @ ("image" | "audio"), name] => {
                let mut t = toml::Table::new();
                t.insert((*name).into(), parsed.clone());
                section(Path::new("--set"), section_name, &toml::Value::Table(t))?;
            }
            ["engines", id, section_name @ ("image" | "audio"), name] => {
                if !engines.contains_key(*id) {
                    return Err(invalid(Path::new("--set"), key, unknown_engine(engines)));
                }
                let mut t = toml::Table::new();
                t.insert((*name).into(), parsed.clone());
                section(
                    Path::new("--set"),
                    &format!("engines.{id}.{section_name}"),
                    &toml::Value::Table(t),
                )?;
            }
            ["engines", id, name] => {
                let specs = engines
                    .get(*id)
                    .ok_or_else(|| invalid(Path::new("--set"), key, unknown_engine(engines)))?;
                if !specs.iter().any(|o| o.name == *name) {
                    return Err(invalid(Path::new("--set"), key, unknown_option(specs)));
                }
                validate_value(Path::new("--set"), key, &parsed, specs)?;
            }
            _ => return Err(invalid(Path::new("--set"), key, "unknown key")),
        }
        sets.insert(key.to_owned(), parsed);
    }
    for (key, value) in &mut result.values {
        let engine_key = format!("engines.{engine}.{key}");
        for (map, candidate, source) in [
            (&sets, engine_key.as_str(), "--set"),
            (&sets, key.as_str(), "--set"),
            (&file_values, engine_key.as_str(), "engine section"),
            (&file_values, key.as_str(), "profile file"),
        ] {
            if let Some(found) = map.get(candidate) {
                *value = Resolved {
                    value: serde_json::to_value(found).unwrap(),
                    source: source.into(),
                };
                break;
            }
        }
    }
    for (key, value) in &mut result.engine_options {
        let engine_key = format!("engines.{engine}.{key}");
        for (map, source) in [(&sets, "--set"), (&file_values, "engine section")] {
            if let Some(found) = map.get(&engine_key) {
                *value = Resolved {
                    value: serde_json::to_value(found).unwrap(),
                    source: source.into(),
                };
                break;
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bitrate_bounds_apply_to_profiles_and_overrides() {
        let engines = BTreeMap::from([("renpy".into(), vec![])]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        for (key, min, max) in [
            ("opus_kbps_per_channel", 6, 256),
            ("mp3_kbps_per_channel", 8, 160),
        ] {
            for value in [min, max] {
                assert!(
                    resolve(None, &[format!("audio.{key}={value}")], "renpy", &engines).is_ok()
                );
                fs::write(&path, format!("[engines.renpy.audio]\n{key} = {value}\n")).unwrap();
                assert!(parse_profile(&path, &engines).is_ok());
            }
            for value in [min - 1, max + 1, 500] {
                let err = resolve(None, &[format!("audio.{key}={value}")], "renpy", &engines)
                    .unwrap_err()
                    .to_string();
                assert!(err.contains(&format!("between {min} and {max}")), "{err}");
                fs::write(&path, format!("[engines.renpy.audio]\n{key} = {value}\n")).unwrap();
                let err = parse_profile(&path, &engines).unwrap_err().to_string();
                assert!(err.contains(&format!("engines.renpy.audio.{key}")), "{err}");
            }
        }
    }

    #[test]
    fn option_errors_list_plain_values() {
        let engines = BTreeMap::from([(
            "renpy".into(),
            vec![OptionSpec {
                name: "ogg_target".into(),
                kind: OptionKind::Enum(vec!["opus".into(), "vorbis".into()]),
                default: serde_json::json!("opus"),
            }],
        )]);
        let err = resolve(
            None,
            &["engines.renpy.ogg_target=aac".into()],
            "renpy",
            &engines,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("allowed values: opus, vorbis"), "{err}");
    }

    #[test]
    fn missing_profile_names_and_files_are_explained() {
        let engines = BTreeMap::from([("renpy".into(), vec![])]);
        let err = resolve(Some("balancd"), &[], "renpy", &engines)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown profile"), "{err}");
        assert!(err.contains("balanced, quality, small"), "{err}");
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        let err = resolve(missing.to_str(), &[], "renpy", &engines)
            .unwrap_err()
            .to_string();
        assert!(err.contains(missing.to_str().unwrap()), "{err}");
        assert!(err.contains("file not found"), "{err}");
    }
    #[test]
    fn override_precedence_and_other_engine_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        let engines = BTreeMap::from([("renpy".into(), vec![]), ("other".into(), vec![])]);
        fs::write(
            &path,
            "[engines.renpy.image]\nquality = 78\n[engines.other.image]\nquality = 60\n",
        )
        .unwrap();
        let selection = path.to_str();
        let value = |sets: &[&str]| {
            resolve(
                selection,
                &sets.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "renpy",
                &engines,
            )
            .unwrap()
            .values["image.quality"]
                .value
                .clone()
        };
        assert_eq!(value(&["image.quality=70"]), serde_json::json!(70));
        assert_eq!(
            value(&["image.quality=70", "engines.renpy.image.quality=75"]),
            serde_json::json!(75)
        );
        assert_eq!(value(&[]), serde_json::json!(78));
        fs::write(&path, "[engines.other.image]\nquality = 150\n").unwrap();
        assert!(resolve(selection, &[], "renpy", &engines).is_err());
    }
    #[test]
    fn rejects_invalid_profile_values_with_location() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        let engines = BTreeMap::from([(
            "renpy".into(),
            vec![OptionSpec {
                name: "ogg_target".into(),
                kind: OptionKind::Enum(vec!["opus".into(), "vorbis".into()]),
                default: serde_json::json!("opus"),
            }],
        )]);
        for (source, key) in [
            ("[image]\nqualty = 80", "image.qualty"),
            ("[audio]\nvorbis_quality = 12", "audio.vorbis_quality"),
            ("[engines.unity]\n", "engines.unity"),
            (
                "[engines.renpy]\nogg_target = 'aac'",
                "engines.renpy.ogg_target",
            ),
        ] {
            fs::write(&path, source).unwrap();
            let err = parse_profile(&path, &engines).unwrap_err().to_string();
            assert!(err.contains(key), "{err}");
            assert!(err.contains(path.to_str().unwrap()), "{err}");
        }
    }
}
