use crate::{
    Action, ConvertSettings, EngineInfo, MediaKind, OptionSpec, ResolvedSettings, ToolSpec,
};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

pub type Operation<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// A member and its planned action within an expandable pack.
#[derive(Clone, Debug)]
pub struct MemberPlan {
    pub member: String,
    pub size: u64,
    pub hash: String,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct ArchivePlan {
    pub path: PathBuf,
    pub format: String,
    pub params: serde_json::Value,
    pub members: Vec<PathBuf>,
}

/// Engine-specific detection and decisions; paths passed to `classify` are relative.
pub trait EngineAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn options(&self) -> Vec<OptionSpec> {
        Vec::new()
    }
    fn detect(&self, root: &Path) -> io::Result<Option<EngineInfo>>;
    fn archives(
        &self,
        _root: &Path,
        _engine: &EngineInfo,
        _settings: &ResolvedSettings,
    ) -> io::Result<Vec<ArchivePlan>> {
        Ok(Vec::new())
    }
    fn classify(
        &self,
        root: &Path,
        path: &Path,
        engine: &EngineInfo,
        settings: &ResolvedSettings,
    ) -> io::Result<Action>;
    /// Supported packs are classified as `Rebuild` with their format before expansion.
    fn expand(
        &self,
        _root: &Path,
        _path: &Path,
        _engine: &EngineInfo,
        _settings: &ResolvedSettings,
    ) -> io::Result<Option<Vec<MemberPlan>>> {
        Ok(None)
    }
    fn plan_notes(
        &self,
        _root: &Path,
        _engine: &EngineInfo,
        _settings: &ResolvedSettings,
        _entries: &[crate::Entry],
    ) -> Vec<String> {
        Vec::new()
    }
}

/// A media-specific encoder and independent validator.
pub trait Transformer: Send + Sync {
    fn kind(&self) -> MediaKind;
    fn required_tools(&self) -> Vec<ToolSpec>;
    fn required_tools_for(&self, _settings: &ConvertSettings) -> Vec<ToolSpec> {
        self.required_tools()
    }
    fn transform<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, Option<String>>;
    fn validate<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()>;
    fn transform_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        _force: &'a CancellationToken,
    ) -> Operation<'a, Option<String>> {
        self.transform(source, output, settings, cancel)
    }
    fn validate_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        _force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        self.validate(source, output, settings, cancel)
    }
}

