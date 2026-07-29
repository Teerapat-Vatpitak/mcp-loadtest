//! Crash-bounded, merge-friendly file store for history samples.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::{HistoryError, HistorySampleV1, validate_run_id, validate_series_name};

const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_SAMPLES: usize = 10_000;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Result of recording one history sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// A new sample file was created.
    Created,
    /// The same run and identical content were already present.
    AlreadyPresent,
}

/// Directory-backed history store.
///
/// Each run is one JSON file. Unlike an append-only JSONL file, independent
/// machine artifacts can be merged by copying files and one interrupted write
/// cannot corrupt every prior sample.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    root: PathBuf,
    max_file_bytes: u64,
    max_samples: usize,
}

impl HistoryStore {
    /// Construct a store with 1 MiB per-file and 10,000-sample scan limits.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_samples: DEFAULT_MAX_SAMPLES,
        }
    }

    /// Override defensive read limits.
    #[must_use]
    pub fn with_limits(mut self, max_file_bytes: u64, max_samples: usize) -> Self {
        self.max_file_bytes = max_file_bytes;
        self.max_samples = max_samples;
        self
    }

    /// Store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load and validate one benchmark series.
    ///
    /// Duplicate run ids with byte-equivalent logical samples are collapsed;
    /// conflicting duplicates fail closed.
    pub fn load(&self, series: &str) -> Result<Vec<HistorySampleV1>, HistoryError> {
        self.validate_limits()?;
        validate_series_name(series)?;
        let directory = self.series_dir(series);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(HistoryError::Io {
                    operation: "read directory",
                    path: directory,
                    source,
                });
            }
        };

        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| HistoryError::Io {
                operation: "enumerate directory",
                path: directory.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| HistoryError::Io {
                operation: "inspect sample",
                path: entry.path(),
                source,
            })?;
            if file_type.is_symlink() {
                return Err(HistoryError::InvalidSample(
                    "symbolic links are forbidden in the history store",
                ));
            }
            if file_type.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            {
                files.push(entry.path());
            }
            if files.len() > self.max_samples {
                return Err(HistoryError::TooManySamples);
            }
        }
        files.sort();

        let mut by_run_id: BTreeMap<String, HistorySampleV1> = BTreeMap::new();
        for path in files {
            let sample = self.read_sample(&path)?;
            sample.validate()?;
            if sample.series != series {
                return Err(HistoryError::InvalidSample(
                    "sample series does not match its store directory",
                ));
            }
            match by_run_id.get(&sample.run_id) {
                Some(existing) if existing == &sample => {}
                Some(_) => return Err(HistoryError::ConflictingDuplicate),
                None => {
                    by_run_id.insert(sample.run_id.clone(), sample);
                }
            }
        }

        let mut samples: Vec<HistorySampleV1> = by_run_id.into_values().collect();
        samples.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        Ok(samples)
    }

    /// Record a sample using create-new semantics.
    ///
    /// If another process wins the same run-id race, its complete sample is
    /// read and compared. Identical content is idempotent; different content
    /// is a conflict.
    pub fn record(&self, sample: &HistorySampleV1) -> Result<RecordOutcome, HistoryError> {
        self.validate_limits()?;
        sample.validate()?;
        validate_run_id(&sample.run_id)?;
        let directory = self.series_dir(&sample.series);
        fs::create_dir_all(&directory).map_err(|source| HistoryError::Io {
            operation: "create directory",
            path: directory.clone(),
            source,
        })?;
        let path = directory.join(format!("{}.json", sample.run_id));

        let mut encoded =
            serde_json::to_vec_pretty(sample).map_err(|source| HistoryError::Json {
                path: path.clone(),
                source,
            })?;
        encoded.push(b'\n');
        if encoded.len() as u64 > self.max_file_bytes {
            return Err(HistoryError::SampleTooLarge);
        }

        if path.exists() {
            return self.compare_existing(&path, sample);
        }

        // Write a complete same-directory temporary file, then publish it
        // with an atomic no-overwrite hard link. Readers never observe a
        // partial JSON target, and concurrent writers cannot replace one
        // another. A crash can leave only an ignored `.tmp` file.
        let temporary = self.unique_temporary_path(&directory, &sample.run_id);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| HistoryError::Io {
                operation: "create temporary sample",
                path: temporary.clone(),
                source,
            })?;
        if let Err(source) = file.write_all(&encoded).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(HistoryError::Io {
                operation: "write temporary sample",
                path: temporary,
                source,
            });
        }
        drop(file);

        let publish = fs::hard_link(&temporary, &path);
        let _ = fs::remove_file(&temporary);
        match publish {
            Ok(()) => Ok(RecordOutcome::Created),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                self.compare_existing(&path, sample)
            }
            Err(source) => Err(HistoryError::Io {
                operation: "publish sample",
                path,
                source,
            }),
        }
    }

    fn validate_limits(&self) -> Result<(), HistoryError> {
        if self.max_file_bytes == 0 {
            return Err(HistoryError::InvalidPolicy(
                "history max_file_bytes must be greater than zero",
            ));
        }
        if self.max_samples == 0 {
            return Err(HistoryError::InvalidPolicy(
                "history max_samples must be greater than zero",
            ));
        }
        Ok(())
    }

    fn series_dir(&self, series: &str) -> PathBuf {
        self.root.join(series)
    }

    fn unique_temporary_path(&self, directory: &Path, run_id: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        directory.join(format!(
            ".{run_id}.{}.{}.{}.tmp",
            std::process::id(),
            nanos,
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn compare_existing(
        &self,
        path: &Path,
        sample: &HistorySampleV1,
    ) -> Result<RecordOutcome, HistoryError> {
        let existing = self.read_sample(path)?;
        existing.validate()?;
        if &existing == sample {
            Ok(RecordOutcome::AlreadyPresent)
        } else {
            Err(HistoryError::ConflictingDuplicate)
        }
    }

    fn read_sample(&self, path: &Path) -> Result<HistorySampleV1, HistoryError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| HistoryError::Io {
            operation: "inspect sample",
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(HistoryError::InvalidSample(
                "symbolic links are forbidden in the history store",
            ));
        }
        if metadata.len() > self.max_file_bytes {
            return Err(HistoryError::SampleTooLarge);
        }
        let mut file =
            OpenOptions::new()
                .read(true)
                .open(path)
                .map_err(|source| HistoryError::Io {
                    operation: "open sample",
                    path: path.to_path_buf(),
                    source,
                })?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::io::Read::by_ref(&mut file)
            .take(self.max_file_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| HistoryError::Io {
                operation: "read sample",
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() as u64 > self.max_file_bytes {
            return Err(HistoryError::SampleTooLarge);
        }
        serde_json::from_slice(&bytes).map_err(|source| HistoryError::Json {
            path: path.to_path_buf(),
            source,
        })
    }
}
