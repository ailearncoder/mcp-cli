#![cfg(unix)]
//! Atomic, bounded, and owner-validated Unix daemon PID metadata.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::ConfigHash;

use super::paths::{ArtifactKind, DaemonPathError, DaemonPaths, private_file_mode};

/// Hard ceiling for PID metadata, including malformed input.
pub const PID_METADATA_MAX_BYTES: usize = 16 * 1024;
const TEMP_CREATE_ATTEMPTS: usize = 32;
const RANDOM_NONCE_BYTES: usize = 16;

/// Persisted daemon process identity. Unknown JSON fields are rejected so the
/// metadata key set can never silently grow to include configuration secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PidMetadata {
    pub pid: u32,
    #[serde(with = "config_hash_hex")]
    pub config_hash: ConfigHash,
    pub started_at: SystemTime,
}

/// Errors from secure metadata publication, loading, and removal.
#[derive(Debug, Error)]
pub enum MetadataError {
    #[error(transparent)]
    UnsafePath(#[from] DaemonPathError),
    #[error("could not {operation} PID metadata {path}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("PID metadata exceeds {limit} bytes")]
    TooLarge { limit: usize },
    #[error("PID metadata is not valid JSON")]
    InvalidJson(#[source] serde_json::Error),
}

impl MetadataError {
    fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

/// Secure storage for one server's PID metadata file.
#[derive(Clone, Debug)]
pub struct MetadataStore {
    paths: DaemonPaths,
}

impl MetadataStore {
    pub fn new(paths: DaemonPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    /// Atomically publishes metadata using an unpredictable same-directory
    /// `create_new` temporary file with mode 0600, followed by sync + rename.
    pub fn write(&self, metadata: &PidMetadata) -> Result<(), MetadataError> {
        self.write_with_before_rename(metadata, || Ok(()))
    }

    fn write_with_before_rename<F>(
        &self,
        metadata: &PidMetadata,
        before_rename: F,
    ) -> Result<(), MetadataError>
    where
        F: FnOnce() -> io::Result<()>,
    {
        self.paths.validate_runtime_dir()?;
        self.paths.validate_pid_path()?;
        self.validate_existing_target()?;

        let encoded = serde_json::to_vec(metadata).map_err(MetadataError::InvalidJson)?;
        if encoded.len() > PID_METADATA_MAX_BYTES {
            return Err(MetadataError::TooLarge {
                limit: PID_METADATA_MAX_BYTES,
            });
        }

        let (mut temporary, temporary_path) = self.create_temporary_file()?;
        let mut cleanup = TemporaryFileCleanup::new(temporary_path.clone());
        temporary
            .set_permissions(fs::Permissions::from_mode(private_file_mode()))
            .map_err(|source| MetadataError::io("set permissions on", &temporary_path, source))?;
        temporary
            .write_all(&encoded)
            .map_err(|source| MetadataError::io("write", &temporary_path, source))?;
        temporary
            .flush()
            .map_err(|source| MetadataError::io("flush", &temporary_path, source))?;
        temporary
            .sync_all()
            .map_err(|source| MetadataError::io("sync", &temporary_path, source))?;
        let temporary_metadata = temporary
            .metadata()
            .map_err(|source| MetadataError::io("inspect", &temporary_path, source))?;
        self.paths.validate_artifact_metadata(
            &temporary_path,
            &temporary_metadata,
            ArtifactKind::RegularFile,
            true,
        )?;

        before_rename().map_err(|source| {
            MetadataError::io("prepare atomic rename for", &temporary_path, source)
        })?;
        self.paths.validate_runtime_dir()?;
        self.validate_existing_target()?;
        fs::rename(&temporary_path, &self.paths.pid)
            .map_err(|source| MetadataError::io("atomically rename", &self.paths.pid, source))?;
        cleanup.disarm();

        let published = fs::symlink_metadata(&self.paths.pid)
            .map_err(|source| MetadataError::io("inspect", &self.paths.pid, source))?;
        self.paths.validate_artifact_metadata(
            &self.paths.pid,
            &published,
            ArtifactKind::RegularFile,
            true,
        )?;
        if published.dev() != temporary_metadata.dev()
            || published.ino() != temporary_metadata.ino()
            || published.mode() & 0o7777 != private_file_mode()
        {
            return Err(DaemonPathError::unsafe_path(
                &self.paths.pid,
                "published PID metadata did not retain the validated temporary file identity",
            )
            .into());
        }

        File::open(&self.paths.runtime_dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| {
                MetadataError::io("sync runtime directory for", &self.paths.pid, source)
            })?;
        Ok(())
    }

    /// Reads and parses metadata without following symlinks. The opened file's
    /// inode is compared before and after the bounded read to detect swaps.
    pub fn read(&self) -> Result<PidMetadata, MetadataError> {
        self.read_with_identity().map(|(metadata, _)| metadata)
    }

    /// Reads metadata and returns the exact validated inode identity used for
    /// later race-resistant cleanup.
    pub(crate) fn read_with_identity(
        &self,
    ) -> Result<(PidMetadata, super::paths::ArtifactIdentity), MetadataError> {
        self.paths.validate_runtime_dir()?;
        self.paths.validate_pid_path()?;
        let before = fs::symlink_metadata(&self.paths.pid)
            .map_err(|source| MetadataError::io("inspect", &self.paths.pid, source))?;
        self.paths.validate_artifact_metadata(
            &self.paths.pid,
            &before,
            ArtifactKind::RegularFile,
            true,
        )?;
        if before.len() > PID_METADATA_MAX_BYTES as u64 {
            return Err(MetadataError::TooLarge {
                limit: PID_METADATA_MAX_BYTES,
            });
        }

        let file = File::open(&self.paths.pid)
            .map_err(|source| MetadataError::io("open", &self.paths.pid, source))?;
        let opened = file
            .metadata()
            .map_err(|source| MetadataError::io("inspect opened", &self.paths.pid, source))?;
        self.paths.validate_artifact_metadata(
            &self.paths.pid,
            &opened,
            ArtifactKind::RegularFile,
            true,
        )?;
        ensure_same_file(&self.paths.pid, &before, &opened)?;

        let mut bytes = Vec::with_capacity((before.len() as usize).min(PID_METADATA_MAX_BYTES));
        file.take((PID_METADATA_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| MetadataError::io("read", &self.paths.pid, source))?;
        if bytes.len() > PID_METADATA_MAX_BYTES {
            return Err(MetadataError::TooLarge {
                limit: PID_METADATA_MAX_BYTES,
            });
        }

        let after = fs::symlink_metadata(&self.paths.pid)
            .map_err(|source| MetadataError::io("reinspect", &self.paths.pid, source))?;
        self.paths.validate_artifact_metadata(
            &self.paths.pid,
            &after,
            ArtifactKind::RegularFile,
            true,
        )?;
        ensure_same_file(&self.paths.pid, &opened, &after)?;
        let metadata = serde_json::from_slice(&bytes).map_err(MetadataError::InvalidJson)?;
        Ok((
            metadata,
            super::paths::ArtifactIdentity::from_metadata(&after),
        ))
    }

    /// Removes metadata only after revalidating path, type, owner, mode, and
    /// runtime directory identity. Missing metadata is a successful no-op.
    pub fn remove(&self) -> Result<bool, MetadataError> {
        self.paths.remove_pid().map_err(MetadataError::from)
    }

    fn validate_existing_target(&self) -> Result<(), MetadataError> {
        match fs::symlink_metadata(&self.paths.pid) {
            Ok(metadata) => self
                .paths
                .validate_artifact_metadata(
                    &self.paths.pid,
                    &metadata,
                    ArtifactKind::RegularFile,
                    true,
                )
                .map_err(MetadataError::from),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(MetadataError::io("inspect", &self.paths.pid, source)),
        }
    }

    fn create_temporary_file(&self) -> Result<(File, PathBuf), MetadataError> {
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let nonce = random_nonce()?;
            let path = self
                .paths
                .runtime_dir
                .join(format!(".{}.tmp-{nonce}", pid_basename(&self.paths.pid)?));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(private_file_mode())
                .open(&path)
            {
                Ok(file) => return Ok((file, path)),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(MetadataError::io("create temporary", path, source)),
            }
        }
        Err(MetadataError::io(
            "create unique temporary",
            &self.paths.pid,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary name collision limit reached",
            ),
        ))
    }
}

fn pid_basename(path: &Path) -> Result<&str, MetadataError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            DaemonPathError::unsafe_path(path, "PID metadata basename is not valid UTF-8").into()
        })
}

