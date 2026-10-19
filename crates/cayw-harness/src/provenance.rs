//! Shared build, executable and platform evidence for every Harness mode.
use crate::measurement::sha256;
use serde_json::{Value, json};
use std::{env, fs, io};

pub fn build_binding() -> Value {
    let source = env!("CAYW_HARNESS_SOURCE_MANIFEST_SHA256");
    json!({
        "status":if source.is_empty() { "unbound" } else { "explicit_build_input" },
        "source_manifest_sha256":if source.is_empty() { None } else { Some(source) },
        "source_manifest_verification":"caller supplies and retains the manifest; this binary records the compile-time digest and does not reconstruct or verify a source tree",
        "source_binding_authority":"caller-declared","source_verified_by_binary":false,
        "version":env!("CARGO_PKG_VERSION"),
        "build_target":env!("CAYW_HARNESS_BUILD_TARGET"),
        "toolchain":env!("CAYW_HARNESS_RUSTC_VERSION"),
        "profile":env!("CAYW_HARNESS_BUILD_PROFILE"),
        "opt_level":env!("CAYW_HARNESS_OPT_LEVEL"),
    })
}

pub fn executable() -> io::Result<Value> {
    let path = env::current_exe()?;
    Ok(json!({"path":path,"sha256":sha256(&fs::read(&path)?)}))
}

pub fn platform() -> Value {
    let mut value = json!({"os":env::consts::OS,"arch":env::consts::ARCH,"family":env::consts::FAMILY,
        "kernel":null,"kernel_release":null,"kernel_version":null,"machine":null});
    #[cfg(unix)]
    {
        let mut name = std::mem::MaybeUninit::<libc::utsname>::zeroed();
        // SAFETY: name is aligned, zero-initialized, writable storage for utsname.
        if unsafe { libc::uname(name.as_mut_ptr()) } == 0 {
            // SAFETY: uname succeeded; its string arrays are NUL-terminated.
            let name = unsafe { name.assume_init() };
            for (key, text) in [
                ("kernel", &name.sysname[..]),
                ("kernel_release", &name.release[..]),
                ("kernel_version", &name.version[..]),
                ("machine", &name.machine[..]),
            ] {
                let bytes: Vec<_> = text
                    .iter()
                    .take_while(|byte| **byte != 0)
                    .map(|byte| *byte as u8)
                    .collect();
                value[key] = String::from_utf8_lossy(&bytes).into_owned().into();
            }
        }
    }
    value
}
