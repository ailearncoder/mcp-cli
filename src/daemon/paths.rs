#![cfg(unix)]
//! Secure Unix daemon runtime paths.

use std::{
    env, fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::config::{SHA256_HEX_LENGTH, ServerId};

const RUNTIME_DIRECTORY_PREFIX: &str = "mcp-cli-";
const SOCKET_SUFFIX: &str = ".sock";
const PID_SUFFIX: &str = ".pid";
const LOCK_SUFFIX: &str = ".lock";
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Filesystem type expected when validating or removing a daemon artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactKind {
    RegularFile,
    Socket,
}

/// Device/inode identity captured when a worker takes ownership of an
/// artifact. Shutdown uses this token to avoid unlinking a path replaced by
/// another process after publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactIdentity {
    device: u64,
    inode: u64,
}

impl ArtifactIdentity {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        metadata.dev() == self.device && metadata.ino() == self.inode
    }
}

/// A fail-closed error raised by daemon path or metadata validation.
#[derive(Debug, Error)]
pub enum DaemonPathError {
    #[error("unsafe daemon path {path}: {reason}")]
    Unsafe { path: PathBuf, reason: &'static str },
    #[error("could not {operation} daemon path {path}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl DaemonPathError {
    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn unsafe_path(path: impl Into<PathBuf>, reason: &'static str) -> Self {
        Self::Unsafe {
            path: path.into(),
            reason,
        }
    }
}

/// Validated per-user daemon paths for one hashed server identifier.
#[derive(Clone, Debug)]
pub struct DaemonPaths {
    pub runtime_dir: PathBuf,
    pub socket: PathBuf,
    pub pid: PathBuf,
    pub lock: PathBuf,
    uid: u32,
    runtime_device: u64,
    runtime_inode: u64,
}

impl DaemonPaths {
    /// Creates or securely reuses `${TMPDIR:-/tmp}/mcp-cli-<uid>/` and derives
    /// artifact names solely from a complete lowercase hexadecimal ServerId.
    pub fn new(server_id: &ServerId) -> Result<Self, DaemonPathError> {
        let temporary_root = env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        Self::from_runtime_parent(&temporary_root, server_id)
    }

    /// Creates or securely reuses a daemon runtime directory below an explicit
    /// temporary root. This is intended for isolated integrations and applies
    /// exactly the same validation as [`Self::new`] without consulting or
    /// modifying the process `TMPDIR`.
    pub fn from_runtime_parent(
        temporary_root: &Path,
        server_id: &ServerId,
    ) -> Result<Self, DaemonPathError> {
        let uid = current_uid();
        Self::from_runtime_parent_for_uid(temporary_root, server_id, uid)
    }

    fn from_runtime_parent_for_uid(
        temporary_root: &Path,
        server_id: &ServerId,
        uid: u32,
    ) -> Result<Self, DaemonPathError> {
        validate_server_id(server_id)?;
        let canonical_parent = fs::canonicalize(temporary_root)
            .map_err(|source| DaemonPathError::io("canonicalize", temporary_root, source))?;
        let parent_metadata = fs::metadata(&canonical_parent)
            .map_err(|source| DaemonPathError::io("inspect", &canonical_parent, source))?;
        if !parent_metadata.is_dir() {
            return Err(DaemonPathError::unsafe_path(
                canonical_parent,
                "temporary root is not a directory",
            ));
        }

        let runtime_dir = canonical_parent.join(format!("{RUNTIME_DIRECTORY_PREFIX}{uid}"));
        create_or_reuse_runtime_dir(&runtime_dir, &canonical_parent, uid)?;
        let metadata = validate_runtime_dir(&runtime_dir, uid, None)?;

        let basename = &server_id.0;
        let socket = runtime_dir.join(format!("{basename}{SOCKET_SUFFIX}"));
        let pid = runtime_dir.join(format!("{basename}{PID_SUFFIX}"));
        let lock = runtime_dir.join(format!("{basename}{LOCK_SUFFIX}"));
        let paths = Self {
            runtime_dir,
            socket,
            pid,
            lock,
            uid,
            runtime_device: metadata.dev(),
            runtime_inode: metadata.ino(),
        };
        paths.validate_all_children()?;
        Ok(paths)
    }