fn random_nonce() -> Result<String, MetadataError> {
    let random_path = Path::new("/dev/urandom");
    let mut bytes = [0_u8; RANDOM_NONCE_BYTES];
    File::open(random_path)
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|source| MetadataError::io("read randomness from", random_path, source))?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(RANDOM_NONCE_BYTES * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(encoded)
}

fn ensure_same_file(
    path: &Path,
    expected: &fs::Metadata,
    actual: &fs::Metadata,
) -> Result<(), MetadataError> {
    if expected.dev() == actual.dev() && expected.ino() == actual.ino() {
        Ok(())
    } else {
        Err(
            DaemonPathError::unsafe_path(path, "PID metadata was replaced during validation")
                .into(),
        )
    }
}

struct TemporaryFileCleanup {
    path: PathBuf,
    armed: bool,
}

impl TemporaryFileCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFileCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Stable process attributes used to bind PID metadata to one worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessSnapshot {
    pid: u32,
    uid: u32,
    started_at: SystemTime,
    executable_device: u64,
    executable_inode: u64,
    worker: bool,
}

/// Result of checking a metadata PID. Only `Verified` permits reuse or a
/// termination decision; every ambiguous live state is an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessStatus {
    Dead,
    Verified(ProcessSnapshot),
}

#[derive(Debug, Error)]
pub enum ProcessIdentityError {
    #[error("daemon process identity could not be verified: {0}")]
    Security(&'static str),
    #[error("daemon process query failed")]
    Query(#[source] io::Error),
}

impl ProcessIdentityError {
    fn query(source: io::Error) -> Self {
        Self::Query(source)
    }
}

pub(crate) trait PreparedTermination: Send {
    fn terminate(self: Box<Self>) -> Result<(), ProcessIdentityError>;
}

pub(crate) trait ProcessQuery: Send + Sync {
    fn snapshot(&self, pid: u32) -> Result<Option<ProcessSnapshot>, ProcessIdentityError>;
    fn prepare_termination(
        &self,
        pid: u32,
    ) -> Result<Box<dyn PreparedTermination>, ProcessIdentityError>;
}

/// Verifies process ownership and identity, and makes signaling conditional on
/// two matching observations. Linux additionally binds the signal to a pidfd,
/// closing the PID-reuse race between verification and delivery.
pub(crate) struct ProcessInspector<Q = SystemProcessQuery> {
    query: Q,
    executable_device: u64,
    executable_inode: u64,
}

impl ProcessInspector<SystemProcessQuery> {
    pub(crate) fn new() -> Result<Self, ProcessIdentityError> {
        let executable = std::env::current_exe().map_err(ProcessIdentityError::query)?;
        validate_absolute_normalized_path(&executable)?;
        let metadata = fs::metadata(&executable).map_err(ProcessIdentityError::query)?;
        if !metadata.is_file() {
            return Err(ProcessIdentityError::Security(
                "current executable is not a regular file",
            ));
        }
        Ok(Self {
            query: SystemProcessQuery,
            executable_device: metadata.dev(),
            executable_inode: metadata.ino(),
        })
    }
}

impl<Q: ProcessQuery> ProcessInspector<Q> {
    #[cfg(test)]
    fn with_query(query: Q, executable_device: u64, executable_inode: u64) -> Self {
        Self {
            query,
            executable_device,
            executable_inode,
        }
    }

    pub(crate) fn inspect(
        &self,
        metadata: &PidMetadata,
    ) -> Result<ProcessStatus, ProcessIdentityError> {
        if metadata.pid == 0 {
            return Err(ProcessIdentityError::Security(
                "metadata contains an invalid process identifier",
            ));
        }
        let Some(snapshot) = self.query.snapshot(metadata.pid)? else {
            return Ok(ProcessStatus::Dead);
        };
        self.verify_snapshot(metadata, snapshot)?;
        Ok(ProcessStatus::Verified(snapshot))
    }

    pub(crate) fn terminate_verified(
        &self,
        metadata: &PidMetadata,
    ) -> Result<(), ProcessIdentityError> {
        let prepared = self.query.prepare_termination(metadata.pid)?;
        let first = match self.inspect(metadata)? {
            ProcessStatus::Verified(snapshot) => snapshot,
            ProcessStatus::Dead => {
                return Err(ProcessIdentityError::Security(
                    "process exited before termination verification",
                ));
            }
        };
        let second = match self.inspect(metadata)? {
            ProcessStatus::Verified(snapshot) => snapshot,
            ProcessStatus::Dead => {
                return Err(ProcessIdentityError::Security(
                    "process exited during termination verification",
                ));
            }
        };
        if first != second {
            return Err(ProcessIdentityError::Security(
                "process identity changed during termination verification",
            ));
        }
        prepared.terminate()
    }

    fn verify_snapshot(
        &self,
        metadata: &PidMetadata,
        snapshot: ProcessSnapshot,
    ) -> Result<(), ProcessIdentityError> {
        if snapshot.pid != metadata.pid
            || snapshot.uid != rustix::process::getuid().as_raw()
            || snapshot.started_at != metadata.started_at
            || snapshot.executable_device != self.executable_device
            || snapshot.executable_inode != self.executable_inode
            || !snapshot.worker
        {
            return Err(ProcessIdentityError::Security(
                "PID does not match the recorded daemon worker identity",
            ));
        }
        Ok(())
    }
}

impl PidMetadata {
    /// Builds metadata from the kernel's identity for the current worker. This
    /// deliberately persists only pid/config_hash/start time.
    pub(crate) fn for_current_worker(
        config_hash: ConfigHash,
    ) -> Result<Self, ProcessIdentityError> {
        let pid = std::process::id();
        let snapshot = SystemProcessQuery
            .snapshot(pid)?
            .ok_or(ProcessIdentityError::Security(
                "current worker process could not be queried",
            ))?;
        if snapshot.uid != rustix::process::getuid().as_raw() || !snapshot.worker {
            return Err(ProcessIdentityError::Security(
                "current process is not the expected daemon worker",
            ));
        }
        Ok(Self {
            pid,
            config_hash,
            started_at: snapshot.started_at,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SystemProcessQuery;

#[cfg(target_os = "linux")]
impl ProcessQuery for SystemProcessQuery {
    fn snapshot(&self, pid: u32) -> Result<Option<ProcessSnapshot>, ProcessIdentityError> {
        linux_process_snapshot(pid)
    }

    fn prepare_termination(
        &self,
        pid: u32,
    ) -> Result<Box<dyn PreparedTermination>, ProcessIdentityError> {
        let pid = rustix::process::Pid::from_raw(pid as i32).ok_or(
            ProcessIdentityError::Security("invalid process identifier for termination"),
        )?;
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .map_err(|error| ProcessIdentityError::query(io::Error::from(error)))?;
        Ok(Box::new(LinuxPreparedTermination(pidfd)))
    }
}

#[cfg(target_os = "linux")]
struct LinuxPreparedTermination(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl PreparedTermination for LinuxPreparedTermination {
    fn terminate(self: Box<Self>) -> Result<(), ProcessIdentityError> {
        rustix::process::pidfd_send_signal(&self.0, rustix::process::Signal::TERM)
            .map_err(|error| ProcessIdentityError::query(io::Error::from(error)))
    }
}

#[cfg(target_os = "linux")]
fn linux_process_snapshot(pid: u32) -> Result<Option<ProcessSnapshot>, ProcessIdentityError> {
    const PROC_FILE_LIMIT: usize = 1024 * 1024;
    let directory = PathBuf::from(format!("/proc/{pid}"));
    let before = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ProcessIdentityError::query(source)),
    };
    let uid = rustix::process::getuid().as_raw();
    if before.file_type().is_symlink() || !before.is_dir() || before.uid() != uid {
        return Err(ProcessIdentityError::Security(
            "proc process directory owner or type is unsafe",
        ));
    }

    let stat = read_validated_proc_file(&directory.join("stat"), uid, PROC_FILE_LIMIT)?;
    let (state, start_ticks) = parse_linux_stat(&stat)?;
    if state == b'Z' || state == b'X' {
        return Ok(None);
    }
    let status = read_validated_proc_file(&directory.join("status"), uid, PROC_FILE_LIMIT)?;
    let status_uid = parse_linux_status_uid(&status)?;
    let cmdline = read_validated_proc_file(&directory.join("cmdline"), uid, PROC_FILE_LIMIT)?;
    let worker = is_worker_cmdline(&cmdline);

    let executable_link = directory.join("exe");
    let link_metadata =
        fs::symlink_metadata(&executable_link).map_err(ProcessIdentityError::query)?;
    if !link_metadata.file_type().is_symlink() || link_metadata.uid() != uid {
        return Err(ProcessIdentityError::Security(
            "proc executable link owner or type is unsafe",
        ));
    }
    let executable_path = fs::read_link(&executable_link).map_err(ProcessIdentityError::query)?;
    validate_absolute_normalized_path(&executable_path)?;
    let executable = fs::metadata(&executable_link).map_err(ProcessIdentityError::query)?;
    if !executable.is_file() {
        return Err(ProcessIdentityError::Security(
            "proc executable target is not a regular file",
        ));
    }

    let after = fs::symlink_metadata(&directory).map_err(ProcessIdentityError::query)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || after.uid() != uid
        || !after.is_dir()
    {
        return Err(ProcessIdentityError::Security(
            "process changed during proc identity query",
        ));
    }

    Ok(Some(ProcessSnapshot {
        pid,
        uid: status_uid,
        started_at: linux_start_time(start_ticks)?,
        executable_device: executable.dev(),
        executable_inode: executable.ino(),
        worker,
    }))
}

#[cfg(target_os = "linux")]
fn read_validated_proc_file(
    path: &Path,
    expected_uid: u32,
    limit: usize,
) -> Result<Vec<u8>, ProcessIdentityError> {
    let before = fs::symlink_metadata(path).map_err(ProcessIdentityError::query)?;
    if before.file_type().is_symlink() || !before.is_file() || before.uid() != expected_uid {
        return Err(ProcessIdentityError::Security(
            "proc identity file owner or type is unsafe",
        ));
    }
    let file = File::open(path).map_err(ProcessIdentityError::query)?;
    let opened = file.metadata().map_err(ProcessIdentityError::query)?;
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        return Err(ProcessIdentityError::Security(
            "proc identity file changed while opening",
        ));
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ProcessIdentityError::query)?;
    if bytes.len() > limit {
        return Err(ProcessIdentityError::Security(
            "proc identity file exceeded its safety limit",
        ));
    }
    let after = fs::symlink_metadata(path).map_err(ProcessIdentityError::query)?;
    if opened.dev() != after.dev() || opened.ino() != after.ino() {
        return Err(ProcessIdentityError::Security(
            "proc identity file changed during query",
        ));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(bytes: &[u8]) -> Result<(u8, u64), ProcessIdentityError> {
    let closing =
        bytes
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or(ProcessIdentityError::Security(
                "proc stat has malformed process name",
            ))?;
    let fields = bytes
        .get(closing + 1..)
        .ok_or(ProcessIdentityError::Security("proc stat is truncated"))?
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() <= 19 || fields[0].len() != 1 {
        return Err(ProcessIdentityError::Security(
            "proc stat does not contain a process start time",
        ));
    }
    let start_ticks = std::str::from_utf8(fields[19])
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(ProcessIdentityError::Security(
            "proc stat process start time is invalid",
        ))?;
    Ok((fields[0][0], start_ticks))
}

#[cfg(target_os = "linux")]
fn parse_linux_status_uid(bytes: &[u8]) -> Result<u32, ProcessIdentityError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ProcessIdentityError::Security("proc status is not valid UTF-8"))?;
    let line = text.lines().find(|line| line.starts_with("Uid:")).ok_or(
        ProcessIdentityError::Security("proc status does not contain a UID"),
    )?;
    line[4..]
        .split_ascii_whitespace()
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(ProcessIdentityError::Security("proc status UID is invalid"))
}

#[cfg(target_os = "linux")]
fn linux_start_time(start_ticks: u64) -> Result<SystemTime, ProcessIdentityError> {
    let system_stat = fs::read_to_string("/proc/stat").map_err(ProcessIdentityError::query)?;
    let boot_seconds = system_stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(ProcessIdentityError::Security(
            "system boot time could not be verified",
        ))?;
    let ticks_per_second = rustix::param::clock_ticks_per_second();
    if ticks_per_second == 0 {
        return Err(ProcessIdentityError::Security(
            "system clock tick rate is invalid",
        ));
    }
    let whole = start_ticks / ticks_per_second;
    let nanos = ((start_ticks % ticks_per_second) as u128 * 1_000_000_000_u128
        / ticks_per_second as u128) as u32;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(boot_seconds.saturating_add(whole)))
        .and_then(|time| time.checked_add(Duration::from_nanos(nanos.into())))
        .ok_or(ProcessIdentityError::Security(
            "process start time is outside the supported range",
        ))
}

fn is_worker_cmdline(bytes: &[u8]) -> bool {
    let mut arguments = bytes.split(|byte| *byte == 0);
    matches!(arguments.next(), Some(executable) if !executable.is_empty())
        && matches!(arguments.next(), Some(b"__daemon"))
        && arguments.all(|argument| argument.is_empty())
}

fn validate_absolute_normalized_path(path: &Path) -> Result<(), ProcessIdentityError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(ProcessIdentityError::Security(
            "executable path is not absolute and normalized",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos_process {
    use std::{ffi::c_void, mem::MaybeUninit, os::raw::c_int};

    use super::*;

    const PROC_PIDTBSDINFO: c_int = 3;
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
    const CTL_KERN: c_int = 1;
    const KERN_ARGMAX: c_int = 8;
    const KERN_PROCARGS2: c_int = 49;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [i8; 16],
        pbi_name: [i8; 32],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
        fn sysctl(
            name: *mut c_int,
            namelen: u32,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    pub(super) fn snapshot(pid: u32) -> Result<Option<ProcessSnapshot>, ProcessIdentityError> {
        let first = match bsd_info(pid)? {
            Some(info) => info,
            None => return Ok(None),
        };
        let mut path_bytes = vec![0_u8; PROC_PIDPATHINFO_MAXSIZE];
        // SAFETY: the buffer is writable for the supplied length.
        let path_length = unsafe {
            proc_pidpath(
                pid as c_int,
                path_bytes.as_mut_ptr().cast(),
                path_bytes.len() as u32,
            )
        };
        if path_length <= 0 {
            return Err(ProcessIdentityError::query(io::Error::last_os_error()));
        }
        path_bytes.truncate(path_length as usize);
        if path_bytes.last() == Some(&0) {
            path_bytes.pop();
        }
        let executable_path = PathBuf::from(std::ffi::OsString::from_vec(path_bytes));
        validate_absolute_normalized_path(&executable_path)?;
        let executable = fs::metadata(&executable_path).map_err(ProcessIdentityError::query)?;
        if !executable.is_file() {
            return Err(ProcessIdentityError::Security(
                "queried executable is not a regular file",
            ));
        }
        let arguments = process_arguments(pid)?;
        let second = bsd_info(pid)?.ok_or(ProcessIdentityError::Security(
            "process exited during identity query",
        ))?;
        if first != second {
            return Err(ProcessIdentityError::Security(
                "process changed during identity query",
            ));
        }
        let started_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(first.pbi_start_tvsec))
            .and_then(|time| time.checked_add(Duration::from_micros(first.pbi_start_tvusec)))
            .ok_or(ProcessIdentityError::Security(
                "process start time is outside the supported range",
            ))?;
        Ok(Some(ProcessSnapshot {
            pid,
            uid: first.pbi_uid,
            started_at,
            executable_device: executable.dev(),
            executable_inode: executable.ino(),
            worker: is_worker_cmdline(&arguments),
        }))
    }

    fn bsd_info(pid: u32) -> Result<Option<ProcBsdInfo>, ProcessIdentityError> {
        let mut info = MaybeUninit::<ProcBsdInfo>::zeroed();
        // SAFETY: proc_pidinfo initializes exactly the provided C structure.
        let read = unsafe {
            proc_pidinfo(
                pid as c_int,
                PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<ProcBsdInfo>() as c_int,
            )
        };
        if read == 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(3) => Ok(None),
                _ => Err(ProcessIdentityError::query(error)),
            };
        }
        if read as usize != std::mem::size_of::<ProcBsdInfo>() {
            return Err(ProcessIdentityError::Security(
                "platform process query returned an ambiguous record",
            ));
        }
        // SAFETY: a complete structure was reported above.
        Ok(Some(unsafe { info.assume_init() }))
    }

    fn process_arguments(pid: u32) -> Result<Vec<u8>, ProcessIdentityError> {
        let mut argmax_name = [CTL_KERN, KERN_ARGMAX];
        let mut argmax: c_int = 0;
        let mut argmax_size = std::mem::size_of::<c_int>();
        // SAFETY: all pointers refer to initialized writable storage.
        if unsafe {
            sysctl(
                argmax_name.as_mut_ptr(),
                argmax_name.len() as u32,
                (&mut argmax as *mut c_int).cast(),
                &mut argmax_size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || argmax <= 0
            || argmax as usize > 16 * 1024 * 1024
        {
            return Err(ProcessIdentityError::Security(
                "platform process argument limit is unsafe",
            ));
        }
        let mut bytes = vec![0_u8; argmax as usize];
        let mut size = bytes.len();
        let mut args_name = [CTL_KERN, KERN_PROCARGS2, pid as c_int];
        // SAFETY: bytes is writable for size bytes and the query is read-only.
        if unsafe {
            sysctl(
                args_name.as_mut_ptr(),
                args_name.len() as u32,
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(ProcessIdentityError::query(io::Error::last_os_error()));
        }
        bytes.truncate(size);
        parse_procargs(&bytes)
    }

    fn parse_procargs(bytes: &[u8]) -> Result<Vec<u8>, ProcessIdentityError> {
        if bytes.len() < std::mem::size_of::<c_int>() {
            return Err(ProcessIdentityError::Security(
                "platform process arguments are truncated",
            ));
        }
        let argc = c_int::from_ne_bytes(bytes[..4].try_into().expect("checked length"));
        if argc != 2 {
            return Ok(Vec::new());
        }
        let mut cursor = 4;
        while cursor < bytes.len() && bytes[cursor] != 0 {
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor] == 0 {
            cursor += 1;
        }
        let mut encoded = Vec::new();
        for _ in 0..argc {
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor] != 0 {
                cursor += 1;
            }
            if cursor == bytes.len() || cursor == start {
                return Err(ProcessIdentityError::Security(
                    "platform process arguments are malformed",
                ));
            }
            encoded.extend_from_slice(&bytes[start..cursor]);
            encoded.push(0);
            while cursor < bytes.len() && bytes[cursor] == 0 {
                cursor += 1;
            }
        }
        Ok(encoded)
    }

    struct MacPreparedTermination(rustix::process::Pid);

    impl PreparedTermination for MacPreparedTermination {
        fn terminate(self: Box<Self>) -> Result<(), ProcessIdentityError> {
            rustix::process::kill_process(self.0, rustix::process::Signal::TERM)
                .map_err(|error| ProcessIdentityError::query(io::Error::from(error)))
        }
    }

    pub(super) fn prepared_termination(
        pid: u32,
    ) -> Result<Box<dyn PreparedTermination>, ProcessIdentityError> {
        let pid = rustix::process::Pid::from_raw(pid as i32).ok_or(
            ProcessIdentityError::Security("invalid process identifier for termination"),
        )?;
        Ok(Box::new(MacPreparedTermination(pid)))
    }
}

#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStringExt;

#[cfg(target_os = "macos")]
impl ProcessQuery for SystemProcessQuery {
    fn snapshot(&self, pid: u32) -> Result<Option<ProcessSnapshot>, ProcessIdentityError> {
        macos_process::snapshot(pid)
    }

    fn prepare_termination(
        &self,
        pid: u32,
    ) -> Result<Box<dyn PreparedTermination>, ProcessIdentityError> {
        macos_process::prepared_termination(pid)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl ProcessQuery for SystemProcessQuery {
    fn snapshot(&self, _pid: u32) -> Result<Option<ProcessSnapshot>, ProcessIdentityError> {
        Err(ProcessIdentityError::Security(
            "daemon process identity queries are unsupported on this Unix platform",
        ))
    }

    fn prepare_termination(
        &self,
        _pid: u32,
    ) -> Result<Box<dyn PreparedTermination>, ProcessIdentityError> {
        Err(ProcessIdentityError::Security(
            "daemon process signaling is unsupported on this Unix platform",
        ))
    }
}

mod config_hash_hex {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    use crate::config::{ConfigHash, SHA256_HEX_LENGTH};

    pub fn serialize<S>(hash: &ConfigHash, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hash.to_hex())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ConfigHash, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != SHA256_HEX_LENGTH {
            return Err(D::Error::custom(
                "config_hash must contain 64 hexadecimal characters",
            ));
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
            let high =
                decode_hex(pair[0]).ok_or_else(|| D::Error::custom("invalid config_hash"))?;
            let low = decode_hex(pair[1]).ok_or_else(|| D::Error::custom("invalid config_hash"))?;
            bytes[index] = high << 4 | low;
        }
        Ok(ConfigHash(bytes))
    }

    fn decode_hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, VecDeque},
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, UNIX_EPOCH},
    };

    use serde_json::Value;
    use tempfile::TempDir;

    use crate::config::{config_hash, server_id};

    use super::*;

    fn store(temp: &TempDir) -> MetadataStore {
        let paths =
            DaemonPaths::from_runtime_parent(temp.path(), &server_id("metadata-test")).unwrap();
        MetadataStore::new(paths)
    }

    fn sample_metadata() -> PidMetadata {
        PidMetadata {
            pid: 4242,
            config_hash: config_hash(&serde_json::json!({"command": "fixture"})),
            started_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    fn write_raw(path: &Path, bytes: &[u8], mode: u32) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn atomically_writes_and_reads_private_metadata_with_exact_fields() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let expected = sample_metadata();
        store.write(&expected).unwrap();

        assert_eq!(store.read().unwrap(), expected);
        let file_metadata = fs::symlink_metadata(&store.paths().pid).unwrap();
        assert_eq!(file_metadata.mode() & 0o7777, 0o600);
        assert_eq!(file_metadata.uid(), rustix::process::getuid().as_raw());

        let value: Value = serde_json::from_slice(&fs::read(&store.paths().pid).unwrap()).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            BTreeSet::from([
                "config_hash".to_owned(),
                "pid".to_owned(),
                "started_at".to_owned(),
            ])
        );
        assert_eq!(value["config_hash"].as_str().unwrap().len(), 64);
        let entries = fs::read_dir(&store.paths().runtime_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![store.paths().pid.file_name().unwrap()]);
    }

    #[test]
    fn rejects_symlink_directory_and_overwide_permissions_without_following_target() {
        let symlink_temp = TempDir::new().unwrap();
        let symlink_store = store(&symlink_temp);
        let outside = symlink_temp.path().join("outside");
        write_raw(&outside, br#"{"pid":1}"#, 0o600);
        symlink(&outside, &symlink_store.paths().pid).unwrap();
        assert!(symlink_store.read().is_err());
        assert!(symlink_store.write(&sample_metadata()).is_err());
        assert_eq!(fs::read(&outside).unwrap(), br#"{"pid":1}"#);

        let type_temp = TempDir::new().unwrap();
        let type_store = store(&type_temp);
        fs::create_dir(&type_store.paths().pid).unwrap();
        assert!(type_store.read().is_err());
        assert!(type_store.remove().is_err());

        let mode_temp = TempDir::new().unwrap();
        let mode_store = store(&mode_temp);
        write_raw(&mode_store.paths().pid, b"{}", 0o644);
        assert!(mode_store.read().is_err());
        assert!(mode_store.write(&sample_metadata()).is_err());
    }

    #[test]
    fn rejects_oversized_invalid_and_extra_field_json() {
        let oversized_temp = TempDir::new().unwrap();
        let oversized_store = store(&oversized_temp);
        write_raw(
            &oversized_store.paths().pid,
            &vec![b'x'; PID_METADATA_MAX_BYTES + 1],
            0o600,
        );
        assert!(matches!(
            oversized_store.read(),
            Err(MetadataError::TooLarge { .. })
        ));

        let invalid_temp = TempDir::new().unwrap();
        let invalid_store = store(&invalid_temp);
        write_raw(&invalid_store.paths().pid, b"not json", 0o600);
        assert!(matches!(
            invalid_store.read(),
            Err(MetadataError::InvalidJson(_))
        ));

        let extra_temp = TempDir::new().unwrap();
        let extra_store = store(&extra_temp);
        let mut value = serde_json::to_value(sample_metadata()).unwrap();
        value["secret"] = Value::String("must-not-be-accepted".into());
        write_raw(
            &extra_store.paths().pid,
            &serde_json::to_vec(&value).unwrap(),
            0o600,
        );
        assert!(matches!(
            extra_store.read(),
            Err(MetadataError::InvalidJson(_))
        ));
    }

    #[test]
    fn failed_publication_cleans_unpredictable_temporary_file() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let result = store.write_with_before_rename(&sample_metadata(), || {
            Err(io::Error::other("injected publication failure"))
        });
        assert!(result.is_err());
        assert!(!store.paths().pid.exists());
        assert_eq!(fs::read_dir(&store.paths().runtime_dir).unwrap().count(), 0);
    }

    #[test]
    fn safe_remove_rejects_symlink_and_removes_only_valid_metadata() {
        let valid_temp = TempDir::new().unwrap();
        let valid_store = store(&valid_temp);
        valid_store.write(&sample_metadata()).unwrap();
        assert!(valid_store.remove().unwrap());
        assert!(!valid_store.remove().unwrap());

        let symlink_temp = TempDir::new().unwrap();
        let symlink_store = store(&symlink_temp);
        let outside = symlink_temp.path().join("outside");
        write_raw(&outside, b"outside", 0o600);
        symlink(&outside, &symlink_store.paths().pid).unwrap();
        assert!(symlink_store.remove().is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[derive(Clone)]
    struct FakeProcessQuery {
        snapshots: Arc<Mutex<VecDeque<Option<ProcessSnapshot>>>>,
        prepared: Arc<AtomicUsize>,
        terminated: Arc<AtomicUsize>,
    }

    impl FakeProcessQuery {
        fn new(snapshots: impl IntoIterator<Item = Option<ProcessSnapshot>>) -> Self {
            Self {
                snapshots: Arc::new(Mutex::new(snapshots.into_iter().collect())),
                prepared: Arc::new(AtomicUsize::new(0)),
                terminated: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl ProcessQuery for FakeProcessQuery {
        fn snapshot(&self, _pid: u32) -> Result<Option<ProcessSnapshot>, ProcessIdentityError> {
            self.snapshots
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ProcessIdentityError::Security("missing fake snapshot"))
        }

        fn prepare_termination(
            &self,
            _pid: u32,
        ) -> Result<Box<dyn PreparedTermination>, ProcessIdentityError> {
            self.prepared.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakePreparedTermination(Arc::clone(
                &self.terminated,
            ))))
        }
    }

    struct FakePreparedTermination(Arc<AtomicUsize>);

    impl PreparedTermination for FakePreparedTermination {
        fn terminate(self: Box<Self>) -> Result<(), ProcessIdentityError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn matching_snapshot(metadata: &PidMetadata) -> ProcessSnapshot {
        ProcessSnapshot {
            pid: metadata.pid,
            uid: rustix::process::getuid().as_raw(),
            started_at: metadata.started_at,
            executable_device: 11,
            executable_inode: 22,
            worker: true,
        }
    }

    #[test]
    fn process_identity_rejects_reused_pid_wrong_owner_executable_and_role() {
        let metadata = sample_metadata();
        let matching = matching_snapshot(&metadata);
        let mut reused = matching;
        reused.started_at = metadata.started_at + Duration::from_secs(1);
        let mut wrong_owner = matching;
        wrong_owner.uid = wrong_owner.uid.wrapping_add(1);
        let mut wrong_executable = matching;
        wrong_executable.executable_inode += 1;
        let mut wrong_role = matching;
        wrong_role.worker = false;

        for snapshot in [reused, wrong_owner, wrong_executable, wrong_role] {
            let inspector =
                ProcessInspector::with_query(FakeProcessQuery::new([Some(snapshot)]), 11, 22);
            assert!(matches!(
                inspector.inspect(&metadata),
                Err(ProcessIdentityError::Security(_))
            ));
        }

        let inspector = ProcessInspector::with_query(FakeProcessQuery::new([None]), 11, 22);
        assert_eq!(inspector.inspect(&metadata).unwrap(), ProcessStatus::Dead);
    }

    #[test]
    fn termination_occurs_only_after_two_matching_identity_observations() {
        let metadata = sample_metadata();
        let matching = matching_snapshot(&metadata);
        let query = FakeProcessQuery::new([Some(matching), Some(matching)]);
        let prepared = Arc::clone(&query.prepared);
        let terminated = Arc::clone(&query.terminated);
        let inspector = ProcessInspector::with_query(query, 11, 22);
        inspector.terminate_verified(&metadata).unwrap();
        assert_eq!(prepared.load(Ordering::SeqCst), 1);
        assert_eq!(terminated.load(Ordering::SeqCst), 1);

        let mut reused = matching;
        reused.started_at = metadata.started_at + Duration::from_secs(1);
        let query = FakeProcessQuery::new([Some(matching), Some(reused)]);
        let terminated = Arc::clone(&query.terminated);
        let inspector = ProcessInspector::with_query(query, 11, 22);
        assert!(inspector.terminate_verified(&metadata).is_err());
        assert_eq!(terminated.load(Ordering::SeqCst), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parsers_require_exact_worker_role_and_valid_start_field() {
        assert!(is_worker_cmdline(b"/tmp/mcp-cli\0__daemon\0"));
        assert!(!is_worker_cmdline(b"/tmp/mcp-cli\0list\0"));
        assert!(!is_worker_cmdline(b"/tmp/mcp-cli\0__daemon\0unexpected\0"));

        let stat =
            b"42 (worker with ) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 98765 20";
        assert_eq!(parse_linux_stat(stat).unwrap(), (b'S', 98765));
        assert!(parse_linux_stat(b"42 malformed").is_err());
        assert_eq!(
            parse_linux_status_uid(b"Name:\tworker\nUid:\t123\t123\t123\t123\n").unwrap(),
            123
        );
    }
}
