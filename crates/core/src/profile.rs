use crate::Profile;
use serde::Deserialize;
use std::{fs, io, path::Path};

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "balanced".into(),
            quality: 82,
            effort: 4,
            sharp_yuv: false,
            reencode_webp: false,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Overrides {
    name: Option<String>,
    quality: Option<u8>,
    effort: Option<u8>,
    sharp_yuv: Option<bool>,
    reencode_webp: Option<bool>,
}

/// Resolve a built-in profile name, or load a TOML file over balanced defaults.
pub fn load_profile(selection: Option<&str>) -> io::Result<Profile> {
    let mut profile = Profile::default();
    match selection.unwrap_or("balanced") {
        "balanced" => {}
        "quality" => {
            profile.name = "quality".into();
            profile.quality = 90;
            profile.effort = 6;
            profile.sharp_yuv = true;
        }
        "small" => {
            profile.name = "small".into();
            profile.quality = 72;
            profile.effort = 6;
            profile.reencode_webp = true;
        }
        path => {
            let content = fs::read_to_string(Path::new(path))?;
            let o: Overrides = toml::from_str(&content)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            profile.name = o.name.unwrap_or_else(|| path.to_owned());
            if let Some(v) = o.quality {
                profile.quality = v;
            }
            if let Some(v) = o.effort {
                profile.effort = v;
            }
            if let Some(v) = o.sharp_yuv {
                profile.sharp_yuv = v;
            }
            if let Some(v) = o.reencode_webp {
                profile.reencode_webp = v;
            }
        }
    }
    if profile.quality > 100 || profile.effort > 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "profile quality must be 0–100 and effort must be 0–6",
        ));
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_and_custom_profile() -> io::Result<()> {
        assert_eq!(load_profile(None)?.name, "balanced");
        assert_eq!(load_profile(None)?.quality, 82);
        assert!(load_profile(Some("small"))?.reencode_webp);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("custom.toml");
        fs::write(&path, "quality = 91\n")?;
        let profile = load_profile(Some(path.to_str().unwrap()))?;
        assert_eq!(profile.quality, 91);
        assert_eq!(profile.effort, 4);
        Ok(())
    }
}
