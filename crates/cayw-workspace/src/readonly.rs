//! Descriptor-anchored observations without a journal or write initialization.
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use std::{
    io,
    path::{Component, Path},
};

pub struct ReadOnlyWorkspace {
    root: Dir,
}

impl ReadOnlyWorkspace {
    /// Reuse a trusted root descriptor, including after its pathname is moved.
    pub fn from_dir(root: Dir) -> Self {
        Self { root }
    }

    pub fn open(path: &Path) -> Result<Self, crate::CapabilityError> {
        Ok(Self {
            root: crate::filesystem::open_root(path)?,
        })
    }

    fn parent(&self, path: &Path) -> io::Result<(Dir, Option<std::ffi::OsString>)> {
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => names.push(name.to_owned()),
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "expected relative workspace path",
                    ));
                }
            }
        }
        if path.as_os_str().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
        }
        let name = names.pop();
        let mut dir = self.root.try_clone()?;
        for name in names {
            dir = dir.open_dir_nofollow(name)?;
        }
        Ok((dir, name))
    }

    pub fn exists(&self, path: &Path) -> io::Result<bool> {
        let result = (|| {
            let (dir, name) = self.parent(path)?;
            let Some(name) = name else {
                return Ok(true);
            };
            let metadata = dir.symlink_metadata(name)?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "symbolic link is not an observable workspace entry",
                ));
            }
            Ok(true)
        })();
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            result => result,
        }
    }

    pub fn open_file(&self, path: &Path) -> io::Result<std::fs::File> {
        let (dir, name) = self.parent(path)?;
        let name =
            name.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "expected file"))?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        let file = dir.open_with(name, &options)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected regular file",
            ));
        }
        Ok(file.into_std())
    }
}
