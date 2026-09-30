//! Crash-recoverable, single-file copies. A job owns its staging file and only
//! publishes a complete file after revalidating the source and destination.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;

const CHUNK: usize = 1024 * 1024;
const CHECKPOINT: u64 = 8 * 1024 * 1024;
static ACTIVE: LazyLock<Mutex<HashSet<(PathBuf, u64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct ActiveTransfer((PathBuf, u64));

impl ActiveTransfer {
    fn acquire(dir: &Path, id: u64) -> Result<Self, String> {
        let key = (dir.to_path_buf(), id);
        let mut active = ACTIVE.lock().map_err(|error| error.to_string())?;
        if !active.insert(key.clone()) {
            return Err("Transfer is already running".into());
        }
        Ok(Self(key))
    }
}

impl Drop for ActiveTransfer {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            active.remove(&self.0);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPlan {
    pub id: u64,
    #[serde(default = "copy_kind")]
    pub kind: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub stage: PathBuf,
    pub source_len: u64,
    pub source_modified_ns: u128,
    pub destination_existed: bool,
    pub bytes_done: u64,
    pub status: String,
    pub error: String,
}

fn copy_kind() -> String {
    "copy".into()
}

fn modified_ns(meta: &fs::Metadata) -> Result<u128, String> {
    meta.modified()
        .and_then(|time| {
            time.duration_since(UNIX_EPOCH)
                .map_err(std::io::Error::other)
        })
        .map(|elapsed| elapsed.as_nanos())
        .map_err(|error| error.to_string())
}

fn is_efs_encrypted(path: &Path) -> Result<bool, String> {
    #[cfg(windows)]
    {
        let meta = fs::metadata(path).map_err(|error| error.to_string())?;
        Ok(
            meta.file_attributes()
                & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ENCRYPTED.0
                != 0,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(windows)]
fn encrypt_stage(stage: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::EncryptFileW;
    use windows::core::PCWSTR;
    let wide: Vec<u16> = stage.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { EncryptFileW(PCWSTR(wide.as_ptr())) }
        .map_err(|error| format!("Cannot protect EFS transfer stage: {error}"))?;
    if !is_efs_encrypted(stage)? {
        return Err("EFS transfer stage is not encrypted".into());
    }
    Ok(())
}

fn plan_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("transfer-{id}.json"))
}

fn persist(dir: &Path, plan: &TransferPlan) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(plan).map_err(|error| error.to_string())?;
    super::atomic_json_file(&plan_path(dir, plan.id), &bytes)
}

pub fn load(dir: &Path, id: u64) -> Result<TransferPlan, String> {
    let bytes = fs::read(plan_path(dir, id)).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

pub fn list(dir: &Path) -> Vec<TransferPlan> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut plans: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("transfer-") && name.ends_with(".json")
        })
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<TransferPlan>(&bytes).ok())
        .collect();
    plans.sort_by_key(|plan| plan.id);
    plans
}

pub fn start(
    dir: &Path,
    id: u64,
    source: &Path,
    destination: &Path,
) -> Result<TransferPlan, String> {
    start_with_kind(dir, id, source, destination, "copy")
}

pub fn start_move(
    dir: &Path,
    id: u64,
    source: &Path,
    destination: &Path,
) -> Result<TransferPlan, String> {
    start_with_kind(dir, id, source, destination, "move")
}

fn start_with_kind(
    dir: &Path,
    id: u64,
    source: &Path,
    destination: &Path,
    kind: &str,
) -> Result<TransferPlan, String> {
    if source == destination || destination.exists() {
        return Err("Destination changed or already exists; choose a new destination".into());
    }
    let meta = fs::metadata(source).map_err(|error| error.to_string())?;
    if !meta.is_file() {
        return Err("Only regular files can use resumable copy".into());
    }
    let parent = destination.parent().ok_or("Destination has no parent")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let stage = parent.join(format!(
        ".{}.pathfinder-{id}.part",
        destination
            .file_name()
            .ok_or("Destination has no name")?
            .to_string_lossy()
    ));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)
        .map_err(|error| format!("Cannot reserve transfer stage: {error}"))?;
    #[cfg(windows)]
    if meta.file_attributes() & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ENCRYPTED.0 != 0
    {
        if let Err(error) = encrypt_stage(&stage) {
            let _ = fs::remove_file(&stage);
            return Err(error);
        }
    }
    let plan = TransferPlan {
        id,
        kind: kind.into(),
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        stage,
        source_len: meta.len(),
        source_modified_ns: modified_ns(&meta)?,
        destination_existed: false,
        bytes_done: 0,
        status: "interrupted".into(),
        error: String::new(),
    };
    if let Err(error) = persist(dir, &plan) {
        let _ = fs::remove_file(&plan.stage);
        return Err(error);
    }
    Ok(plan)
}

