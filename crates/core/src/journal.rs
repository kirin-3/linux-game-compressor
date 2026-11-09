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
    pub member: Option<String>,
    pub plan_hash: String,
    pub input_hash: String,
    pub output_hash: Option<String>,
    pub input_size: u64,
    pub output_size: u64,
    pub outcome: String,
    pub failure: bool,
}

pub fn display_path(path: &StoredPath, member: Option<&str>) -> StoredPath {
    let mut name = path.0.as_os_str().to_os_string();
    if let Some(member) = member {
        name.push("::");
        name.push(member);
    }
    StoredPath(name.into())
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
    let mut latest: HashMap<(StoredPath, Option<String>), JournalEntry> = HashMap::new();
    let plan_hash = fingerprint(&plan);
    for record in records {
        if record.plan_hash == plan_hash {
            latest.insert((record.path.clone(), record.member.clone()), record.clone());
        }
    }
    let entries = plan
        .entries
        .iter()
        .filter_map(|entry| latest.remove(&(entry.path.clone(), entry.member.clone())))
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
) -> HashMap<(StoredPath, Option<String>), JournalEntry> {
    let expected: HashMap<_, _> = plan
        .entries
        .iter()
        .map(|entry| ((entry.path.clone(), entry.member.clone()), &entry.hash))
        .collect();
    let mut latest = HashMap::new();
    let plan_hash = fingerprint(plan);
    for record in records {
        if record.plan_hash == plan_hash
            && expected.contains_key(&(record.path.clone(), record.member.clone()))
        {
            latest.insert((record.path.clone(), record.member.clone()), record.clone());
        }
    }
    // Staged members are removed once their pack is rebuilt; the rebuilt
    // pack's recorded hash then vouches for them.
    let rebuilt_packs: Vec<StoredPath> = latest
        .iter()
        .filter(|((path, member), record)| {
            member.is_none()
                && record.outcome == "rebuilt"
                && expected.get(&(path.clone(), None)).copied() == Some(&record.input_hash)
                && record.output_hash.as_ref().is_some_and(|hash| {
                    hash_input(&output.join(&path.0)).is_ok_and(|(_, actual)| &actual == hash)
                })
        })
        .map(|((path, _), _)| path.clone())
        .collect();
    latest.retain(|key, record| {
        if expected.get(key).copied() != Some(&record.input_hash) {
            return false;
        }
        if key.1.is_some() && !record.outcome.starts_with("converted") {
            return record.output_hash.as_ref() == Some(&record.input_hash);
        }
        if key.1.is_some() && rebuilt_packs.contains(&key.0) {
            return true;
        }
        if record.outcome == "removed" && key.1.is_none() {
            return !output.join(&key.0 .0).exists();
        }
        match &record.output_hash {
            Some(hash) => hash_input(&if key.1.is_some() {
                let index = plan
                    .entries
                    .iter()
                    .position(|entry| entry.path == key.0 && entry.member == key.1)
                    .expect("record belongs to plan");
                output.join(".compressor/members").join(index.to_string())
            } else {
                output.join(&key.0 .0)
            })
            .is_ok_and(|(_, actual)| &actual == hash),
            None => false,
        }
    });
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, EngineInfo, Entry, ResolvedSettings};
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
            member: None,
            plan_hash: String::new(),
            input_hash: "input".into(),
            output_hash: Some(hash),
            input_size: 9,
            output_size: 4,
            outcome: "converted".into(),
            failure: false,
        };
        let plan = Plan {
            plan_version: 6,
            input_root: StoredPath(temp.path().to_owned()),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: ResolvedSettings::default(),
            tools: vec![],
            entries: vec![Entry {
                path: path.clone(),
                member: None,
                size: 9,
                hash: "input".into(),
                action: Action::Copy {
                    reason: "test".into(),
                },
            }],
        };
        record.plan_hash = fingerprint(&plan);
        let mut changed_settings = plan.clone();
        changed_settings
            .settings
            .values
            .get_mut("audio.vorbis_quality")
            .unwrap()
            .value = serde_json::json!(4);
        assert_ne!(fingerprint(&plan), fingerprint(&changed_settings));
        let mut journal = Journal::open(&output.join(".compressor/journal.jsonl"))?;
        journal.append(&record)?;
        let records = journal.records()?;
        assert_eq!(
            make_manifest(plan.clone(), &records, false).entries.len(),
            1
        );
        assert!(resumable_records(&plan, &records, &output).contains_key(&(path.clone(), None)));
        fs::write(output.join(&path.0), b"modified")?;
        assert!(!resumable_records(&plan, &records, &output).contains_key(&(path.clone(), None)));
        Ok(())
    }
    #[test]
    fn resume_distinguishes_members_of_one_pack() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let output = temp.path();
        fs::create_dir_all(output.join(".compressor/members"))?;
        let path = StoredPath("game.pck".into());
        let entries: Vec<_> = ["a", "b"]
            .iter()
            .map(|name| Entry {
                path: path.clone(),
                member: Some((*name).into()),
                size: 5,
                hash: format!("input-{name}"),
                action: Action::Copy {
                    reason: "test".into(),
                },
            })
            .collect();
        let plan = Plan {
            plan_version: 6,
            input_root: StoredPath(output.to_owned()),
            engine: EngineInfo {
                name: "fake".into(),
                version: None,
                excluded_paths: vec![],
                notes: vec![],
                version_check_overridden: false,
            },
            settings: ResolvedSettings::default(),
            tools: vec![],
            entries,
        };
        let mut records = Vec::new();
        for (index, entry) in plan.entries.iter().enumerate() {
            let staged = output.join(".compressor/members").join(index.to_string());
            fs::write(&staged, format!("result-{index}"))?;
            records.push(JournalEntry {
                path: path.clone(),
                member: entry.member.clone(),
                plan_hash: fingerprint(&plan),
                input_hash: entry.hash.clone(),
                output_hash: Some(hash_input(&staged)?.1),
                input_size: 5,
                output_size: 8,
                outcome: "converted".into(),
                failure: false,
            });
        }
        assert_eq!(
            make_manifest(plan.clone(), &records, false).entries.len(),
            2
        );
        assert_eq!(resumable_records(&plan, &records, output).len(), 2);
        fs::write(output.join(".compressor/members/0"), b"tampered")?;
        let resumed = resumable_records(&plan, &records, output);
        assert_eq!(resumed.len(), 1);
        assert!(resumed.contains_key(&(path, Some("b".into()))));
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
pub struct Removal {
    pub path: StoredPath,
    pub reason: String,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveSummary {
    pub path: StoredPath,
    pub member_count: usize,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub outcome: String,
}

/// The combined sizes of a rewritten pair, reported under the primary file's path.
#[derive(Debug, Serialize, Deserialize)]
pub struct RewriteSummary {
    pub path: StoredPath,
    pub partner: StoredPath,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub outcome: String,
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
    pub removals: Vec<Removal>,
    pub archives: Vec<ArchiveSummary>,
    #[serde(default)]
    pub rewrites: Vec<RewriteSummary>,
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
        .map(|entry| display_path(&entry.path, entry.member.as_deref()))
        .collect();
    let pending = manifest
        .plan
        .entries
        .iter()
        .filter(|entry| {
            !manifest
                .entries
                .iter()
                .any(|record| record.path == entry.path && record.member == entry.member)
        })
        .map(|entry| display_path(&entry.path, entry.member.as_deref()))
        .collect();
    let mut savings_by_media = BTreeMap::new();
    let mut removals = Vec::new();
    let mut skips = Vec::new();
    let mut failures = Vec::new();
    let mut archives = Vec::new();
    let mut rewrites = Vec::new();
    for record in &manifest.entries {
        let mut display_record = record.clone();
        display_record.path = display_path(&record.path, record.member.as_deref());
        if record.failure {
            failures.push(display_record);
        } else if !record.outcome.starts_with("converted")
            && record.outcome != "rebuilt"
            && record.outcome != "removed"
            && record.outcome != "archived"
            && record.outcome != "rewritten"
        {
            skips.push(display_record);
        }
        if let Some(entry) = manifest
            .plan
            .entries
            .iter()
            .find(|entry| entry.path == record.path && entry.member == record.member)
        {
            if let crate::Action::Assemble { members, .. } = &entry.action {
                archives.push(ArchiveSummary {
                    path: entry.path.clone(),
                    member_count: members.len(),
                    input_bytes: entry.size,
                    output_bytes: record.output_size,
                    outcome: record.outcome.clone(),
                });
                if record.outcome == "archived" {
                    *savings_by_media.entry("data".to_owned()).or_insert(0) +=
                        i128::from(entry.size) - i128::from(record.output_size);
                }
            }
            if let crate::Action::Rewrite { partner, .. } = &entry.action {
                if let Some(other) = manifest
                    .entries
                    .iter()
                    .find(|r| r.member.is_none() && r.path == *partner)
                {
                    rewrites.push(RewriteSummary {
                        path: entry.path.clone(),
                        partner: partner.clone(),
                        input_bytes: record.input_size + other.input_size,
                        output_bytes: record.output_size + other.output_size,
                        outcome: record.outcome.clone(),
                    });
                    if record.outcome == "rewritten" {
                        *savings_by_media.entry("data".to_owned()).or_insert(0) +=
                            i128::from(record.input_size + other.input_size)
                                - i128::from(record.output_size + other.output_size);
                    }
                }
            }
            if let crate::Action::Remove { reason } = &entry.action {
                if record.outcome == "removed" {
                    removals.push(Removal {
                        path: display_path(&record.path, record.member.as_deref()),
                        reason: reason.clone(),
                        size: record.input_size,
                    });
                }
            }
            let pack_failed = record.member.is_some()
                && manifest
                    .entries
                    .iter()
                    .any(|pack| pack.path == record.path && pack.member.is_none() && pack.failure);
            if let crate::Action::Convert { kind, .. } = &entry.action {
                if pack_failed {
                    continue;
                }
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
        input_bytes: manifest
            .entries
            .iter()
            .filter(|e| e.member.is_none())
            .map(|e| e.input_size)
            .sum(),
        output_bytes: manifest
            .entries
            .iter()
            .filter(|e| e.member.is_none())
            .map(|e| e.output_size)
            .sum(),
        savings_by_media,
        removals,
        archives,
        rewrites,
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
