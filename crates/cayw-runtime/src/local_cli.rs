//! Local CLI state and discovery. This does not grant model or tool permissions.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::SocketAddr,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct LocalPaths {
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
    pub database: PathBuf,
    pub config_file: PathBuf,
    pub token_file: PathBuf,
    pub connection_file: PathBuf,
    state_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Connection {
    address: SocketAddr,
    workspace: PathBuf,
    token_file: PathBuf,
}

impl LocalPaths {
    pub fn from_environment(workspace: impl AsRef<Path>) -> io::Result<Self> {
        let workspace = workspace.as_ref().canonicalize()?;
        if !workspace.is_dir() {
            return Err(invalid("workspace must be an existing directory"));
        }
        let data_dir = match std::env::var_os("CAYW_HOME") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            Some(_) => return Err(invalid("CAYW_HOME must be an absolute nonempty path")),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        invalid("HOME is unavailable; set CAYW_HOME to an absolute path")
                    })?,
            )
            .join(".cayw"),
        };
        if !data_dir.is_absolute() {
            return Err(invalid("CAYW_HOME and HOME must be absolute paths"));
        }
        let key = format!("{:x}", Sha256::digest(workspace.as_os_str().as_bytes()));
        let state_dir = data_dir.join("workspaces").join(key);
        Ok(Self {
            database: state_dir.join("session.sqlite"),
            config_file: data_dir.join("config.json"),
            token_file: data_dir.join("ipc.token"),
            connection_file: state_dir.join("connection.json"),
            data_dir,
            workspace,
            state_dir,
        })
    }

    /// Create only missing files. Existing invalid or public state is never replaced.
    pub fn initialize(&self) -> io::Result<()> {
        private_directory(&self.data_dir)?;
        let initialization = private_lock(&self.data_dir.join("init.lock"))?;
        initialization.lock()?;
        private_directory(&self.data_dir.join("workspaces"))?;
        private_directory(&self.state_dir)?;
        create_private(&self.config_file, b"{\n  \"permissions\": []\n}\n")?;
        self.validate_config()?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        create_private(&self.token_file, token.as_bytes())?;
        self.token()?;
        Ok(())
    }

    pub fn validate_config(&self) -> io::Result<()> {
        read_private(&self.config_file, 1024 * 1024)?;
        Ok(())
    }

    pub fn token(&self) -> io::Result<String> {
        check_directory(&self.data_dir)?;
        let bytes = read_private(&self.token_file, 4096)?;
        let token = String::from_utf8(bytes).map_err(|_| invalid("local IPC token is invalid"))?;
        if !(32..=4096).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(invalid(
                "local IPC token is invalid; existing token was not replaced",
            ));
        }
        Ok(token)
    }

    pub fn connect(&self) -> io::Result<(SocketAddr, String)> {
        check_directory(&self.data_dir)?;
        check_directory(&self.data_dir.join("workspaces"))?;
        check_directory(&self.state_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                invalid("no local service for this workspace; start `cayw serve`")
            } else {
                error
            }
        })?;
        let bytes = read_private(&self.connection_file, 16 * 1024).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                invalid("no local service for this workspace; start `cayw serve`")
            } else {
                error
            }
        })?;
        let connection: Connection = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("local connection record is invalid; restart `cayw serve`"))?;
        if connection.workspace != self.workspace
            || connection.token_file != self.token_file
            || !connection.address.ip().is_loopback()
            || connection.address.port() == 0
        {
            return Err(invalid(
                "local connection record does not match this workspace",
            ));
        }
        Ok((connection.address, self.token()?))
    }

    pub fn acquire_server(&self) -> io::Result<LocalServer> {
        check_directory(&self.data_dir)?;
        check_directory(&self.data_dir.join("workspaces"))?;
        check_directory(&self.state_dir)?;
        let file = private_lock(&self.state_dir.join("server.lock"))?;
        file.try_lock()
            .map_err(|_| invalid("local server already running for this workspace"))?;
        Ok(LocalServer {
            _lock: file,
            paths: self.clone(),
            published: None,
        })
    }
}

pub struct LocalServer {
    _lock: File,
    paths: LocalPaths,
    published: Option<(u64, u64)>,
}

impl LocalServer {
    pub fn publish(&mut self, address: SocketAddr) -> io::Result<()> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(invalid(
                "local service address must use loopback and a nonzero port",
            ));
        }
        match fs::symlink_metadata(&self.paths.connection_file) {
            Ok(_) => {
                open_private(&self.paths.connection_file)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let bytes = serde_json::to_vec(&Connection {
            address,
            workspace: self.paths.workspace.clone(),
            token_file: self.paths.token_file.clone(),
        })?;
        let temporary = self
            .paths
            .state_dir
            .join(format!(".connection-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            let metadata = file.metadata()?;
            fs::rename(&temporary, &self.paths.connection_file)?;
            self.published = Some((metadata.dev(), metadata.ino()));
            File::open(&self.paths.state_dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if let Some(identity) = self.published
            && let Ok(metadata) = fs::symlink_metadata(&self.paths.connection_file)
            && metadata.is_file()
            && (metadata.dev(), metadata.ino()) == identity
        {
            let _ = fs::remove_file(&self.paths.connection_file);
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn check_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(invalid(
            "local CLI directory must be a private directory (0700), without symlinks",
        ));
    }
    Ok(())
}

fn private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => check_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path)?;
            check_directory(path)
        }
        Err(error) => Err(error),
    }
}

fn open_private(path: &Path) -> io::Result<File> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.permissions().mode() & 0o077 != 0 || before.nlink() != 1 {
        return Err(invalid(
            "local CLI file must be a private regular file (0600), without links",
        ));
    }
    let file = File::open(path)?;
    let after = file.metadata()?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(invalid("local CLI file changed while opening"));
    }
    Ok(file)
}

fn read_private(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_private(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("local CLI file exceeds its size limit"));
    }
    Ok(bytes)
}

fn private_lock(path: &Path) -> io::Result<File> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => open_private(path),
        Err(error) => Err(error),
    }
}

/// Explain how to configure the model selected by this CLI host.
pub fn model_setup_hint(config_file: Option<&Path>) -> String {
    match config_file {
        Some(path) => format!(
            "尚未配置模型。配置文件：{}。运行 `cayw configure --config {}` 逐项配置模型。设置所选密钥环境变量后，重新启动 `cayw serve`。",
            path.display(),
            shell_path(path)
        ),
        None => "尚未配置模型。运行 `cayw configure` 完成配置，然后在当前启动命令中加入 `--config <配置文件路径>`。设置所选密钥环境变量后重新启动服务。".into(),
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

/// Replace a configuration only when it still matches the user's initial read.
pub fn save_config(path: &Path, expected: Option<&[u8]>, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("配置文件超过 1 MiB，未修改文件"));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let initialization = private_lock(&parent.join("init.lock"))?;
    initialization.lock()?;
    let current = match read_private(path, 1024 * 1024) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if current.as_deref() != expected {
        return Err(invalid(
            "配置文件已被修改；未覆盖文件，请重新运行 `cayw configure`",
        ));
    }
    write_private(path, bytes)
}

// The root initialization lock serializes creation. Publish only complete files.
fn create_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            open_private(path)?;
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    write_private(path, bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(".init-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