pub fn finish_move(dir: &Path, id: u64) -> Result<(), String> {
    let _active = ActiveTransfer::acquire(dir, id)?;
    let plan = load(dir, id)?;
    if plan.kind != "move" || plan.status != "done" {
        return Err("Move copy has not been verified and published".into());
    }
    if !plan.source.exists() {
        return Ok(());
    }
    validate_source_fingerprint(&plan)?;
    if !files_equal(&plan.source, &plan.destination)? {
        return Err("Move destination differs from source; source was preserved".into());
    }
    fs::remove_file(&plan.source).map_err(|error| error.to_string())
}

fn validate_source(plan: &TransferPlan) -> Result<(), String> {
    let meta = fs::metadata(&plan.source).map_err(|error| error.to_string())?;
    if !meta.is_file()
        || meta.len() != plan.source_len
        || modified_ns(&meta)? != plan.source_modified_ns
    {
        return Err("Source changed since transfer began; restart with a new plan".into());
    }
    if plan.destination.exists() {
        return Err("Destination now exists; choose a new destination".into());
    }
    Ok(())
}

fn verify_prefix(plan: &TransferPlan) -> Result<u64, String> {
    let mut stage_len = fs::metadata(&plan.stage)
        .map_err(|error| error.to_string())?
        .len();
    if stage_len > plan.bytes_done {
        OpenOptions::new()
            .write(true)
            .open(&plan.stage)
            .and_then(|file| file.set_len(plan.bytes_done))
            .map_err(|error| error.to_string())?;
        stage_len = plan.bytes_done;
    }
    if stage_len > plan.source_len {
        return Err("Staged copy exceeds source length".into());
    }
    let mut source = File::open(&plan.source).map_err(|error| error.to_string())?;
    let mut stage = File::open(&plan.stage).map_err(|error| error.to_string())?;
    let mut source_hash = Sha256::new();
    let mut stage_hash = Sha256::new();
    let mut left = stage_len;
    let mut source_buf = vec![0; CHUNK];
    let mut stage_buf = vec![0; CHUNK];
    while left > 0 {
        let take = left.min(CHUNK as u64) as usize;
        source
            .read_exact(&mut source_buf[..take])
            .map_err(|error| error.to_string())?;
        stage
            .read_exact(&mut stage_buf[..take])
            .map_err(|error| error.to_string())?;
        source_hash.update(&source_buf[..take]);
        stage_hash.update(&stage_buf[..take]);
        left -= take as u64;
    }
    if source_hash.finalize() != stage_hash.finalize() {
        return Err("Staged bytes differ from source; restart with a new plan".into());
    }
    Ok(stage_len)
}

