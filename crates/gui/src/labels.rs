pub(super) struct Label {
    pub name: String,
    pub help: &'static str,
    pub unit: &'static str,
}
pub(super) fn profile(id: &str) -> (&'static str, &'static str) {
    match id {
        "quality" => (
            "Best looking",
            "Keeps more image and sound detail, with a larger result.",
        ),
        "small" => (
            "Smallest",
            "Saves the most space with stronger image and sound compression.",
        ),
        _ => (
            "Recommended",
            "A balanced choice for quality and smaller files.",
        ),
    }
}
pub(super) fn setting(key: &str) -> Label {
    let (name, help, unit) = match key {
        "image.quality" => (
            "Image quality",
            "Higher values preserve more detail and use more space.",
            "%",
        ),
        "image.effort" => (
            "Image effort",
            "Higher effort takes longer and can make files smaller.",
            "",
        ),
        "image.sharp_yuv" => (
            "Sharper colour edges",
            "Preserve coloured edges more accurately when encoding.",
            "",
        ),
        "image.reencode_webp" => (
            "Re-compress existing WebP",
            "Allow existing WebP images to be compressed again.",
            "",
        ),
        "audio.opus_kbps_per_channel" => (
            "Opus bitrate",
            "Target bitrate per audio channel.",
            "kbps / channel",
        ),
        "audio.vorbis_quality" => (
            "Vorbis quality",
            "Higher values keep more sound detail.",
            "",
        ),
        "audio.mp3_kbps_per_channel" => (
            "MP3 bitrate",
            "Target bitrate per audio channel.",
            "kbps / channel",
        ),
        "video.crf" => (
            "Video quality",
            "Higher quality uses more space; CRF is the encoder's inverse quality scale.",
            "",
        ),
        "video.effort" => (
            "Video effort",
            "Higher effort takes longer and can make files smaller.",
            "",
        ),
        "video.opus_kbps_per_channel" => (
            "Video sound bitrate",
            "Target bitrate per audio channel in videos.",
            "kbps / channel",
        ),
        "engines.renpy.ogg_target" => (
            "Ogg audio format",
            "Choose the format used for Ogg audio.",
            "",
        ),
        "engines.godot.min_texture_kib" => (
            "Minimum texture size",
            "Keep smaller textures unchanged.",
            "KiB",
        ),
        "engines.rpgmaker.remove_unused" => (
            "Remove unused duplicates",
            "Remove audio duplicates that the game does not use.",
            "",
        ),
        "engines.unity.archive_data" => (
            "Archive player data",
            "Compress supported Unity player data into an archive.",
            "",
        ),
        _ => {
            return Label {
                name: key.rsplit('.').next().unwrap_or(key).replace('_', " "),
                help: "Engine option.",
                unit: "",
            }
        }
    };
    Label {
        name: name.into(),
        help,
        unit,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_cover_resolved_settings_and_engine_options() {
        let engines = compressor_app::engine_registry(false);
        let specs = compressor_app::engine_specs(&engines);
        for engine in engines.ids() {
            let resolved = compressor_core::settings::resolve(None, &[], engine, &specs).unwrap();
            for key in resolved.values.keys() {
                assert_ne!(setting(key).help, "Engine option.", "{key}");
            }
            for option in &specs[engine] {
                let key = format!("engines.{engine}.{}", option.name);
                assert_ne!(setting(&key).help, "Engine option.", "{key}");
            }
        }
    }
}
