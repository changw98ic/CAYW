//! Scope-local protection for a trusted top-level directory.
use crate::{failure, io_error};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use cayw_codemode::CapabilityError;

#[derive(Clone, Default)]
pub struct DirectoryWritePolicy {
    read_only: Option<String>,
}

impl DirectoryWritePolicy {
    pub fn read_only(name: &str) -> Result<Self, CapabilityError> {
        let parts = crate::filesystem::components(name, false)?;
        if parts.len() != 1 {
            return Err(failure("read-only directory must be a top-level name"));
        }
        Ok(Self {
            read_only: Some(name.to_owned()),
        })
    }

    /// Resolve once, check the actual first directory, and return the same
    /// parent handle that the caller uses for the mutation.
    pub fn parent<'a>(
        &self,
        root: &Dir,
        parts: &[&'a str],
    ) -> Result<(Dir, &'a str), CapabilityError> {
        let (leaf, parents) = parts
            .split_last()
            .ok_or_else(|| failure("path must name an entry"))?;
        let mut dir = root.try_clone().map_err(io_error)?;
        if let Some(protected) = &self.read_only {
            let first = parts[0];
            if first == protected || case_alias(root, first, protected)? {
                return Err(failure("directory is read-only in this scope"));
            }
            // On a case-sensitive volume differently spelled directories can
            // coexist. Compare filesystem identity, not a lowercase blacklist.
            #[cfg(unix)]
            {
                use cap_std::fs::MetadataExt;
                let protected_metadata = match root.symlink_metadata(protected) {
                    Ok(metadata) => Some(metadata),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(io_error(error)),
                };
                if let Some(metadata) = protected_metadata {
                    let candidate = if parents.is_empty() {
                        match root.symlink_metadata(first) {
                            Ok(metadata) => Some(metadata),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                            Err(error) => return Err(io_error(error)),
                        }
                    } else {
                        dir = root.open_dir_nofollow(first).map_err(io_error)?;
                        Some(dir.dir_metadata().map_err(io_error)?)
                    };
                    if candidate.is_some_and(|candidate| {
                        candidate.dev() == metadata.dev() && candidate.ino() == metadata.ino()
                    }) {
                        return Err(failure("directory is read-only in this scope"));
                    }
                    if !parents.is_empty() {
                        for component in &parents[1..] {
                            dir = dir.open_dir_nofollow(component).map_err(io_error)?;
                        }
                        return Ok((dir, leaf));
                    }
                }
            }
        }
        for component in parents {
            dir = dir.open_dir_nofollow(component).map_err(io_error)?;
        }
        Ok((dir, leaf))
    }
}

fn case_alias(root: &Dir, candidate: &str, protected: &str) -> Result<bool, CapabilityError> {
    if !candidate.eq_ignore_ascii_case(protected) {
        return Ok(false);
    }
    // Also protect the first mkdir before memory exists. No probing files are
    // created, and an independent Memory directory on a sensitive volume works.
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let sensitive = unsafe { libc::fpathconf(root.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
        match sensitive {
            0 => Ok(true),
            value if value > 0 => Ok(false),
            _ => Err(failure(
                "could not determine workspace filename case sensitivity",
            )),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        Ok(false)
    }
}