pub fn run(
    dir: &Path,
    id: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<(), String> {
    let _active = ActiveTransfer::acquire(dir, id)?;
    let mut plan = load(dir, id)?;
    if plan.status == "done" {
        return Ok(());
    }
    // A crash can happen between publishing the complete file and recording
    // success. Reconcile that phase from the actual bytes before retrying.
    if !plan.stage.exists() && plan.destination.is_file() {
        validate_source_fingerprint(&plan)?;
        if is_efs_encrypted(&plan.source)? && !is_efs_encrypted(&plan.destination)? {
            return Err(
                "Encrypted source has an unencrypted destination; source was preserved".into(),
            );
        }
        if files_equal(&plan.source, &plan.destination)? {
            plan.status = "done".into();
            plan.error.clear();
            plan.bytes_done = plan.source_len;
            persist(dir, &plan)?;
            return Ok(());
        }
    }
    let result = (|| -> Result<(), String> {
        validate_source(&plan)?;
        if is_efs_encrypted(&plan.source)? && !is_efs_encrypted(&plan.stage)? {
            return Err(
                "Encrypted source has an unencrypted transfer stage; copy was stopped".into(),
            );
        }
        let offset = verify_prefix(&plan)?;
        let mut source = File::open(&plan.source).map_err(|error| error.to_string())?;
        let mut stage = OpenOptions::new()
            .write(true)
            .open(&plan.stage)
            .map_err(|error| error.to_string())?;
        source
            .seek(SeekFrom::Start(offset))
            .map_err(|error| error.to_string())?;
        stage
            .seek(SeekFrom::Start(offset))
            .map_err(|error| error.to_string())?;
        plan.status = "running".into();
        plan.bytes_done = offset;
        persist(dir, &plan)?;
        progress(offset);
        let mut buffer = vec![0; CHUNK];
        let mut since_checkpoint = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Operation cancelled".into());
            }
            let count = source
                .read(&mut buffer)
                .map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            stage
                .write_all(&buffer[..count])
                .map_err(|error| error.to_string())?;
            plan.bytes_done += count as u64;
            since_checkpoint += count as u64;
            progress(plan.bytes_done);
            if since_checkpoint >= CHECKPOINT {
                stage.sync_data().map_err(|error| error.to_string())?;
                persist(dir, &plan)?;
                since_checkpoint = 0;
            }
        }
        stage.sync_all().map_err(|error| error.to_string())?;
        persist(dir, &plan)?;
        validate_source(&plan)?;
        if is_efs_encrypted(&plan.source)? && !is_efs_encrypted(&plan.stage)? {
            return Err(
                "Encrypted source lost stage encryption; destination was not published".into(),
            );
        }
        // rename cannot replace a changed target: the existing-target check is
        // repeated immediately before commit. Other processes can still race;
        // Windows rename fails when the target appears, which is safe.
        super::rename_file_noreplace(&plan.stage, &plan.destination)?;
        plan.status = "done".into();
        plan.error.clear();
        persist(dir, &plan)?;
        Ok(())
    })();
    if let Err(error) = &result {
        plan.status = if error == "Operation cancelled" {
            "cancelled"
        } else {
            "failed"
        }
        .into();
        plan.error = error.clone();
        let _ = persist(dir, &plan);
    }
    result
}

fn validate_source_fingerprint(plan: &TransferPlan) -> Result<(), String> {
    let meta = fs::metadata(&plan.source).map_err(|error| error.to_string())?;
    if !meta.is_file()
        || meta.len() != plan.source_len
        || modified_ns(&meta)? != plan.source_modified_ns
    {
        return Err("Source changed since transfer began; restart with a new plan".into());
    }
    Ok(())
}

fn files_equal(a: &Path, b: &Path) -> Result<bool, String> {
    let a_meta = fs::metadata(a).map_err(|error| error.to_string())?;
    let b_meta = fs::metadata(b).map_err(|error| error.to_string())?;
    if a_meta.len() != b_meta.len() {
        return Ok(false);
    }
    let hash = |path: &Path| -> Result<_, String> {
        let mut file = File::open(path).map_err(|error| error.to_string())?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; CHUNK];
        loop {
            let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        Ok(hasher.finalize())
    };
    Ok(hash(a)? == hash(b)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn fixture() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pathfinder-transfer-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn resumes_verified_stage_and_keeps_destination_atomic() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("destination.bin");
        let body = vec![31u8; 2 * CHUNK + 17];
        fs::write(&source, &body).unwrap();
        let mut plan = start(&root.join("plans"), 1, &source, &destination).unwrap();
        fs::write(&plan.stage, &body[..CHUNK]).unwrap();
        plan.bytes_done = CHUNK as u64;
        persist(&root.join("plans"), &plan).unwrap();
        assert!(!destination.exists());
        run(&root.join("plans"), 1, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), body);
        assert!(!plan.stage.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_modified_source_and_tampered_checkpoint() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("destination.bin");
        fs::write(&source, vec![1u8; CHUNK]).unwrap();
        let mut plan = start(&root.join("plans"), 2, &source, &destination).unwrap();
        fs::write(&plan.stage, vec![2u8; CHUNK]).unwrap();
        plan.bytes_done = CHUNK as u64;
        persist(&root.join("plans"), &plan).unwrap();
        assert!(run(&root.join("plans"), 2, &AtomicBool::new(false), |_| {}).is_err());
        assert!(!destination.exists());
        fs::write(&source, vec![3u8; CHUNK + 1]).unwrap();
        assert!(run(&root.join("plans"), 2, &AtomicBool::new(false), |_| {}).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn move_keeps_source_until_verified_copy_is_published() {
        let root = fixture();
        let plans = root.join("plans");
        let source = root.join("source.bin");
        let destination = root.join("destination.bin");
        fs::write(&source, b"move safely").unwrap();
        start_move(&plans, 3, &source, &destination).unwrap();
        run(&plans, 3, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"move safely");
        finish_move(&plans, 3).unwrap();
        assert!(!source.exists());
        finish_move(&plans, 3).unwrap();
        let _ = fs::remove_dir_all(root);
    }
}