    /// Revalidates the runtime directory, including its original device/inode
    /// identity, ownership, mode, and canonical parent relationship.
    pub fn validate_runtime_dir(&self) -> Result<(), DaemonPathError> {
        validate_runtime_dir(
            &self.runtime_dir,
            self.uid,
            Some((self.runtime_device, self.runtime_inode)),
        )?;
        Ok(())
    }

    /// Captures the currently published socket identity after validating its
    /// parent, type, owner, and symlink status. A missing artifact is not owned
    /// by this worker and must not be removed if it appears later.
    pub(crate) fn capture_socket_identity(
        &self,
    ) -> Result<Option<ArtifactIdentity>, DaemonPathError> {
        self.capture_identity(&self.socket, ArtifactKind::Socket, true)
    }

    /// Captures the currently published PID identity with private-file mode
    /// validation.
    pub(crate) fn capture_pid_identity(&self) -> Result<Option<ArtifactIdentity>, DaemonPathError> {
        self.capture_identity(&self.pid, ArtifactKind::RegularFile, true)
    }

    /// Captures the lock identity held by this worker with private-file mode
    /// validation.
    pub(crate) fn capture_lock_identity(
        &self,
    ) -> Result<Option<ArtifactIdentity>, DaemonPathError> {
        self.capture_identity(&self.lock, ArtifactKind::RegularFile, true)
    }

    pub(crate) fn remove_socket_if_owned(
        &self,
        identity: ArtifactIdentity,
    ) -> Result<bool, DaemonPathError> {
        self.safe_remove(&self.socket, ArtifactKind::Socket, Some(identity))
    }

    pub(crate) fn remove_pid_if_owned(
        &self,
        identity: ArtifactIdentity,
    ) -> Result<bool, DaemonPathError> {
        self.safe_remove(&self.pid, ArtifactKind::RegularFile, Some(identity))
    }

    pub(crate) fn remove_lock_if_owned(
        &self,
        identity: ArtifactIdentity,
    ) -> Result<bool, DaemonPathError> {
        self.safe_remove(&self.lock, ArtifactKind::RegularFile, Some(identity))
    }

    /// Safely removes this server's PID file when it exists.
    pub fn remove_pid(&self) -> Result<bool, DaemonPathError> {
        self.safe_remove(&self.pid, ArtifactKind::RegularFile, None)
    }

    /// Safely removes this server's lock file when it exists.
    pub fn remove_lock(&self) -> Result<bool, DaemonPathError> {
        self.safe_remove(&self.lock, ArtifactKind::RegularFile, None)
    }

    /// Safely removes this server's Unix socket when it exists.
    pub fn remove_socket(&self) -> Result<bool, DaemonPathError> {
        self.safe_remove(&self.socket, ArtifactKind::Socket, None)
    }

    pub(crate) fn validate_pid_path(&self) -> Result<(), DaemonPathError> {
        self.validate_child(&self.pid, PID_SUFFIX)
    }

    pub(crate) fn validate_artifact_metadata(
        &self,
        path: &Path,
        metadata: &fs::Metadata,
        kind: ArtifactKind,
        require_private_mode: bool,
    ) -> Result<(), DaemonPathError> {
        validate_artifact_metadata(path, metadata, self.uid, kind, require_private_mode)
    }

    fn validate_all_children(&self) -> Result<(), DaemonPathError> {
        self.validate_child(&self.socket, SOCKET_SUFFIX)?;
        self.validate_child(&self.pid, PID_SUFFIX)?;
        self.validate_child(&self.lock, LOCK_SUFFIX)
    }

    fn validate_child(&self, path: &Path, suffix: &str) -> Result<(), DaemonPathError> {
        if path.parent() != Some(self.runtime_dir.as_path()) {
            return Err(DaemonPathError::unsafe_path(
                path,
                "artifact parent is outside the runtime directory",
            ));
        }
        let expected_name = format!("{}{suffix}", self.server_id_from_path()?);
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
            return Err(DaemonPathError::unsafe_path(
                path,
                "artifact basename is not the expected hashed server identifier",
            ));
        }

