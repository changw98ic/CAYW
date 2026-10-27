//! Real unmodified Pi npm extension path (F15 acceptance).
//!
//! Loads `@earendil-works/pi-coding-agent@0.84.1` `examples/extensions/hello.ts`
//! through the Node [`crate::PiCompatHost`] adapter (`npm/pi-npm-bridge.mjs`).
//! The TypeScript source is never rewritten; only the CAYW-side adapter
//! translates Pi ExtensionAPI ↔ CAYW external JSON protocol.

use crate::types::{PINNED_PI_CODING_AGENT, PINNED_PI_CODING_AGENT_VERSION, PiPackageId};
use cayw_ai::ToolDefinition;
use cayw_plugins::{PLUGIN_PROTOCOL_VERSION, PluginKind, PluginManifest};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Identity + discovery for the pinned real npm Pi hello extension.
pub struct PiNpmHelloExtension;

impl PiNpmHelloExtension {
    /// Same package id as the Pi ABI pin — this *is* that package's shipped example.
    pub const PACKAGE_NAME: &'static str = PINNED_PI_CODING_AGENT;
    pub const PACKAGE_VERSION: &'static str = PINNED_PI_CODING_AGENT_VERSION;
    /// Tool name registered by unmodified `examples/extensions/hello.ts`.
    pub const TOOL: &'static str = "hello";
    /// Relative path inside the installed npm package (never copied/rewritten).
    pub const EXTENSION_REL: &'static str =
        "node_modules/@earendil-works/pi-coding-agent/examples/extensions/hello.ts";

    pub fn package_id() -> PiPackageId {
        PiPackageId::new(Self::PACKAGE_NAME, Self::PACKAGE_VERSION)
    }

    pub fn crate_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    pub fn npm_dir() -> PathBuf {
        Self::crate_dir().join("npm")
    }

    pub fn bridge_script() -> PathBuf {
        Self::npm_dir().join("pi-npm-bridge.mjs")
    }

    pub fn extension_path() -> PathBuf {
        Self::npm_dir().join(Self::EXTENSION_REL)
    }

    pub fn expected_manifest() -> PluginManifest {
        PluginManifest {
            name: Self::PACKAGE_NAME.into(),
            version: Self::PACKAGE_VERSION.into(),
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            kind: PluginKind::External,
            capabilities: vec![ToolDefinition {
                name: Self::TOOL.into(),
                description: "A simple greeting tool".into(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name"],
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Name to greet"
                        }
                    }
                }),
            }],
            permissions: Default::default(),
        }
    }

    /// Resolve a Node ≥ 22.19 binary (Pi 0.84.1 engine requirement).
    pub fn resolve_node() -> Option<PathBuf> {
        if let Ok(explicit) = std::env::var("CAYW_PI_NODE") {
            let p = PathBuf::from(explicit);
            if p.is_file() {
                return Some(p);
            }
        }
        for candidate in ["node22", "node"] {
            if let Some(path) = which(candidate)
                && node_meets_pi_engine(&path)
            {
                return Some(path);
            }
        }
        None
    }

    /// Ensure `npm/node_modules` exists (runs `npm ci` once when missing).
    pub fn ensure_npm_deps() -> Result<(), String> {
        let npm_dir = Self::npm_dir();
        let marker = npm_dir.join("node_modules/@earendil-works/pi-coding-agent/package.json");
        if marker.is_file() && Self::extension_path().is_file() {
            return Ok(());
        }
        let npm = which("npm").ok_or_else(|| "npm not found on PATH".to_string())?;
        let status = Command::new(npm)
            .args(["ci", "--omit=dev", "--no-fund", "--no-audit"])
            .current_dir(&npm_dir)
            .status()
            .map_err(|e| format!("npm ci spawn failed: {e}"))?;
        if !status.success() {
            return Err(format!("npm ci failed with status {status}"));
        }
        if !Self::extension_path().is_file() {
            return Err(format!(
                "after npm ci, extension still missing: {}",
                Self::extension_path().display()
            ));
        }
        Ok(())
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = Path::new(dir).join(bin);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn node_meets_pi_engine(node: &Path) -> bool {
    let output = Command::new(node)
        .arg("-p")
        .arg("process.versions.node")
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let version = version.trim();
    let mut parts = version.split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    major > 22 || (major == 22 && minor >= 19)
}