/// An opaque directory entry identifying a member in a pack.
#[derive(Clone, Debug)]
pub struct PackMember {
    pub name: String,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct ArchiveInput {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub hash: String,
}

pub trait ArchiveFormat: Send + Sync {
    /// Estimate compressed bytes divided by input bytes when supported.
    fn estimate_ratio(
        &self,
        _inputs: &[ArchiveInput],
        _sample_blocks: usize,
        _cancel: &CancellationToken,
    ) -> io::Result<Option<f64>> {
        Ok(None)
    }
    fn write(
        &self,
        members: &[ArchiveInput],
        output: &Path,
        params: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> io::Result<()>;
    fn read_directory(&self, archive: &Path) -> io::Result<Vec<PackMember>>;
    fn hash_member(&self, archive: &Path, member: &PackMember) -> io::Result<(u64, String)>;
}

#[derive(Default)]
pub struct ArchiveRegistry(HashMap<String, Arc<dyn ArchiveFormat>>);
impl ArchiveRegistry {
    pub fn register(&mut self, format: &str, handler: Arc<dyn ArchiveFormat>) {
        self.0.insert(format.to_owned(), handler);
    }
    pub fn get(&self, format: &str) -> Option<Arc<dyn ArchiveFormat>> {
        self.0.get(format).cloned()
    }
}

pub trait PackFormat: Send + Sync {
    fn read_directory(&self, input: &Path) -> io::Result<Vec<PackMember>>;
    fn read_member(&self, input: &Path, member: &PackMember) -> io::Result<Vec<u8>>;
    fn rebuild(
        &self,
        input: &Path,
        output: &Path,
        replacements: &BTreeMap<String, PathBuf>,
    ) -> io::Result<()>;
}

pub trait EnvelopeFormat: Send + Sync {
    fn unwrap(&self, data: &[u8], params: &serde_json::Value) -> io::Result<Vec<u8>>;
    fn wrap(&self, payload: &[u8], params: &serde_json::Value) -> io::Result<Vec<u8>>;
}

#[derive(Clone, Default)]
pub struct EnvelopeRegistry(HashMap<String, Arc<dyn EnvelopeFormat>>);
impl EnvelopeRegistry {
    pub fn register(&mut self, format: &str, handler: Arc<dyn EnvelopeFormat>) {
        self.0.insert(format.to_owned(), handler);
    }
    pub fn get(&self, format: &str) -> Option<Arc<dyn EnvelopeFormat>> {
        self.0.get(format).cloned()
    }
}

#[derive(Clone, Default)]
pub struct PackRegistry(HashMap<String, Arc<dyn PackFormat>>);
impl PackRegistry {
    pub fn register(&mut self, format: &str, handler: Arc<dyn PackFormat>) {
        self.0.insert(format.to_owned(), handler);
    }
    pub fn get(&self, format: &str) -> Option<Arc<dyn PackFormat>> {
        self.0.get(format).cloned()
    }
}

#[derive(Default)]
pub struct EngineRegistry(Vec<Arc<dyn EngineAdapter>>);
impl EngineRegistry {
    pub fn register(&mut self, adapter: Arc<dyn EngineAdapter>) {
        self.0.push(adapter);
    }
    pub fn ids(&self) -> Vec<&'static str> {
        self.0.iter().map(|a| a.id()).collect()
    }
    pub fn by_id(&self, id: &str) -> Option<Arc<dyn EngineAdapter>> {
        self.0.iter().find(|a| a.id() == id).cloned()
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
pub struct TransformerRegistry(std::collections::HashMap<String, Arc<dyn Transformer>>);
impl TransformerRegistry {
    pub fn register(&mut self, encoder: &str, transformer: Arc<dyn Transformer>) {
        self.0.insert(encoder.to_owned(), transformer);
    }
    pub fn get(&self, encoder: &str) -> Option<Arc<dyn Transformer>> {
        self.0.get(encoder).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Archive;
    impl ArchiveFormat for Archive {
        fn write(
            &self,
            _: &[ArchiveInput],
            _: &Path,
            _: &serde_json::Value,
            _: &CancellationToken,
        ) -> io::Result<()> {
            Ok(())
        }
        fn read_directory(&self, _: &Path) -> io::Result<Vec<PackMember>> {
            Ok(vec![])
        }
        fn hash_member(&self, _: &Path, _: &PackMember) -> io::Result<(u64, String)> {
            Ok((0, "fake".into()))
        }
    }
    struct Adapter;
    impl EngineAdapter for Adapter {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn detect(&self, _: &Path) -> io::Result<Option<EngineInfo>> {
            Ok(Some(EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            }))
        }
        fn classify(
            &self,
            _: &Path,
            _: &Path,
            _: &EngineInfo,
            _: &ResolvedSettings,
        ) -> io::Result<Action> {
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
                features: vec![],
            }]
        }
        fn transform<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a ConvertSettings,
            _: &'a CancellationToken,
        ) -> Operation<'a, Option<String>> {
            Box::pin(async { Ok(None) })
        }
        fn validate<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a ConvertSettings,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    #[test]
    fn registries_find_fake_components() {
        let mut archives = ArchiveRegistry::default();
        archives.register("fake", Arc::new(Archive));
        assert!(archives.get("missing").is_none());
        assert!(archives
            .get("fake")
            .unwrap()
            .read_directory(Path::new("."))
            .unwrap()
            .is_empty());
        let mut engines = EngineRegistry::default();
        engines.register(Arc::new(Adapter));
        let (adapter, info) = engines.detect(Path::new(".")).unwrap().unwrap();
        assert_eq!(info.name, "fake");
        assert_eq!(engines.ids(), vec!["fake"]);
        assert_eq!(engines.by_id("fake").unwrap().id(), "fake");
        assert!(engines.by_id("missing").is_none());
        let profile = ResolvedSettings::default();
        assert!(matches!(
            adapter
                .classify(Path::new("."), Path::new("file"), &info, &profile)
                .unwrap(),
            Action::Copy { .. }
        ));
        let mut transformers = TransformerRegistry::default();
        transformers.register("fake", Arc::new(Encoder));
        assert_eq!(
            transformers.get("fake").unwrap().required_tools()[0].name,
            "fake"
        );
    }
}
