//! Sampling-based, read-only output-size estimates.
use compressor_core::{
    planning::preview_plan, registry::ArchiveInput, Action, Entry, MediaKind, Plan,
    ResolvedSettings,
};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default)]
pub struct Estimate {
    pub likely: u64,
    pub low: u64,
    pub high: u64,
    pub sampled_bytes: u64,
    pub convertible_bytes: u64,
    pub failed_samples: usize,
    pub breakdown: BTreeMap<String, u64>,
}

#[derive(Clone)]
pub struct EstimateOptions {
    pub image_samples: usize,
    pub audio_samples: usize,
    pub video_samples: usize,
    pub video_seconds: f64,
    pub cache: Arc<Mutex<BTreeMap<String, Sample>>>,
    pub limiter: Option<Arc<crate::sample_limiter::SampleLimiter>>,
    pub temporary_parent: Option<PathBuf>,
}
impl Default for EstimateOptions {
    fn default() -> Self {
        Self {
            image_samples: 24,
            audio_samples: 48,
            video_samples: 3,
            video_seconds: 8.0,
            cache: Arc::default(),
            limiter: None,
            temporary_parent: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Sample {
    output: u64,
    failed: bool,
    full: bool,
}

fn cancelled(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
    } else {
        Ok(())
    }
}
fn sample_path(plan: &Plan, entry: &Entry) -> PathBuf {
    let mut path = plan.input_root.0.join(&entry.path.0).into_os_string();
    if let Some(member) = &entry.member {
        path.push("::");
        path.push(member);
    }
    path.into()
}
fn sample_indices(entries: &[&Entry], limit: usize) -> Vec<usize> {
    let mut indices: Vec<_> = (0..entries.len()).collect();
    indices.sort_by(|&a, &b| {
        entries[a]
            .size
            .cmp(&entries[b].size)
            .then(entries[a].path.0.cmp(&entries[b].path.0))
            .then(entries[a].member.cmp(&entries[b].member))
    });
    let count = limit.min(indices.len());
    (0..count)
        .map(|i| indices[(i * indices.len() / count + (i + 1) * indices.len() / count) / 2])
        .collect()
}

/// Preview and estimate a game, calling `progress` after each sampled entry.
pub async fn estimate(
    game: &Path,
    settings: &ResolvedSettings,
    options: &EstimateOptions,
    cancel: &CancellationToken,
    mut progress: impl FnMut(usize, usize),
) -> io::Result<Estimate> {
    cancelled(cancel)?;
    let game = game.to_owned();
    let settings = settings.clone();
    let planning = tokio::task::spawn_blocking(move || {
        let registries = crate::registries(false);
        let (adapter, engine) = registries.engines.detect(&game)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no supported engine detected")
        })?;
        preview_plan(
            &game,
            adapter.as_ref(),
            engine,
            settings,
            &registries.transformers,
        )
    });
    let plan = tokio::select! {
        plan = planning => plan.map_err(io::Error::other)??,
        _ = cancel.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
    };
    estimate_plan(&plan, options, cancel, &mut progress).await
}