        self.validate_runtime_dir()?;
        let canonical_parent = fs::canonicalize(path.parent().expect("checked parent"))
            .map_err(|source| DaemonPathError::io("canonicalize", path, source))?;
        if canonical_parent != self.runtime_dir {
            return Err(DaemonPathError::unsafe_path(
                path,
                "artifact canonical parent escaped the runtime directory",
            ));
        }
        Ok(())
    }

    fn server_id_from_path(&self) -> Result<&str, DaemonPathError> {
        let name = self
            .pid
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                DaemonPathError::unsafe_path(&self.pid, "PID basename is not valid UTF-8")
            })?;
        name.strip_suffix(PID_SUFFIX).ok_or_else(|| {
            DaemonPathError::unsafe_path(&self.pid, "PID basename has an invalid suffix")
        })
    }

    fn capture_identity(
        &self,
        path: &Path,
        kind: ArtifactKind,
        require_private_mode: bool,
    ) -> Result<Option<ArtifactIdentity>, DaemonPathError> {
        let suffix = match kind {
            ArtifactKind::Socket => SOCKET_SUFFIX,
            ArtifactKind::RegularFile if path == self.pid => PID_SUFFIX,
            ArtifactKind::RegularFile => LOCK_SUFFIX,
        };
        self.validate_child(path, suffix)?;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(DaemonPathError::io("inspect", path, source)),
        };
        self.validate_artifact_metadata(path, &metadata, kind, require_private_mode)?;
        Ok(Some(ArtifactIdentity::from_metadata(&metadata)))
    }

    fn safe_remove(
        &self,
        path: &Path,
        kind: ArtifactKind,
        expected_identity: Option<ArtifactIdentity>,
    ) -> Result<bool, DaemonPathError> {
        let suffix = match kind {
            ArtifactKind::Socket => SOCKET_SUFFIX,
            ArtifactKind::RegularFile if path == self.pid => PID_SUFFIX,
            ArtifactKind::RegularFile => LOCK_SUFFIX,
        };
        self.validate_child(path, suffix)?;
        let first = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(DaemonPathError::io("inspect", path, source)),
        };
        self.validate_artifact_metadata(path, &first, kind, false)?;
        if expected_identity.is_some_and(|identity| !identity.matches(&first)) {
            return Err(DaemonPathError::unsafe_path(
                path,
                "artifact identity no longer belongs to this worker",
            ));
        }
        self.validate_runtime_dir()?;
        let second = fs::symlink_metadata(path)
            .map_err(|source| DaemonPathError::io("reinspect", path, source))?;
        self.validate_artifact_metadata(path, &second, kind, false)?;
        if first.dev() != second.dev() || first.ino() != second.ino() {
            return Err(DaemonPathError::unsafe_path(
                path,
                "artifact was replaced during validation",
            ));
        }
        if expected_identity.is_some_and(|identity| !identity.matches(&second)) {
            return Err(DaemonPathError::unsafe_path(
                path,
                "artifact identity no longer belongs to this worker",
            ));
        }
        fs::remove_file(path).map_err(|source| DaemonPathError::io("remove", path, source))?;
        Ok(true)
    }
}

fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn validate_server_id(server_id: &ServerId) -> Result<(), DaemonPathError> {
    let valid = server_id.0.len() == SHA256_HEX_LENGTH
        && server_id
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(DaemonPathError::unsafe_path(
            PathBuf::from(&server_id.0),
            "server identifier must be a full lowercase SHA-256 digest",
        ))
    }
}

fn create_or_reuse_runtime_dir(
    runtime_dir: &Path,
    canonical_parent: &Path,
    uid: u32,
) -> Result<(), DaemonPathError> {
    match fs::symlink_metadata(runtime_dir) {
        Ok(metadata) => validate_runtime_entry(runtime_dir, &metadata, uid)?,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            if let Err(source) = fs::create_dir(runtime_dir)
                && source.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(DaemonPathError::io("create", runtime_dir, source));
            }
            let metadata = fs::symlink_metadata(runtime_dir)
                .map_err(|source| DaemonPathError::io("inspect", runtime_dir, source))?;
            validate_runtime_entry(runtime_dir, &metadata, uid)?;
        }
        Err(source) => return Err(DaemonPathError::io("inspect", runtime_dir, source)),
    }

    fs::set_permissions(
        runtime_dir,
        fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
    )
    .map_err(|source| DaemonPathError::io("set permissions on", runtime_dir, source))?;
    let canonical_runtime = fs::canonicalize(runtime_dir)
        .map_err(|source| DaemonPathError::io("canonicalize", runtime_dir, source))?;
    if canonical_runtime.parent() != Some(canonical_parent) || canonical_runtime != runtime_dir {
        return Err(DaemonPathError::unsafe_path(
            runtime_dir,
            "runtime directory escaped its canonical temporary root",
        ));
    }
    Ok(())
}

