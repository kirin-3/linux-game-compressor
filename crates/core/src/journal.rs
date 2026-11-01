use crate::{planning::hash_input, Manifest, Plan, StoredPath};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalEntry {
    pub path: StoredPath,
    pub plan_hash: String,
    pub input_hash: String,
    pub output_hash: Option<String>,
    pub input_size: u64,
    pub output_size: u64,
    pub outcome: String,
    pub failure: bool,
}

pub struct Journal {
    path: PathBuf,
    file: File,
}
impl Journal {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            let last_complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            file.set_len(last_complete as u64)?;
            file.sync_all()?;
        }
        Ok(Self {
            path: path.to_owned(),
            file,
        })
    }
    pub fn append(&mut self, entry: &JournalEntry) -> io::Result<()> {
        let mut json = serde_json::to_vec(entry)?;
        json.push(b'\n');
        self.file.write_all(&json)?;
        self.file.sync_all()
    }
    pub fn records(&self) -> io::Result<Vec<JournalEntry>> {
        let content = fs::read(&self.path)?;
        content
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).map_err(io::Error::other))
            .collect()
    }
}

pub fn fingerprint(plan: &Plan) -> String {
    blake3::hash(&serde_json::to_vec(plan).expect("plan serialization is infallible"))
        .to_hex()
        .to_string()
}

/// Keep the last synced completion for each path and reconstruct a manifest.
pub fn make_manifest(plan: Plan, records: &[JournalEntry], complete: bool) -> Manifest {
    let mut latest: HashMap<StoredPath, JournalEntry> = HashMap::new();
    let plan_hash = fingerprint(&plan);
    for record in records {
        if record.plan_hash == plan_hash {
            latest.insert(record.path.clone(), record.clone());
        }
    }
    let entries = plan
        .entries
        .iter()
        .filter_map(|entry| latest.remove(&entry.path))
        .collect();
    Manifest {
        plan,
        complete,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        tool_versions: Vec::new(),
        entries,
    }
}

/// Trust a completion only if the current output still has the recorded hash.
pub fn resumable_records(
    plan: &Plan,
    records: &[JournalEntry],
    output: &Path,
) -> HashMap<StoredPath, JournalEntry> {
    let expected: HashMap<_, _> = plan
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), &entry.hash))
        .collect();
    let mut latest = HashMap::new();
    let plan_hash = fingerprint(plan);
    for record in records {
        if record.plan_hash == plan_hash && expected.contains_key(&record.path) {
            latest.insert(record.path.clone(), record.clone());
        }
    }
    latest.retain(|path, record| {
        if expected.get(path).copied() != Some(&record.input_hash) {
            return false;
        }
        match &record.output_hash {
            Some(hash) => {
                hash_input(&output.join(&path.0)).is_ok_and(|(_, actual)| &actual == hash)
            }
            None => false,
        }
    });
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, EngineInfo, Entry, Profile};
    #[test]
    fn resumes_synced_records_and_redoes_tampered_output() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("out");
        fs::create_dir(&output)?;
        let path = StoredPath("x.png".into());
        fs::write(output.join(&path.0), b"done")?;
        let (_, hash) = hash_input(&output.join(&path.0))?;
        let mut record = JournalEntry {
            path: path.clone(),
            plan_hash: String::new(),
            input_hash: "input".into(),
            output_hash: Some(hash),
            input_size: 9,
            output_size: 4,
            outcome: "converted".into(),
            failure: false,
        };
        let plan = Plan {
            plan_version: 1,
            input_root: StoredPath(temp.path().to_owned()),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            profile: Profile::default(),
            tools: vec![],
            entries: vec![Entry {
                path: path.clone(),
                size: 9,
                hash: "input".into(),
                action: Action::Copy {
                    reason: "test".into(),
                },
            }],
        };
        record.plan_hash = fingerprint(&plan);
        let mut journal = Journal::open(&output.join(".compressor/journal.jsonl"))?;
        journal.append(&record)?;
        let records = journal.records()?;
        assert_eq!(
            make_manifest(plan.clone(), &records, false).entries.len(),
            1
        );
        assert!(resumable_records(&plan, &records, &output).contains_key(&path));
        fs::write(output.join(&path.0), b"modified")?;
        assert!(!resumable_records(&plan, &records, &output).contains_key(&path));
        Ok(())
    }
    #[test]
    fn truncates_torn_last_record() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("journal");
        fs::write(&path, b"{broken")?;
        let journal = Journal::open(&path)?;
        assert!(journal.records()?.is_empty());
        assert_eq!(fs::metadata(path)?.len(), 0);
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub complete: bool,
    pub notes: Vec<String>,
    pub completed: Vec<StoredPath>,
    pub pending: Vec<StoredPath>,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub savings_by_media: BTreeMap<String, i128>,
    pub skips: Vec<JournalEntry>,
    pub failures: Vec<JournalEntry>,
}

/// Remove partial stages and persist an incomplete, resumable snapshot.
pub fn finish_cancelled(plan: &Plan, output: &Path, journal: &Journal) -> io::Result<()> {
    let metadata = output.join(".compressor");
    let temporary = metadata.join("tmp");
    if temporary.exists() {
        for entry in fs::read_dir(&temporary)? {
            let path = entry?.path();
            if fs::symlink_metadata(&path)?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
    }
    let records = journal.records()?;
    let manifest = make_manifest(plan.clone(), &records, false);
    let report = build_report(&manifest);
    write_json_atomic(&metadata.join("manifest.json"), &manifest)?;
    write_json_atomic(&metadata.join("report.json"), &report)
}

pub fn build_report(manifest: &Manifest) -> Report {
    let completed: Vec<_> = manifest
        .entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    let pending = manifest
        .plan
        .entries
        .iter()
        .filter(|entry| !completed.contains(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();
    let mut savings_by_media = BTreeMap::new();
    let mut skips = Vec::new();
    let mut failures = Vec::new();
    for record in &manifest.entries {
        if record.failure {
            failures.push(record.clone());
        } else if !record.outcome.starts_with("converted") {
            skips.push(record.clone());
        }
        if let Some(entry) = manifest
            .plan
            .entries
            .iter()
            .find(|entry| entry.path == record.path)
        {
            if let crate::Action::Convert { kind, .. } = &entry.action {
                *savings_by_media
                    .entry(format!("{kind:?}").to_lowercase())
                    .or_insert(0) += i128::from(record.input_size) - i128::from(record.output_size);
            }
        }
    }
    let mut notes = manifest.plan.engine.notes.clone();
    if manifest.plan.engine.version_check_overridden {
        notes.push("engine version check overridden by user".into());
    }
    Report {
        complete: manifest.complete,
        notes,
        completed,
        pending,
        input_bytes: manifest.entries.iter().map(|e| e.input_size).sum(),
        output_bytes: manifest.entries.iter().map(|e| e.output_size).sum(),
        savings_by_media,
        skips,
        failures,
    }
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = File::create(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temp, path)
}
