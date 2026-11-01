use std::{io, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
pub enum ConversionOutcome {
    Converted,
    KeptOriginal { reason: String, failed: bool },
}

#[cfg(test)]
pub(crate) async fn transform_one(
    source: PathBuf,
    destination: PathBuf,
    temporary: PathBuf,
    profile: crate::Profile,
    transformer: Arc<dyn crate::registry::Transformer>,
    cancel: &CancellationToken,
) -> io::Result<ConversionOutcome> {
    transform_one_with_force(
        source,
        destination,
        temporary,
        profile,
        transformer,
        cancel,
        &CancellationToken::new(),
    )
    .await
}

pub(crate) async fn transform_one_with_force(
    source: PathBuf,
    destination: PathBuf,
    temporary: PathBuf,
    profile: crate::Profile,
    transformer: Arc<dyn crate::registry::Transformer>,
    cancel: &CancellationToken,
    force: &CancellationToken,
) -> io::Result<ConversionOutcome> {
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let encoded = transformer
        .transform_with_force(&source, &temporary, &profile, cancel, force)
        .await;
    let outcome = if let Err(err) = encoded {
        ConversionOutcome::KeptOriginal {
            reason: format!("encoder failed: {err}"),
            failed: true,
        }
    } else if let Err(err) = tokio::fs::metadata(&temporary).await {
        ConversionOutcome::KeptOriginal {
            reason: format!("encoder produced no output: {err}"),
            failed: true,
        }
    } else if let Err(err) = transformer
        .validate_with_force(&source, &temporary, cancel, force)
        .await
    {
        ConversionOutcome::KeptOriginal {
            reason: format!("validation failed: {err}"),
            failed: true,
        }
    } else if tokio::fs::metadata(&temporary).await?.len()
        >= tokio::fs::metadata(&source).await?.len()
    {
        ConversionOutcome::KeptOriginal {
            reason: "kept original: not smaller".into(),
            failed: false,
        }
    } else {
        ConversionOutcome::Converted
    };
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    if matches!(outcome, ConversionOutcome::KeptOriginal { .. }) {
        let _ = tokio::fs::remove_file(&temporary).await;
        tokio::fs::copy(&source, &temporary).await?;
    }
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    crate::copy::preserve_metadata(&source, &temporary)?;
    tokio::fs::rename(&temporary, &destination).await?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        registry::{Operation, Transformer, TransformerRegistry},
        Action, EngineInfo, Entry, MediaKind, Plan, Profile, StoredPath, ToolSpec,
    };
    use std::{
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };
    struct Fake {
        active: AtomicUsize,
        max: AtomicUsize,
        started: AtomicUsize,
    }
    impl Transformer for Fake {
        fn kind(&self) -> MediaKind {
            MediaKind::Image
        }
        fn required_tools(&self) -> Vec<ToolSpec> {
            vec![]
        }
        fn transform<'a>(
            &'a self,
            _src: &'a Path,
            out: &'a Path,
            _: &'a Profile,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                tokio::fs::write(out, b"encoded").await?;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
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
    struct FailingFake {
        mode: &'static str,
    }
    impl Transformer for FailingFake {
        fn kind(&self) -> MediaKind {
            MediaKind::Image
        }
        fn required_tools(&self) -> Vec<ToolSpec> {
            vec![]
        }
        fn transform<'a>(
            &'a self,
            _: &'a Path,
            out: &'a Path,
            _: &'a Profile,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                if self.mode == "encoder" {
                    return Err(io::Error::other("encoder error"));
                }
                let data = if self.mode == "larger" {
                    b"output that is too large".as_slice()
                } else {
                    b"ok".as_slice()
                };
                tokio::fs::write(out, data).await
            })
        }
        fn validate<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
            _: &'a CancellationToken,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                if self.mode == "validation" {
                    Err(io::Error::other("bad image"))
                } else {
                    Ok(())
                }
            })
        }
    }
    #[tokio::test]
    async fn failed_or_larger_encodes_keep_original() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        for mode in ["encoder", "validation", "larger"] {
            let source = dir.path().join(format!("{mode}.src"));
            let destination = dir.path().join(format!("{mode}.dst"));
            let temporary = dir.path().join(format!("{mode}.tmp"));
            std::fs::write(&source, b"original")?;
            let result = transform_one(
                source,
                destination.clone(),
                temporary,
                Profile::default(),
                Arc::new(FailingFake { mode }),
                &CancellationToken::new(),
            )
            .await?;
            match result {
                ConversionOutcome::KeptOriginal { reason, failed } => {
                    assert_eq!(failed, mode != "larger");
                    assert!(reason.contains(match mode {
                        "encoder" => "encoder error",
                        "validation" => "bad image",
                        _ => "not smaller",
                    }));
                }
                _ => panic!("expected fallback"),
            }
            assert_eq!(std::fs::read(destination)?, b"original");
        }
        Ok(())
    }

    /// Build a plan converting `count` files with `content`, hashed like a real plan.
    fn convert_plan(input: &Path, count: usize, content: &[u8]) -> io::Result<Plan> {
        std::fs::create_dir_all(input)?;
        let input = input.canonicalize()?;
        let profile = Profile::default();
        let mut entries = Vec::new();
        for i in 0..count {
            let name = format!("{i}.png");
            std::fs::write(input.join(&name), content)?;
            let (size, hash) = crate::planning::hash_input(&input.join(&name))?;
            entries.push(Entry {
                path: StoredPath(name.into()),
                size,
                hash,
                action: Action::Convert {
                    kind: MediaKind::Image,
                    target: "webp".into(),
                    encoder: "fake".into(),
                    settings: profile.clone(),
                },
            });
        }
        Ok(Plan {
            plan_version: 1,
            input_root: StoredPath(input),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            profile,
            tools: vec![],
            entries,
        })
    }
    async fn run(
        plan: &Plan,
        output: &Path,
        registry: &TransformerRegistry,
        jobs: usize,
        cancel: CancellationToken,
    ) -> io::Result<crate::journal::Report> {
        let (events, _) = tokio::sync::broadcast::channel(64);
        crate::job::run_job(plan, output, registry, jobs, cancel, events).await
    }
    fn fake() -> Arc<Fake> {
        Arc::new(Fake {
            active: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn resumed_job_skips_completed_and_reprocesses_tampered() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("output");
        let plan = convert_plan(&dir.path().join("input"), 5, b"source is long enough")?;
        let fake = fake();
        let mut registry = TransformerRegistry::default();
        registry.register(fake.clone());
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        let journal_path = output.join(".compressor/journal.jsonl");
        let stop_task = tokio::spawn(async move {
            while !std::fs::metadata(&journal_path).is_ok_and(|m| m.len() > 0) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            stopper.cancel();
        });
        let error = run(&plan, &output, &registry, 1, cancel).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        stop_task.await.unwrap();
        let report: crate::journal::Report =
            serde_json::from_slice(&std::fs::read(output.join(".compressor/report.json"))?)?;
        assert!(!report.complete);
        assert!(!report.pending.is_empty());
        let manifest: crate::Manifest =
            serde_json::from_slice(&std::fs::read(output.join(".compressor/manifest.json"))?)?;
        assert!(!manifest.complete);
        assert_eq!(
            std::fs::read_dir(output.join(".compressor/tmp"))?.count(),
            0
        );
        let records =
            crate::journal::Journal::open(&output.join(".compressor/journal.jsonl"))?.records()?;
        let completed = crate::journal::resumable_records(&plan, &records, &output).len();
        assert!(completed >= 1 && completed < plan.entries.len());
        let before = fake.started.load(Ordering::SeqCst);
        assert!(
            run(&plan, &output, &registry, 1, CancellationToken::new())
                .await?
                .complete
        );
        assert_eq!(
            fake.started.load(Ordering::SeqCst) - before,
            plan.entries.len() - completed
        );
        std::fs::write(output.join("0.png"), b"tampered")?;
        let before = fake.started.load(Ordering::SeqCst);
        run(&plan, &output, &registry, 1, CancellationToken::new()).await?;
        assert_eq!(fake.started.load(Ordering::SeqCst) - before, 1);
        assert_eq!(std::fs::read(output.join("0.png"))?, b"encoded");
        Ok(())
    }

    #[tokio::test]
    async fn resumes_after_crash_before_manifest() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("output");
        let plan = convert_plan(&dir.path().join("input"), 2, b"source is long enough")?;
        let mut registry = TransformerRegistry::default();
        registry.register(fake());
        run(&plan, &output, &registry, 1, CancellationToken::new()).await?;
        // A killed process leaves plan.json and the journal but no manifest.
        std::fs::remove_file(output.join(".compressor/manifest.json"))?;
        assert!(
            run(&plan, &output, &registry, 1, CancellationToken::new())
                .await?
                .complete
        );
        Ok(())
    }

    #[tokio::test]
    async fn job_bounds_workers_and_installs_staged_files() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("output");
        let plan = convert_plan(
            &dir.path().join("input"),
            6,
            b"source is longer than encoded",
        )?;
        let fake = fake();
        let mut registry = TransformerRegistry::default();
        registry.register(fake.clone());
        run(&plan, &output, &registry, 2, CancellationToken::new()).await?;
        assert_eq!(fake.max.load(Ordering::SeqCst), 2);
        for i in 0..6 {
            assert_eq!(std::fs::read(output.join(format!("{i}.png")))?, b"encoded");
        }
        assert_eq!(
            std::fs::read_dir(output.join(".compressor/tmp"))?.count(),
            0
        );
        Ok(())
    }
}
