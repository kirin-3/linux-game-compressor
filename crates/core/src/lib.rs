//! Engine-independent planning and execution types.

pub mod copy;
pub mod job;
pub mod journal;
pub mod output;
pub mod planning;
pub mod process;
pub mod registry;
pub mod scheduler;
pub mod settings;
pub mod tools;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
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
    Audio,
    Video,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageSettings {
    pub quality: u8,
    pub effort: u8,
    pub sharp_yuv: bool,
    pub reencode_webp: bool,
    #[serde(default)]
    pub lossless: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioSettings {
    pub opus_kbps_per_channel: u32,
    pub vorbis_quality: u8,
    pub mp3_kbps_per_channel: u32,
}

/// The complete image and audio defaults for a named built-in profile.
pub fn builtin_settings(name: &str) -> Option<(ImageSettings, AudioSettings)> {
    let (quality, effort, sharp_yuv, reencode_webp, opus, vorbis, mp3) = match name {
        "balanced" => (82, 4, false, false, 48, 3, 64),
        "quality" => (90, 6, true, false, 64, 5, 96),
        "small" => (72, 6, false, true, 32, 1, 48),
        _ => return None,
    };
    Some((
        ImageSettings {
            quality,
            effort,
            sharp_yuv,
            reencode_webp,
            lossless: false,
        },
        AudioSettings {
            opus_kbps_per_channel: opus,
            vorbis_quality: vorbis,
            mp3_kbps_per_channel: mp3,
        },
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resolved {
    pub value: serde_json::Value,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedSettings {
    pub profile: String,
    pub values: BTreeMap<String, Resolved>,
    pub engine_options: BTreeMap<String, Resolved>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OptionKind {
    Enum(Vec<String>),
    Bool,
    Int { min: i64, max: i64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptionSpec {
    pub name: String,
    pub kind: OptionKind,
    pub default: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    Opus,
    Vorbis,
    Mp3,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioProps {
    pub channels: u32,
    pub sample_rate: u32,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioEncode {
    pub codec: AudioCodec,
    pub bitrate_kbps: Option<u32>,
    pub vorbis_quality: Option<u8>,
    pub source: AudioProps,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoSettings {
    pub crf: u8,
    pub effort: u8,
    pub opus_kbps_per_channel: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoProps {
    pub width: u32,
    pub height: u32,
    pub fps: String,
    pub duration_ms: u64,
    pub alpha: bool,
    pub audio_channels: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoEncode {
    pub crf: u8,
    pub cpu_used: u8,
    pub opus_kbps: Option<u32>,
    pub source: VideoProps,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConvertSettings {
    Image(ImageSettings),
    Audio(AudioEncode),
    Video(VideoEncode),
}

impl ResolvedSettings {
    pub fn video(&self) -> VideoSettings {
        let get = |key: &str| {
            self.values
                .get(key)
                .expect("resolved video setting")
                .value
                .clone()
        };
        VideoSettings {
            crf: serde_json::from_value(get("video.crf")).expect("validated crf"),
            effort: serde_json::from_value(get("video.effort")).expect("validated effort"),
            opus_kbps_per_channel: serde_json::from_value(get("video.opus_kbps_per_channel"))
                .expect("validated bitrate"),
        }
    }
    pub fn audio(&self) -> AudioSettings {
        let get = |key: &str| {
            self.values
                .get(key)
                .expect("resolved audio setting")
                .value
                .clone()
        };
        AudioSettings {
            opus_kbps_per_channel: serde_json::from_value(get("audio.opus_kbps_per_channel"))
                .expect("validated opus bitrate"),
            vorbis_quality: serde_json::from_value(get("audio.vorbis_quality"))
                .expect("validated vorbis quality"),
            mp3_kbps_per_channel: serde_json::from_value(get("audio.mp3_kbps_per_channel"))
                .expect("validated mp3 bitrate"),
        }
    }
    pub fn image(&self) -> ImageSettings {
        let get = |key: &str| {
            self.values
                .get(key)
                .expect("resolved image setting")
                .value
                .clone()
        };
        ImageSettings {
            quality: serde_json::from_value(get("image.quality")).expect("validated quality"),
            effort: serde_json::from_value(get("image.effort")).expect("validated effort"),
            sharp_yuv: serde_json::from_value(get("image.sharp_yuv")).expect("validated sharp_yuv"),
            reencode_webp: serde_json::from_value(get("image.reencode_webp"))
                .expect("validated reencode_webp"),
            lossless: false,
        }
    }
}

impl Default for ResolvedSettings {
    fn default() -> Self {
        settings::resolve(
            None,
            &[],
            "default",
            &BTreeMap::from([("default".into(), vec![])]),
        )
        .expect("built-in settings are valid")
    }
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
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    pub format: String,
    pub params: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Archive {
        archive: StoredPath,
    },
    Assemble {
        format: String,
        params: serde_json::Value,
        members: Vec<String>,
    },
    Rebuild {
        format: String,
    },
    Convert {
        kind: MediaKind,
        target: String,
        encoder: String,
        settings: ConvertSettings,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<Envelope>,
    },
    Remove {
        reason: String,
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
    pub member: Option<String>,
    pub size: u64,
    pub hash: String,
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    pub plan_version: u32,
    pub input_root: StoredPath,
    pub engine: EngineInfo,
    pub settings: ResolvedSettings,
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
    fn builtins_set_every_image_and_audio_key() {
        for (name, image, audio) in [
            ("balanced", (82, 4, false, false), (48, 3, 64)),
            ("quality", (90, 6, true, false), (64, 5, 96)),
            ("small", (72, 6, false, true), (32, 1, 48)),
        ] {
            let (i, a) = builtin_settings(name).unwrap();
            assert_eq!((i.quality, i.effort, i.sharp_yuv, i.reencode_webp), image);
            assert_eq!(
                (
                    a.opus_kbps_per_channel,
                    a.vorbis_quality,
                    a.mp3_kbps_per_channel
                ),
                audio
            );
        }
        assert!(builtin_settings("unknown").is_none());
    }

    #[test]
    fn settings_types_round_trip() {
        fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
            value: T,
        ) {
            assert_eq!(
                serde_json::from_str::<T>(&serde_json::to_string(&value).unwrap()).unwrap(),
                value
            );
        }
        let image = ImageSettings {
            quality: 82,
            effort: 4,
            sharp_yuv: false,
            reencode_webp: false,
            lossless: false,
        };
        let audio = AudioSettings {
            opus_kbps_per_channel: 48,
            vorbis_quality: 3,
            mp3_kbps_per_channel: 64,
        };
        let props = AudioProps {
            channels: 2,
            sample_rate: 44100,
            duration_ms: 1000,
        };
        let encode = AudioEncode {
            codec: AudioCodec::Opus,
            bitrate_kbps: Some(96),
            vorbis_quality: None,
            source: props.clone(),
        };
        let resolved = Resolved {
            value: serde_json::json!(82),
            source: "balanced".into(),
        };
        round_trip(image.clone());
        round_trip(audio);
        round_trip(props);
        round_trip(encode.clone());
        round_trip(resolved.clone());
        round_trip(ResolvedSettings {
            profile: "balanced".into(),
            values: BTreeMap::from([("image.quality".into(), resolved)]),
            engine_options: BTreeMap::new(),
        });
        for kind in [
            OptionKind::Enum(vec!["opus".into(), "vorbis".into()]),
            OptionKind::Bool,
            OptionKind::Int { min: 0, max: 10 },
        ] {
            round_trip(OptionSpec {
                name: "option".into(),
                kind,
                default: serde_json::json!(true),
            });
        }
        round_trip(ConvertSettings::Image(image));
        round_trip(ConvertSettings::Audio(encode));
        round_trip(MediaKind::Audio);
    }

    #[test]
    fn plan_round_trips_paths_and_actions() {
        let settings = ResolvedSettings::default();
        let paths = [
            PathBuf::from("game/café.png"),
            PathBuf::from(OsString::from_vec(b"game/invalid-\xff.png".to_vec())),
        ];
        let plan = Plan {
            plan_version: 5,
            input_root: StoredPath("/tmp/game".into()),
            engine: EngineInfo {
                name: "Ren'Py".into(),
                version: Some("8.5.3".into()),
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: settings.clone(),
            tools: vec![ToolSpec {
                name: "cwebp".into(),
                package: "libwebp-utils".into(),
                features: vec![],
            }],
            entries: vec![
                Entry {
                    path: StoredPath(paths[0].clone()),
                    member: None,
                    size: 12,
                    hash: "abc".into(),
                    action: Action::Convert {
                        kind: MediaKind::Image,
                        target: "webp".into(),
                        encoder: "cwebp".into(),
                        envelope: None,
                        settings: ConvertSettings::Image(settings.image()),
                    },
                },
                Entry {
                    path: StoredPath(paths[1].clone()),
                    member: None,
                    size: 7,
                    hash: "def".into(),
                    action: Action::Skip {
                        reason: "test".into(),
                    },
                },
                Entry {
                    path: StoredPath("unused.m4a".into()),
                    member: None,
                    size: 5,
                    hash: "remove".into(),
                    action: Action::Remove {
                        reason: "unused on desktop".into(),
                    },
                },
                Entry {
                    path: StoredPath("Game.sh".into()),
                    member: Some("member.png".into()),
                    size: 3,
                    hash: "ghi".into(),
                    action: Action::Rebuild {
                        format: "test-pack".into(),
                    },
                },
                Entry {
                    path: StoredPath("data/level0".into()),
                    member: None,
                    size: 3,
                    hash: "jkl".into(),
                    action: Action::Archive {
                        archive: StoredPath("data/data.unity3d".into()),
                    },
                },
                Entry {
                    path: StoredPath("data/data.unity3d".into()),
                    member: None,
                    size: 3,
                    hash: "mno".into(),
                    action: Action::Assemble {
                        format: "unity-fs".into(),
                        params: serde_json::json!({"format_version": 8}),
                        members: vec!["level0".into()],
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
