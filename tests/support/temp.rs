use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::{Builder, TempDir};

/// Owns isolated cwd, home, XDG, and daemon-root paths for a test.
///
/// Child commands receive only a minimal environment, so proxy credentials and
/// the real user daemon directory are never inherited accidentally.
#[derive(Debug)]
pub struct IsolatedTestDir {
    root: TempDir,
    cwd: PathBuf,
    home: PathBuf,
    xdg_config: PathBuf,
    tmp: PathBuf,
}

impl IsolatedTestDir {
    pub fn new() -> io::Result<Self> {
        let root = Builder::new().prefix("mcp-cli-test-").tempdir()?;
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        let xdg_config = root.path().join("xdg-config");
        let tmp = root.path().join("tmp");
        for directory in [&cwd, &home, &xdg_config, &tmp] {
            fs::create_dir(directory)?;
        }
        Ok(Self {
            root,
            cwd,
            home,
            xdg_config,
            tmp,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.path().join(relative)
    }

    pub fn isolated_environment(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("HOME", self.home.as_os_str().to_owned()),
            ("USERPROFILE", self.home.as_os_str().to_owned()),
            ("XDG_CONFIG_HOME", self.xdg_config.as_os_str().to_owned()),
            ("TMPDIR", self.tmp.as_os_str().to_owned()),
            ("TEMP", self.tmp.as_os_str().to_owned()),
            ("TMP", self.tmp.as_os_str().to_owned()),
        ]
    }

    pub fn configure_command(&self, command: &mut Command) {
        let path = std::env::var_os("PATH");
        let system_root = std::env::var_os("SystemRoot");

        command.env_clear().current_dir(&self.cwd);
        command.envs(self.isolated_environment());
        if let Some(path) = path {
            command.env("PATH", path);
        }
        if let Some(system_root) = system_root {
            command.env("SystemRoot", system_root);
        }
    }

    pub fn configure_direct_command(&self, command: &mut Command) {
        self.configure_command(command);
        command.env("MCP_NO_DAEMON", "1");
    }
}
