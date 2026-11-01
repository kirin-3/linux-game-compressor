use compressor_core::{
    process::run_process,
    registry::{Operation, Transformer},
    MediaKind, Profile, ToolSpec,
};
use std::{ffi::OsString, io, path::Path, process::Command};
use tokio_util::sync::CancellationToken;

pub struct WebpTransformer;
impl WebpTransformer {
    pub fn arguments(source: &Path, output: &Path, profile: &Profile) -> io::Result<Vec<OsString>> {
        if !source.is_absolute() || !output.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "encoder paths must be absolute",
            ));
        }
        let mut args: Vec<OsString> = vec![
            "-quiet".into(),
            "-metadata".into(),
            "none".into(),
            "-q".into(),
            profile.quality.to_string().into(),
            "-m".into(),
            profile.effort.to_string().into(),
            "-alpha_q".into(),
            "100".into(),
        ];
        if profile.sharp_yuv {
            args.push("-sharp_yuv".into());
        }
        args.extend([
            source.as_os_str().to_owned(),
            "-o".into(),
            output.as_os_str().to_owned(),
        ]);
        Ok(args)
    }
    /// Decode a WebP fully, for verifying an already validated output by hash.
    pub async fn validate_output(output: &Path, cancel: &CancellationToken) -> io::Result<()> {
        let pam = tempfile::NamedTempFile::new()?;
        let args = [
            std::ffi::OsStr::new("-quiet"),
            std::ffi::OsStr::new("-pam"),
            output.as_os_str(),
            std::ffi::OsStr::new("-o"),
            pam.path().as_os_str(),
        ];
        let result = run_process("dwebp".as_ref(), args, cancel, &CancellationToken::new()).await?;
        if result.cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if !result.status.success() {
            return Err(io::Error::other(format!(
                "decode failed: {}",
                String::from_utf8_lossy(&result.stderr)
            )));
        }
        Ok(())
    }
    pub fn version() -> io::Result<String> {
        let output = Command::new("cwebp")
            .arg("-version")
            .stdin(std::process::Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("cwebp -version failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}
impl Transformer for WebpTransformer {
    fn kind(&self) -> MediaKind {
        MediaKind::Image
    }
    fn required_tools(&self) -> Vec<ToolSpec> {
        ["cwebp", "dwebp"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                package: "libwebp-utils".into(),
            })
            .collect()
    }
    fn transform<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        profile: &'a Profile,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            let force = CancellationToken::new();
            self.transform_with_force(source, output, profile, cancel, &force)
                .await
        })
    }
    fn transform_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        profile: &'a Profile,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            let args = Self::arguments(source, output, profile)?;
            let result = run_process("cwebp".as_ref(), args, cancel, force).await?;
            if result.cancelled {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if !result.status.success() {
                return Err(io::Error::other(format!(
                    "cwebp: {}",
                    String::from_utf8_lossy(&result.stderr)
                )));
            }
            Ok(())
        })
    }
    fn validate<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            let force = CancellationToken::new();
            self.validate_with_force(source, output, cancel, &force)
                .await
        })
    }
    fn validate_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            if matches!(
                crate::detect(source)?,
                crate::ImageFormat::AnimatedPng | crate::ImageFormat::AnimatedWebp
            ) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "animated image"));
            }
            let pam = tempfile::NamedTempFile::new()?;
            let args = [
                std::ffi::OsStr::new("-quiet"),
                std::ffi::OsStr::new("-pam"),
                output.as_os_str(),
                std::ffi::OsStr::new("-o"),
                pam.path().as_os_str(),
            ];
            let result = run_process("dwebp".as_ref(), args, cancel, force).await?;
            if result.cancelled {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if !result.status.success() {
                return Err(io::Error::other(format!(
                    "decode failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                )));
            }
            let source = source.to_owned();
            let output = output.to_owned();
            let pam_path = pam.path().to_owned();
            tokio::task::spawn_blocking(move || validate_decoded(&source, &output, &pam_path))
                .await
                .map_err(io::Error::other)?
        })
    }
}

