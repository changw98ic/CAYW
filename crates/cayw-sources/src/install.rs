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
            let cleanup = rollback(registry, &installed).await;
            return Err(with_cleanup(failure("MCP install cancelled"), cleanup));
        }
        let id = entry.spec.id.clone();
        let origin = entry.spec.origin.clone();
        let prepared = match ManagedStdio::prepare(entry.spec, cancel.child_token()).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let cleanup = rollback(registry, &installed).await;
                return Err(with_cleanup(error, cleanup));
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
            let mut cleanup = Vec::new();
            if let Some(error) = close_control(&id, &control).await {
                cleanup.push(error);
            }
            cleanup.extend(rollback(registry, &installed).await);
            return Err(with_cleanup(error, cleanup));
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

async fn close_control(id: &str, control: &ManagedControl) -> Option<String> {
    control.close();
    match tokio::time::timeout(std::time::Duration::from_secs(5), control.wait_closed()).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("MCP source {id} cleanup: {error}")),
        Err(_) => Some(format!("MCP source {id} cleanup timed out")),
    }
}

async fn rollback(registry: &Registry, installed: &[InstalledServer]) -> Vec<String> {
    let mut failures = Vec::new();
    for entry in installed.iter().rev() {
        if let Err(error) = registry.disconnect(&entry.id) {
            failures.push(format!("MCP source {} disconnect: {error}", entry.id));
        }
        if let Some(error) = close_control(&entry.id, &entry.control).await {
            failures.push(error);
        }
    }
    failures
}

fn with_cleanup(mut primary: CapabilityError, failures: Vec<String>) -> CapabilityError {
    if !failures.is_empty() {
        primary.message.push_str("; cleanup unconfirmed: ");
        primary.message.push_str(&failures.join("; "));
        primary.outcome_unknown = true;
    }
    primary
}

fn failure(message: &str) -> CapabilityError {
    CapabilityError::new(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_prepare_unknown_survives_empty_rollback() {
        let error = with_cleanup(
            CapabilityError::unknown("managed MCP probe cleanup unconfirmed"),
            Vec::new(),
        );
        assert!(error.outcome_unknown);
        assert_eq!(error.message, "managed MCP probe cleanup unconfirmed");
    }

    #[test]
    fn cleanup_failure_preserves_primary_and_reports_unknown() {
        let error = with_cleanup(
            failure("registration failed"),
            vec!["source one timed out".into()],
        );
        assert!(error.outcome_unknown);
        assert!(error.message.contains("registration failed"));
        assert!(error.message.contains("source one timed out"));
        let known = with_cleanup(failure("registration failed"), Vec::new());
        assert!(!known.outcome_unknown);
        assert_eq!(known.message, "registration failed");
    }
}
