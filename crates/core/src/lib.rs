//! Engine-independent planning and execution types.

pub mod copy;
pub mod job;
pub mod journal;
pub mod output;
pub mod planning;
pub mod process;
pub mod profile;
pub mod registry;
pub mod scheduler;
pub mod tools;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// A filesystem path with a lossless JSON representation on Unix.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StoredPath(pub PathBuf);

impl Serialize for StoredPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0.to_str() {
            Some(s) => serializer.serialize_str(s),
            None => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("bytes_b64", &STANDARD.encode(self.0.as_os_str().as_bytes()))?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for StoredPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Representation {
            Text(String),
            Bytes { bytes_b64: String },
        }
        let path = match Representation::deserialize(deserializer)? {
            Representation::Text(s) => PathBuf::from(s),
            Representation::Bytes { bytes_b64 } => {
                let bytes = STANDARD.decode(bytes_b64).map_err(de::Error::custom)?;
                PathBuf::from(OsString::from_vec(bytes))
            }
        };
        Ok(Self(path))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub quality: u8,
    pub effort: u8,
    pub sharp_yuv: bool,
    pub reencode_webp: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineInfo {
    pub name: String,
    pub version: Option<String>,
    #[serde(default)]
    pub excluded_paths: Vec<StoredPath>,
    pub notes: Vec<String>,
    pub version_check_overridden: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub package: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Convert {
        kind: MediaKind,
        target: String,
        encoder: String,
        settings: Profile,
    },
    Copy {
        reason: String,
    },
    Skip {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: StoredPath,
    pub size: u64,
    pub hash: String,
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    pub plan_version: u32,
    pub input_root: StoredPath,
    pub engine: EngineInfo,
    pub profile: Profile,
    pub tools: Vec<ToolSpec>,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolVersion {
    pub path: StoredPath,
    pub version: String,
}

/// A job manifest; completion records and tool versions are filled by the pipeline.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub plan: Plan,
    pub complete: bool,
    pub tool_version: String,
    pub tool_versions: Vec<ToolVersion>,
    pub entries: Vec<journal::JournalEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn plan_round_trips_paths_and_actions() {
        let profile = Profile {
            name: "balanced".into(),
            quality: 82,
            effort: 4,
            sharp_yuv: false,
            reencode_webp: false,
        };
        let paths = [
            PathBuf::from("game/café.png"),
            PathBuf::from(OsString::from_vec(b"game/invalid-\xff.png".to_vec())),
        ];
        let plan = Plan {
            plan_version: 1,
            input_root: StoredPath("/tmp/game".into()),
            engine: EngineInfo {
                name: "Ren'Py".into(),
                version: Some("8.5.3".into()),
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            profile: profile.clone(),
            tools: vec![ToolSpec {
                name: "cwebp".into(),
                package: "libwebp-utils".into(),
            }],
            entries: vec![
                Entry {
                    path: StoredPath(paths[0].clone()),
                    size: 12,
                    hash: "abc".into(),
                    action: Action::Convert {
                        kind: MediaKind::Image,
                        target: "webp".into(),
                        encoder: "cwebp".into(),
                        settings: profile,
                    },
                },
                Entry {
                    path: StoredPath(paths[1].clone()),
                    size: 7,
                    hash: "def".into(),
                    action: Action::Skip {
                        reason: "test".into(),
                    },
                },
                Entry {
                    path: StoredPath("Game.sh".into()),
                    size: 3,
                    hash: "ghi".into(),
                    action: Action::Copy {
                        reason: "launcher".into(),
                    },
                },
            ],
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("bytes_b64"));
        assert!(json.contains("game/café.png"));
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);
    }
}
