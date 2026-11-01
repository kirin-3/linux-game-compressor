use crate::{Action, EngineInfo, Manifest, MediaKind, Profile, ToolSpec};
use std::{future::Future, io, path::Path, pin::Pin, sync::Arc};
use tokio_util::sync::CancellationToken;

pub type Operation<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// Engine-specific detection and decisions; paths passed to `classify` are relative.
pub trait EngineAdapter: Send + Sync {
    fn detect(&self, root: &Path) -> io::Result<Option<EngineInfo>>;
    fn classify(
        &self,
        root: &Path,
        path: &Path,
        engine: &EngineInfo,
        profile: &Profile,
    ) -> io::Result<Action>;
    fn package(&self, _output: &Path, _manifest: &Manifest) -> io::Result<()> {
        Ok(())
    }
}

/// A media-specific encoder and independent validator.
pub trait Transformer: Send + Sync {
    fn kind(&self) -> MediaKind;
    fn required_tools(&self) -> Vec<ToolSpec>;
    fn transform<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        profile: &'a Profile,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()>;
    fn validate<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()>;
    fn transform_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        profile: &'a Profile,
        cancel: &'a CancellationToken,
        _force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        self.transform(source, output, profile, cancel)
    }
    fn validate_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        cancel: &'a CancellationToken,
        _force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        self.validate(source, output, cancel)
    }
}

#[derive(Default)]
pub struct EngineRegistry(Vec<Arc<dyn EngineAdapter>>);
impl EngineRegistry {
    pub fn register(&mut self, adapter: Arc<dyn EngineAdapter>) {
        self.0.push(adapter);
    }
    pub fn detect(&self, root: &Path) -> io::Result<Option<(Arc<dyn EngineAdapter>, EngineInfo)>> {
        for adapter in &self.0 {
            if let Some(info) = adapter.detect(root)? {
                return Ok(Some((Arc::clone(adapter), info)));
            }
        }
        Ok(None)
    }
}

#[derive(Default)]
pub struct TransformerRegistry(Vec<Arc<dyn Transformer>>);
impl TransformerRegistry {
    pub fn register(&mut self, transformer: Arc<dyn Transformer>) {
        self.0.push(transformer);
    }
    pub fn get(&self, kind: &MediaKind) -> Option<Arc<dyn Transformer>> {
        self.0.iter().find(|t| &t.kind() == kind).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Adapter;
    impl EngineAdapter for Adapter {
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            Ok(Some(EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            }))
        }
        fn classify(&self, _: &Path, _: &Path, _: &EngineInfo, _: &Profile) -> io::Result<Action> {
            Ok(Action::Copy {
                reason: "fake".into(),
            })
        }
    }
    struct Encoder;
    impl Transformer for Encoder {
        fn kind(&self) -> MediaKind {
            MediaKind::Image
        }
        fn required_tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "fake".into(),
                package: "fake-package".into(),
            }]
        }
        fn transform<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a Profile,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn validate<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    #[test]
    fn registries_find_fake_components() {
        let mut engines = EngineRegistry::default();
        engines.register(Arc::new(Adapter));
        let (adapter, info) = engines.detect(Path::new(".")).unwrap().unwrap();
        assert_eq!(info.name, "fake");
        let profile = Profile {
            name: "fake".into(),
            quality: 80,
            effort: 4,
            sharp_yuv: false,
            reencode_webp: false,
        };
        assert!(matches!(
            adapter
                .classify(Path::new("."), Path::new("file"), &info, &profile)
                .unwrap(),
            Action::Copy { .. }
        ));
        let mut transformers = TransformerRegistry::default();
        transformers.register(Arc::new(Encoder));
        assert_eq!(
            transformers
                .get(&MediaKind::Image)
                .unwrap()
                .required_tools()[0]
                .name,
            "fake"
        );
    }
}