fn validate_runtime_dir(
    runtime_dir: &Path,
    uid: u32,
    expected_identity: Option<(u64, u64)>,
) -> Result<fs::Metadata, DaemonPathError> {
    let metadata = fs::symlink_metadata(runtime_dir)
        .map_err(|source| DaemonPathError::io("inspect", runtime_dir, source))?;
    validate_runtime_entry(runtime_dir, &metadata, uid)?;
    if metadata.mode() & 0o7777 != PRIVATE_DIRECTORY_MODE {
        return Err(DaemonPathError::unsafe_path(
            runtime_dir,
            "runtime directory permissions are not exactly 0700",
        ));
    }
    if let Some((device, inode)) = expected_identity
        && (metadata.dev() != device || metadata.ino() != inode)
    {
        return Err(DaemonPathError::unsafe_path(
            runtime_dir,
            "runtime directory was replaced after validation",
        ));
    }
    let canonical = fs::canonicalize(runtime_dir)
        .map_err(|source| DaemonPathError::io("canonicalize", runtime_dir, source))?;
    if canonical != runtime_dir {
        return Err(DaemonPathError::unsafe_path(
            runtime_dir,
            "runtime directory is not canonical",
        ));
    }
    Ok(metadata)
}

fn validate_runtime_entry(
    path: &Path,
    metadata: &fs::Metadata,
    uid: u32,
) -> Result<(), DaemonPathError> {
    if metadata.file_type().is_symlink() {
        return Err(DaemonPathError::unsafe_path(
            path,
            "runtime directory must not be a symbolic link",
        ));
    }
    if !metadata.is_dir() {
        return Err(DaemonPathError::unsafe_path(
            path,
            "runtime path is not a directory",
        ));
    }
    if metadata.uid() != uid {
        return Err(DaemonPathError::unsafe_path(
            path,
            "runtime directory is not owned by the current UID",
        ));
    }
    Ok(())
}

pub(crate) fn validate_artifact_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    uid: u32,
    kind: ArtifactKind,
    require_private_mode: bool,
) -> Result<(), DaemonPathError> {
    if metadata.file_type().is_symlink() {
        return Err(DaemonPathError::unsafe_path(
            path,
            "daemon artifact must not be a symbolic link",
        ));
    }
    let type_matches = match kind {
        ArtifactKind::RegularFile => metadata.is_file(),
        ArtifactKind::Socket => metadata.file_type().is_socket(),
    };
    if !type_matches {
        return Err(DaemonPathError::unsafe_path(
            path,
            "daemon artifact has an unexpected file type",
        ));
    }
    if metadata.uid() != uid {
        return Err(DaemonPathError::unsafe_path(
            path,
            "daemon artifact is not owned by the current UID",
        ));
    }
    if require_private_mode && metadata.mode() & 0o077 != 0 {
        return Err(DaemonPathError::unsafe_path(
            path,
            "daemon artifact permissions are broader than 0600",
        ));
    }
    Ok(())
}

