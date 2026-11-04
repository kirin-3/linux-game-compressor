use compressor_core::{
    registry::{Operation, Transformer},
    ConvertSettings, MediaKind, ToolSpec,
};
use std::{io, path::Path};
use tokio_util::sync::CancellationToken;
pub struct GodotTextureTransformer;
impl Transformer for GodotTextureTransformer {
    fn kind(&self) -> MediaKind {
        MediaKind::Image
    }
    fn required_tools(&self) -> Vec<ToolSpec> {
        compressor_media::webp::WebpTransformer.required_tools()
    }
    fn transform<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, Option<String>> {
        Box::pin(async move {
            self.transform_with_force(src, dst, settings, cancel, &CancellationToken::new())
                .await
        })
    }
    fn transform_with_force<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, Option<String>> {
        Box::pin(async move {
            let mut texture = crate::ctex::Texture::parse(&std::fs::read(src)?)?;
            if texture.images.len() != 1 || texture.mipmaps != 0 {
                return Err(io::Error::other("unsupported texture type"));
            }
            let input = tempfile::NamedTempFile::new_in(dst.parent().unwrap_or(Path::new(".")))?;
            let output = tempfile::NamedTempFile::new_in(dst.parent().unwrap_or(Path::new(".")))?;
            std::fs::write(input.path(), &texture.images[0])?;
            let transformer = compressor_media::webp::WebpTransformer;
            let note = transformer
                .transform_with_force(input.path(), output.path(), settings, cancel, force)
                .await?;
            transformer
                .validate_with_force(input.path(), output.path(), settings, cancel, force)
                .await?;
            texture.data_format = 2;
            texture.images[0] = std::fs::read(output.path())?;
            std::fs::write(dst, texture.write()?)?;
            Ok(note)
        })
    }
    fn validate<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            self.validate_with_force(src, dst, settings, cancel, &CancellationToken::new())
                .await
        })
    }
    fn validate_with_force<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            let before = crate::ctex::Texture::parse(&std::fs::read(src)?)?;
            let after = crate::ctex::Texture::parse(&std::fs::read(dst)?)?;
            if before.header != after.header
                || before.width != after.width
                || before.height != after.height
                || before.image_format != after.image_format
                || after.data_format != 2
                || before.images.len() != 1
                || after.images.len() != 1
                || after.mipmaps != 0
            {
                return Err(io::Error::other("texture header changed"));
            }
            let input = tempfile::NamedTempFile::new_in(dst.parent().unwrap_or(Path::new(".")))?;
            let output = tempfile::NamedTempFile::new_in(dst.parent().unwrap_or(Path::new(".")))?;
            std::fs::write(input.path(), &before.images[0])?;
            std::fs::write(output.path(), &after.images[0])?;
            compressor_media::webp::WebpTransformer
                .validate_with_force(input.path(), output.path(), settings, cancel, force)
                .await
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn converts_lossless_texture_and_rejects_changed_header() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let png = dir.path().join("input.png");
        let webp = dir.path().join("lossless.webp");
        let mut image = image::RgbaImage::new(128, 128);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([
                (x * 2) as u8,
                (y * 2) as u8,
                ((x * y) % 256) as u8,
                if x < 20 { 0 } else { 255 },
            ]);
        }
        image.save(&png).map_err(io::Error::other)?;
        let status = std::process::Command::new("cwebp")
            .args(["-quiet", "-lossless"])
            .arg(&png)
            .arg("-o")
            .arg(&webp)
            .status()?;
        assert!(status.success());
        let mut header = [0; 36];
        header[..4].copy_from_slice(b"GST2");
        let texture = crate::ctex::Texture {
            header,
            data_format: 2,
            width: 128,
            height: 128,
            mipmaps: 0,
            image_format: 5,
            images: vec![std::fs::read(webp)?],
        };
        let src = dir.path().join("in.ctex");
        let dst = dir.path().join("out.ctex");
        std::fs::write(&src, texture.write()?)?;
        let settings = ConvertSettings::Image(compressor_core::ResolvedSettings::default().image());
        let cancel = CancellationToken::new();
        let transformer = GodotTextureTransformer;
        transformer
            .transform(&src, &dst, &settings, &cancel)
            .await?;
        transformer.validate(&src, &dst, &settings, &cancel).await?;
        assert!(std::fs::metadata(&dst)?.len() < std::fs::metadata(&src)?.len());
        let converted = crate::ctex::Texture::parse(&std::fs::read(&dst)?)?;
        assert_eq!(converted.header, texture.header);
        assert_eq!((converted.width, converted.height), (128, 128));
        let mut tampered = std::fs::read(&dst)?;
        tampered[8] ^= 1;
        std::fs::write(&dst, tampered)?;
        assert_eq!(
            transformer
                .validate(&src, &dst, &settings, &cancel)
                .await
                .unwrap_err()
                .to_string(),
            "texture header changed"
        );
        Ok(())
    }
}