async fn estimate_plan(
    plan: &Plan,
    options: &EstimateOptions,
    cancel: &CancellationToken,
    progress: &mut impl FnMut(usize, usize),
) -> io::Result<Estimate> {
    cancelled(cancel)?;
    let temp = match &options.temporary_parent {
        Some(parent) => tempfile::tempdir_in(parent)?,
        None => tempfile::tempdir()?,
    };
    let regs = crate::registries(false);
    let mut estimate = Estimate::default();
    let mut base = 0f64;
    let mut maximum = 0f64;
    let mut variance = 0f64;
    let mut groups: Vec<(&str, usize, Vec<&Entry>)> = vec![
        ("images", options.image_samples, vec![]),
        ("audio", options.audio_samples, vec![]),
        ("video", options.video_samples, vec![]),
    ];
    for entry in &plan.entries {
        match &entry.action {
            Action::Convert { kind, .. } => {
                let index = match kind {
                    MediaKind::Image => 0,
                    MediaKind::Audio => 1,
                    MediaKind::Video => 2,
                };
                groups[index].2.push(entry);
                estimate.convertible_bytes += entry.size;
                maximum += entry.size as f64;
            }
            Action::Copy { .. } | Action::Skip { .. } => {
                base += entry.size as f64;
                maximum += entry.size as f64;
            }
            Action::Rebuild { .. } => {
                // A pack may be embedded in an executable. Preserve the bytes
                // outside its listed members, including its container overhead.
                let members: u64 = plan
                    .entries
                    .iter()
                    .filter(|member| member.path == entry.path && member.member.is_some())
                    .map(|member| member.size)
                    .sum();
                base += entry.size.saturating_sub(members) as f64;
                maximum += entry.size.saturating_sub(members) as f64;
            }
            Action::Assemble { format, .. } => {
                let inputs: Vec<_> = plan.entries.iter().filter(|e| matches!(&e.action, Action::Archive { archive } if archive == &entry.path)).map(|e| ArchiveInput { name: e.path.0.to_string_lossy().into_owned(), path: plan.input_root.0.join(&e.path.0), size: e.size, hash: e.hash.clone() }).collect();
                let handler = regs
                    .archives
                    .get(format)
                    .ok_or_else(|| io::Error::other("missing archive format"))?;
                let ratio = loop {
                    let permit = if let Some(limiter) = &options.limiter {
                        Some(limiter.acquire(sample_path(plan, entry), cancel).await?)
                    } else {
                        None
                    };
                    let sample_cancel = permit
                        .as_ref()
                        .map_or_else(|| cancel.child_token(), |p| p.cancel.clone());
                    let handler = handler.clone();
                    let inputs = inputs.clone();
                    let token = sample_cancel.clone();
                    let ratio = tokio::task::spawn_blocking(move || {
                        handler.estimate_ratio(&inputs, 32, &token)
                    })
                    .await
                    .map_err(io::Error::other)?;
                    cancelled(cancel)?;
                    if sample_cancel.is_cancelled() {
                        drop(permit);
                        continue;
                    }
                    break ratio?.unwrap_or(1.0);
                };
                let bytes = (entry.size as f64 * ratio).round() as u64;
                estimate.breakdown.insert("archive".into(), bytes);
                base += bytes as f64;
                maximum += entry.size as f64;
            }
            _ => {}
        }
    }
    let total_samples: usize = groups
        .iter()
        .map(|(_, limit, entries)| entries.len().min(*limit))
        .sum();
    let mut done = 0;
    let mut succeeded = 0;
    for (kind, limit, entries) in groups {
        if entries.is_empty() {
            continue;
        }
        let indices = sample_indices(&entries, limit);
        if indices.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sample limit must be positive",
            ));
        }
        let mut sampled_in = 0u64;
        let mut sampled_out = 0u64;
        let mut ratios = Vec::new();
        let mut all_full = true;
        let mut tasks = tokio::task::JoinSet::new();
        let plan_for_samples = Arc::new(plan.clone());
        let regs_for_samples = Arc::new(crate::registries(false));
        let mut sample_options = options.clone();
        if sample_options.limiter.is_none() {
            let cores = std::thread::available_parallelism().map_or(1, usize::from);
            sample_options.limiter = Some(crate::sample_limiter::SampleLimiter::new(
                (cores / 2).max(1),
            ));
        }
        for (slot, index) in indices.into_iter().enumerate() {
            let slot = done + slot;
            let plan = plan_for_samples.clone();
            let regs = regs_for_samples.clone();
            let options = sample_options.clone();
            let entry = entries[index].clone();
            let cancel = cancel.clone();
            let dir = temp.path().to_owned();
            tasks.spawn(async move {
                let input_size = entry.size;
                let sample = sample_entry(plan, entry, regs, options, cancel, dir, slot).await?;
                Ok::<_, io::Error>((slot, input_size, sample))
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let result = joined.map_err(io::Error::other)??;
            results.push(result);
            done += 1;
            progress(done, total_samples);
            cancelled(cancel)?;
        }
        results.sort_by_key(|(slot, _, _)| *slot);
        for (_, input, sample) in results {
            if sample.failed {
                estimate.failed_samples += 1;
            } else {
                succeeded += 1;
            }
            sampled_in += input;
            sampled_out += sample.output;
            all_full &= sample.full;
            if input > 0 {
                ratios.push(sample.output as f64 / input as f64);
            }
        }
        let total: u64 = entries.iter().map(|entry| entry.size).sum();
        estimate.sampled_bytes += sampled_in;
        let ratio = if sampled_in == 0 {
            1.0
        } else {
            sampled_out as f64 / sampled_in as f64
        };
        let unsampled = total.saturating_sub(sampled_in);
        let value = sampled_out as f64 + unsampled as f64 * ratio;
        estimate.breakdown.insert(kind.into(), value.round() as u64);
        base += value;
        let se = if ratios.len() < 3 {
            ratio * 0.25
        } else {
            let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
            (ratios.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (ratios.len() - 1) as f64)
                .sqrt()
                / (ratios.len() as f64).sqrt()
        };
        let uncertainty_bytes = if all_full { unsampled } else { total };
        variance += (2.0 * se * uncertainty_bytes as f64).powi(2);
    }
    if estimate.convertible_bytes > 0 && succeeded == 0 {
        return Err(io::Error::other("no convertible samples could be encoded"));
    }
    let uncertainty = variance.sqrt();
    estimate.likely = base.round() as u64;
    estimate.low = (base - uncertainty).max(0.0).round() as u64;
    estimate.high = (base + uncertainty).min(maximum).round() as u64;
    Ok(estimate)
}