pub(crate) const fn private_file_mode() -> u32 {
    PRIVATE_FILE_MODE
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::symlink, os::unix::net::UnixListener};

    use tempfile::TempDir;

    use crate::config::server_id;

    use super::*;

    #[test]
    fn creates_private_runtime_and_hash_only_artifact_names() {
        let temp = TempDir::new().expect("tempdir");
        let malicious_name = "../server/name\nsecret";
        let id = server_id(malicious_name);
        let paths = DaemonPaths::from_runtime_parent(temp.path(), &id).expect("secure paths");

        assert_eq!(
            paths.runtime_dir,
            temp.path().join(format!("mcp-cli-{}", current_uid()))
        );
        assert_eq!(
            fs::symlink_metadata(&paths.runtime_dir)
                .expect("runtime metadata")
                .mode()
                & 0o7777,
            0o700
        );
        for (path, suffix) in [
            (&paths.socket, SOCKET_SUFFIX),
            (&paths.pid, PID_SUFFIX),
            (&paths.lock, LOCK_SUFFIX),
        ] {
            assert_eq!(path.parent(), Some(paths.runtime_dir.as_path()));
            let basename = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(basename, format!("{}{suffix}", id.0));
            assert!(!basename.contains('/'));
            assert!(!basename.contains(".."));
            assert!(!basename.contains(malicious_name));
        }
    }

    #[test]
    fn reuse_repairs_mode_but_rejects_symlink_non_directory_and_wrong_owner() {
        let id = server_id("alpha");

        let mode_temp = TempDir::new().expect("tempdir");
        let runtime = mode_temp.path().join(format!("mcp-cli-{}", current_uid()));
        fs::create_dir(&runtime).expect("runtime");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).expect("set mode");
        let paths = DaemonPaths::from_runtime_parent(mode_temp.path(), &id).expect("reuse");
        assert_eq!(
            fs::metadata(paths.runtime_dir).unwrap().mode() & 0o7777,
            0o700
        );

        let symlink_temp = TempDir::new().expect("tempdir");
        let symlink_runtime = symlink_temp
            .path()
            .join(format!("mcp-cli-{}", current_uid()));
        let target = symlink_temp.path().join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, &symlink_runtime).unwrap();
        assert!(DaemonPaths::from_runtime_parent(symlink_temp.path(), &id).is_err());

        let file_temp = TempDir::new().expect("tempdir");
        let file_runtime = file_temp.path().join(format!("mcp-cli-{}", current_uid()));
        fs::write(&file_runtime, b"not a directory").unwrap();
        assert!(DaemonPaths::from_runtime_parent(file_temp.path(), &id).is_err());

        let owner_temp = TempDir::new().expect("tempdir");
        let owner_runtime = owner_temp
            .path()
            .join(format!("mcp-cli-{}", current_uid().wrapping_add(1)));
        fs::create_dir(&owner_runtime).unwrap();
        assert!(
            DaemonPaths::from_runtime_parent_for_uid(
                owner_temp.path(),
                &id,
                current_uid().wrapping_add(1)
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_server_id_cannot_escape_runtime_root() {
        let temp = TempDir::new().expect("tempdir");
        for value in ["../escape", "a/b", "A", "0", "\n"] {
            assert!(
                DaemonPaths::from_runtime_parent(temp.path(), &ServerId(value.into())).is_err()
            );
        }
        assert!(!temp.path().join("escape.pid").exists());
    }

    #[test]
    fn safe_removal_validates_expected_type_and_symlink() {
        let temp = TempDir::new().expect("tempdir");
        let paths = DaemonPaths::from_runtime_parent(temp.path(), &server_id("alpha")).unwrap();

        fs::write(&paths.lock, b"lock").unwrap();
        fs::set_permissions(&paths.lock, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(paths.remove_lock().unwrap());
        assert!(!paths.remove_lock().unwrap());

        let listener = UnixListener::bind(&paths.socket).unwrap();
        assert!(paths.remove_socket().unwrap());
        drop(listener);

        fs::create_dir(&paths.pid).unwrap();
        assert!(paths.remove_pid().is_err());
        fs::remove_dir(&paths.pid).unwrap();

        let target = temp.path().join("outside");
        fs::write(&target, b"outside").unwrap();
        symlink(&target, &paths.pid).unwrap();
        assert!(paths.remove_pid().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"outside");
    }

    #[test]
    fn artifact_owner_validation_fails_closed() {
        let temp = TempDir::new().expect("tempdir");
        let file = temp.path().join("artifact");
        fs::write(&file, b"data").unwrap();
        let metadata = fs::symlink_metadata(&file).unwrap();
        let error = validate_artifact_metadata(
            &file,
            &metadata,
            metadata.uid().wrapping_add(1),
            ArtifactKind::RegularFile,
            true,
        );
        assert!(error.is_err());
    }
}
