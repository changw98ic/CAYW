//! Product entry: land imported MCP specs in a live Bridge Registry.
//!
//! `import_mcp_servers_*` only classifies accepted vs skipped. Callers that want
//! Runtime/Host to see those sources must prepare each accepted absolute-path
//! local stdio server and register its single handle. This module is that step.

use crate::config::{ImportedServer, SkippedServer, import_mcp_servers_json};
use crate::lifecycle::{ManagedControl, ManagedStdio};
use crate::stdio::ServerInfo;
use cayw_ai::CancellationToken;
use cayw_bridge::{Registry, SourceIdentity};
use cayw_codemode::CapabilityError;
use std::path::Path;

/// One source that finished prepare and is registered in the Registry.
#[derive(Clone)]
pub struct InstalledServer {
    pub id: String,
    pub control: ManagedControl,
    pub server_info: ServerInfo,
}

/// Result of the product import → prepare → Registry path.
pub struct InstallReport {
    pub installed: Vec<InstalledServer>,
    pub skipped: Vec<SkippedServer>,
}

/// Prepare each imported server and register its handle into `registry`.
///
/// On failure after some servers succeeded, previously registered sources are
/// disconnected and their controls are closed before the error is returned.
pub async fn install_imported(
    imported: Vec<ImportedServer>,
    registry: &Registry,
    cancel: CancellationToken,
) -> Result<Vec<InstalledServer>, CapabilityError> {
    let mut installed = Vec::with_capacity(imported.len());
    for entry in imported {
        if cancel.is_cancelled() {
            rollback(registry, &installed).await;
            return Err(failure("MCP install cancelled"));
        }
        let id = entry.spec.id.clone();
        let origin = entry.spec.origin.clone();
        let prepared = match ManagedStdio::prepare(entry.spec, cancel.child_token()).await {
            Ok(prepared) => prepared,
            Err(error) => {
                rollback(registry, &installed).await;
                return Err(error);
            }
        };
        let server_info = prepared.server_info().clone();
        let control = prepared.control();
        let (manifest, handle) = prepared.into_parts();
        if let Err(error) = registry.register(
            SourceIdentity {
                id: id.clone(),
                version: server_info.version.clone(),
                origin,
            },
            manifest,
            handle,
        ) {
            control.close();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), control.wait_closed())
                .await;
            rollback(registry, &installed).await;
            return Err(error);
        }
        installed.push(InstalledServer {
            id,
            control,
            server_info,
        });
    }
    Ok(installed)
}

/// Read an external `mcpServers` JSON file, import, prepare, and register.
///
/// Skipped non-stdio / non-absolute entries are returned in `skipped` without
/// failing the overall install. An empty accepted list yields an empty
/// `installed` list (still Ok).
pub async fn install_mcp_servers_file(
    path: &Path,
    default_directory: &Path,
    registry: &Registry,
    cancel: CancellationToken,
) -> Result<InstallReport, CapabilityError> {
    if !path.is_absolute() || !path.is_file() {
        return Err(failure(
            "MCP servers file must be an absolute existing regular file",
        ));
    }
    let text =
        std::fs::read_to_string(path).map_err(|_| failure("MCP servers file cannot be read"))?;
    let imported = import_mcp_servers_json(&text, Some(path), default_directory)?;
    let skipped = imported.skipped;
    let installed = install_imported(imported.accepted, registry, cancel).await?;
    Ok(InstallReport { installed, skipped })
}

async fn rollback(registry: &Registry, installed: &[InstalledServer]) {
    for entry in installed.iter().rev() {
        let _ = registry.disconnect(&entry.id);
        entry.control.close();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            entry.control.wait_closed(),
        )
        .await;
    }
}

fn failure(message: &str) -> CapabilityError {
    CapabilityError::new(message)
}