async fn sample_entry(
    plan: Arc<Plan>,
    entry: Entry,
    regs: Arc<crate::Registries>,
    options: EstimateOptions,
    cancel: CancellationToken,
    dir: PathBuf,
    slot: usize,
) -> io::Result<Sample> {
    cancelled(&cancel)?;
    let Action::Convert {
        encoder,
        settings,
        envelope,
        ..
    } = &entry.action
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sample is not convertible",
        ));
    };
    let key = serde_json::to_string(&(
        plan.input_root.0.clone(),
        &entry.path,
        &entry.member,
        entry.size,
        settings,
        envelope,
        options.video_seconds,
    ))?;
    if let Some(sample) = options.cache.lock().unwrap().get(&key).cloned() {
        return Ok(sample);
    }
    loop {
        let permit = if let Some(limiter) = &options.limiter {
            Some(limiter.acquire(sample_path(&plan, &entry), &cancel).await?)
        } else {
            None
        };
        let sample_cancel = permit
            .as_ref()
            .map_or_else(|| cancel.child_token(), |p| p.cancel.clone());
        let output = dir.join(format!("sample-{slot}"));
        match std::fs::remove_file(&output) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let result: io::Result<(u64, bool)> = async {
            let stage_plan = plan.clone();
            let stage_entry = entry.clone();
            let stage_regs = regs.clone();
            let stage_dir = dir.clone();
            let source = tokio::task::spawn_blocking(move || {
                compressor_core::planning::stage_entry_source(
                    &stage_plan.input_root.0,
                    &stage_plan,
                    &stage_entry,
                    &stage_regs.packs,
                    &stage_regs.envelopes,
                    &stage_dir,
                )
            })
            .await
            .map_err(io::Error::other)??;
            let transformer = regs
                .transformers
                .get(encoder)
                .ok_or_else(|| io::Error::other("missing transformer"))?;
            if let compressor_core::ConvertSettings::Video(video) = settings {
                let duration = video.source.duration_ms as f64 / 1000.0;
                if duration >= 12.0 {
                    let seconds = options.video_seconds.min(duration);
                    compressor_media::video::encode_clip_with_cancel(
                        &source,
                        video,
                        (duration * 0.4).min(duration - seconds),
                        seconds,
                        &output,
                        &sample_cancel,
                    )
                    .await?;
                    let size = std::fs::metadata(&output)?.len() as f64 * duration / seconds;
                    return Ok(((size.round() as u64).min(entry.size), false));
                }
            }
            transformer
                .transform_with_force(&source, &output, settings, &sample_cancel, &sample_cancel)
                .await?;
            let bytes = if let Some(envelope) = envelope {
                let handler = regs
                    .envelopes
                    .get(&envelope.format)
                    .ok_or_else(|| io::Error::other("missing envelope"))?;
                handler
                    .wrap(&std::fs::read(&output)?, &envelope.params)?
                    .len() as u64
            } else {
                std::fs::metadata(&output)?.len()
            };
            Ok((bytes.min(entry.size), true))
        }
        .await;
        cancelled(&cancel)?;
        if sample_cancel.is_cancelled() {
            drop(permit);
            continue;
        }
        let sample = match result {
            Ok((output, full)) => Sample {
                output,
                failed: false,
                full,
            },
            Err(_) => Sample {
                output: entry.size,
                failed: true,
                full: true,
            },
        };
        options.cache.lock().unwrap().insert(key, sample.clone());
        return Ok(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_plan(root: &Path) -> io::Result<Plan> {
        let regs = crate::registries(false);
        let (adapter, engine) = regs.engines.detect(root)?.unwrap();
        let settings =
            compressor_core::settings::resolve(None, &[], adapter.id(), &regs.engine_specs)?;
        preview_plan(root, adapter.as_ref(), engine, settings, &regs.transformers)
    }
    #[test]
    fn deterministic_binned_sampling_and_full_sample() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let plan = fixture_plan(fixture.root())?;
        let entries: Vec<_> = plan
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.action,
                    Action::Convert {
                        kind: MediaKind::Image,
                        ..
                    }
                )
            })
            .collect();
        assert!(entries.len() > 2);
        assert_eq!(sample_indices(&entries, 2), sample_indices(&entries, 2));
        let mut all = sample_indices(&entries, entries.len() + 1);
        all.sort();
        assert_eq!(all, (0..entries.len()).collect::<Vec<_>>());
        Ok(())
    }
    #[tokio::test]
    async fn estimate_ranges_cache_and_cleanup() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let plan = fixture_plan(fixture.root())?;
        let scratch = tempfile::tempdir()?;
        let options = EstimateOptions {
            temporary_parent: Some(scratch.path().to_owned()),
            ..Default::default()
        };
        let token = CancellationToken::new();
        let full = estimate_plan(&plan, &options, &token, &mut |_, _| {}).await?;
        assert_eq!((full.low, full.high), (full.likely, full.likely));
        assert!(std::fs::read_dir(scratch.path())?.next().is_none());
        let first_count = options.cache.lock().unwrap().len();
        assert!(first_count > 0);
        // A repeated estimate must use the cached encodes, even if the input
        // becomes unreadable after planning.
        for entry in plan
            .entries
            .iter()
            .filter(|e| matches!(e.action, Action::Convert { .. }))
        {
            if entry.member.is_none() {
                std::fs::write(plan.input_root.0.join(&entry.path.0), b"corrupt")?;
            }
        }
        let again = estimate_plan(&plan, &options, &token, &mut |_, _| {}).await?;
        assert_eq!(full.likely, again.likely);
        assert_eq!(first_count, options.cache.lock().unwrap().len());
        // Restore a separate fixture for the partial sample.
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let plan = fixture_plan(fixture.root())?;
        let partial = EstimateOptions {
            image_samples: 1,
            cache: Arc::default(),
            ..options.clone()
        };
        let partial = estimate_plan(&plan, &partial, &token, &mut |_, _| {}).await?;
        assert!(partial.low < partial.likely && partial.high > partial.likely);
        Ok(())
    }
    #[tokio::test]
    async fn corrupt_sample_counts_as_original() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let plan = fixture_plan(fixture.root())?;
        let entry = plan
            .entries
            .iter()
            .find(|e| {
                matches!(
                    e.action,
                    Action::Convert {
                        kind: MediaKind::Image,
                        ..
                    }
                ) && e.member.is_none()
            })
            .unwrap();
        std::fs::write(plan.input_root.0.join(&entry.path.0), b"not an image")?;
        let result = estimate_plan(
            &plan,
            &EstimateOptions::default(),
            &CancellationToken::new(),
            &mut |_, _| {},
        )
        .await?;
        assert_eq!(result.failed_samples, 1);
        Ok(())
    }
    #[tokio::test]
    async fn copy_only_and_cancel_remove_temp() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let mut plan = fixture_plan(fixture.root())?;
        plan.entries.retain(|e| e.member.is_none());
        for entry in &mut plan.entries {
            entry.action = Action::Copy {
                reason: "test".into(),
            };
        }
        let expected = plan.entries.iter().map(|e| e.size).sum();
        let result = estimate_plan(
            &plan,
            &EstimateOptions::default(),
            &CancellationToken::new(),
            &mut |_, _| {},
        )
        .await?;
        assert_eq!(
            (result.low, result.likely, result.high),
            (expected, expected, expected)
        );
        let plan = fixture_plan(fixture.root())?;
        let scratch = tempfile::tempdir()?;
        let options = EstimateOptions {
            temporary_parent: Some(scratch.path().to_owned()),
            ..Default::default()
        };
        let token = CancellationToken::new();
        let result = estimate_plan(&plan, &options, &token, &mut |_, _| token.cancel()).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(std::fs::read_dir(scratch.path())?.next().is_none());
        Ok(())
    }
}