fn validate_decoded(source: &Path, output: &Path, pam: &Path) -> io::Result<()> {
    let source = image::ImageReader::open(source)?
        .with_guessed_format()?
        .decode()
        .map_err(|e| io::Error::other(format!("decode failed: {e}")))?
        .into_rgba8();
    let bytes = std::fs::read(pam)?;
    let marker = b"ENDHDR\n";
    let end = bytes
        .windows(marker.len())
        .position(|part| part == marker)
        .ok_or_else(|| io::Error::other("decode failed: invalid PAM header"))?
        + marker.len();
    let header = std::str::from_utf8(&bytes[..end]).map_err(io::Error::other)?;
    if !header.starts_with("P7\n") {
        return Err(io::Error::other("decode failed: invalid PAM signature"));
    }
    let number = |key: &str| -> io::Result<usize> {
        header
            .lines()
            .find_map(|l| l.strip_prefix(key).and_then(|v| v.trim().parse().ok()))
            .ok_or_else(|| io::Error::other("decode failed: invalid PAM header"))
    };
    let width = number("WIDTH ")?;
    let height = number("HEIGHT ")?;
    let depth = number("DEPTH ")?;
    let max = number("MAXVAL ")?;
    if width != source.width() as usize || height != source.height() as usize {
        return Err(io::Error::other("dimensions changed"));
    }
    if max != 255
        || (depth != 3 && depth != 4)
        || width.checked_mul(height).and_then(|n| n.checked_mul(depth)) != Some(bytes.len() - end)
    {
        return Err(io::Error::other("decode failed: invalid PAM data"));
    }
    let transparent = source.pixels().any(|p| p[3] < 255);
    if transparent && !webp_has_alpha(output)? {
        return Err(io::Error::other("alpha channel lost"));
    }
    for (index, pixel) in source.pixels().enumerate() {
        if pixel[3] == 0 && (depth != 4 || bytes[end + index * depth + 3] != 0) {
            return Err(io::Error::other("transparent pixels changed"));
        }
    }
    Ok(())
}

