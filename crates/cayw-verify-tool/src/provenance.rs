//! Shared build, executable and platform evidence for every verifyTool mode.
use crate::measurement::sha256;
use serde_json::{Value, json};
use std::{env, fs, io};

#[path = "build_receipt.rs"]
pub mod build_receipt;

pub fn build_binding() -> Value {
    let source = env!("CAYW_VERIFY_TOOL_SOURCE_MANIFEST_SHA256");
    json!({
        "status":if source.is_empty() { "unbound" } else { "build_verified_listed_inputs" },
        "source_manifest_sha256":if source.is_empty() { None } else { Some(source) },
        "source_manifest_verification":"build script hashes listed file bytes during compilation; Cargo dep-info closure must be checked independently",
        "source_binding_authority":"build-script-file-content","listed_inputs_verified":!source.is_empty(),
        "build_input_completeness":"pending", "source_verified_by_binary":false,
        "source_inputs":serde_json::from_str::<Value>(include_str!(concat!(env!("OUT_DIR"),"/source-inputs.json"))).expect("build-generated JSON"),
        "version":env!("CARGO_PKG_VERSION"),
        "build_target":env!("CAYW_VERIFY_TOOL_BUILD_TARGET"),
        "toolchain":env!("CAYW_VERIFY_TOOL_RUSTC_VERSION"),
        "profile":env!("CAYW_VERIFY_TOOL_BUILD_PROFILE"),
        "opt_level":env!("CAYW_VERIFY_TOOL_OPT_LEVEL"),
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