fn webp_has_alpha(path: &Path) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let mut riff = [0u8; 12];
    f.read_exact(&mut riff)?;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WEBP" {
        return Err(io::Error::other("decode failed: invalid WebP"));
    }
    loop {
        let mut chunk = [0u8; 8];
        match f.read_exact(&mut chunk) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(chunk[4..].try_into().unwrap());
        match &chunk[..4] {
            b"VP8X" if len >= 1 => {
                let mut flags = [0];
                f.read_exact(&mut flags)?;
                if flags[0] & 0x10 != 0 {
                    return Ok(true);
                }
                f.seek(SeekFrom::Current(i64::from(len - 1) + i64::from(len % 2)))?;
            }
            b"ALPH" => return Ok(true),
            b"VP8L" if len >= 5 => {
                let mut header = [0; 5];
                f.read_exact(&mut header)?;
                if header[0] != 0x2f {
                    return Err(io::Error::other("decode failed: invalid VP8L"));
                }
                return Ok(header[4] & 0x10 != 0);
            }
            _ => {
                f.seek(SeekFrom::Current(i64::from(len) + i64::from(len % 2)))?;
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_follow_profile_and_are_absolute() {
        let p = Profile {
            quality: 90,
            effort: 6,
            sharp_yuv: true,
            ..Profile::default()
        };
        let args =
            WebpTransformer::arguments(Path::new("/game/-a.png"), Path::new("/tmp/out.webp"), &p)
                .unwrap();
        assert_eq!(
            args,
            [
                "-quiet",
                "-metadata",
                "none",
                "-q",
                "90",
                "-m",
                "6",
                "-alpha_q",
                "100",
                "-sharp_yuv",
                "/game/-a.png",
                "-o",
                "/tmp/out.webp"
            ]
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
        assert!(
            WebpTransformer::arguments(Path::new("relative"), Path::new("/tmp/out.webp"), &p)
                .is_err()
        );
    }
    #[tokio::test]
    async fn validator_checks_dimensions_decode_and_transparency() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let images = fixture.root().join("game/images");
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("encoded.webp");
        let token = CancellationToken::new();
        let p = Profile::default();
        let codec = WebpTransformer;
        let transparent = images.join("sprite transparent.png");
        codec.transform(&transparent, &output, &p, &token).await?;
        codec.validate(&transparent, &output, &token).await?;
        codec
            .transform(&images.join("rgba-opaque.png"), &output, &p, &token)
            .await?;
        codec
            .validate(&images.join("rgba-opaque.png"), &output, &token)
            .await?;
        assert!(codec
            .validate(&transparent, &output, &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("alpha"));
        let different = dir.path().join("different.png");
        image::RgbaImage::new(3, 4)
            .save(&different)
            .map_err(io::Error::other)?;
        codec.transform(&different, &output, &p, &token).await?;
        assert!(codec
            .validate(&transparent, &output, &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("dimensions changed"));
        std::fs::write(&output, b"RIFFtruncated")?;
        assert!(codec
            .validate(&transparent, &output, &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("decode failed"));
        Ok(())
    }

    #[tokio::test]
    async fn small_profile_validates_still_webp_and_keeps_larger_result() -> io::Result<()> {
        use compressor_core::{
            registry::TransformerRegistry, Action, EngineInfo, Entry, Plan, StoredPath,
        };
        use std::sync::Arc;
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let dir = tempfile::tempdir()?;
        let source_dir = dir.path().join("input");
        std::fs::create_dir(&source_dir)?;
        let source = source_dir.join("sprite.png");
        let status = Command::new("cwebp")
            .args(["-quiet", "-lossless", "-z", "9"])
            .arg(fixture.root().join("game/images/sprite transparent.png"))
            .arg("-o")
            .arg(&source)
            .status()?;
        assert!(status.success());
        let profile = compressor_core::profile::load_profile(Some("small"))?;
        let converted = dir.path().join("check.webp");
        let token = CancellationToken::new();
        WebpTransformer
            .transform(&source, &converted, &profile, &token)
            .await?;
        WebpTransformer
            .validate(&source, &converted, &token)
            .await?;
        assert!(webp_has_alpha(&converted)?);
        let opaque = dir.path().join("opaque.webp");
        WebpTransformer
            .transform(
                &fixture.root().join("game/images/rgba-opaque.png"),
                &opaque,
                &profile,
                &token,
            )
            .await?;
        assert!(WebpTransformer
            .validate(&source, &opaque, &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("alpha"));
        assert!(std::fs::metadata(&converted)?.len() >= std::fs::metadata(&source)?.len());
        let (size, hash) = compressor_core::planning::hash_input(&source)?;
        let plan = Plan {
            plan_version: 1,
            input_root: StoredPath(source_dir.canonicalize()?),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            profile: profile.clone(),
            tools: WebpTransformer.required_tools(),
            entries: vec![Entry {
                path: StoredPath("sprite.png".into()),
                size,
                hash,
                action: Action::Convert {
                    kind: MediaKind::Image,
                    target: "webp".into(),
                    encoder: "cwebp".into(),
                    settings: profile,
                },
            }],
        };
        let mut registry = TransformerRegistry::default();
        registry.register(Arc::new(WebpTransformer));
        let full_out = dir.path().join("full-job");
        let (events, _) = tokio::sync::broadcast::channel(16);
        let report = compressor_core::job::run_job(
            &plan,
            &full_out,
            &registry,
            1,
            CancellationToken::new(),
            events,
        )
        .await?;
        assert!(report.failures.is_empty());
        assert!(report
            .skips
            .iter()
            .any(|skip| skip.outcome.contains("not smaller")));
        assert_eq!(
            std::fs::read(full_out.join("sprite.png"))?,
            std::fs::read(&source)?
        );
        let manifest: compressor_core::Manifest =
            serde_json::from_slice(&std::fs::read(full_out.join(".compressor/manifest.json"))?)?;
        assert_eq!(manifest.tool_versions.len(), 2);
        assert!(manifest
            .tool_versions
            .iter()
            .any(|tool| tool.path.0.ends_with("cwebp") && !tool.version.is_empty()));
        Ok(())
    }

    #[tokio::test]
    async fn converted_jpeg_has_no_metadata_chunks() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("result.webp");
        WebpTransformer
            .transform(
                &fixture.root().join("game/images/café.jpg"),
                &output,
                &Profile::default(),
                &CancellationToken::new(),
            )
            .await?;
        let info = Command::new("webpinfo").arg(&output).output()?;
        assert!(info.status.success());
        let info = String::from_utf8_lossy(&info.stdout);
        for chunk in ["Chunk EXIF", "Chunk ICCP", "Chunk XMP "] {
            assert!(!info.contains(chunk), "{chunk} in {info}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn cwebp_converts_fixture() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let out = tempfile::tempdir()?;
        let output = out.path().join("converted.webp");
        WebpTransformer
            .transform(
                &fixture.root().join("game/images/opaque.png"),
                &output,
                &Profile::default(),
                &CancellationToken::new(),
            )
            .await?;
        assert_eq!(crate::detect(&output)?, crate::ImageFormat::Webp);
        assert!(!WebpTransformer::version()?.is_empty());
        Ok(())
    }
}
