//! Offline demo: official and third-party panels share one registry and hooks.

use cayw_panel::*;
use std::sync::Arc;

fn main() {
    let registry = Arc::new(PanelRegistry::new());
    let _files = registry
        .register_panel(PanelDescriptor {
            id: "files".into(),
            title: "Files".into(),
            contributor: ContributorKind::Official,
            placement: PanelPlacement::Both,
            order: 10,
            description: Some("Workspace file tree".into()),
            hidden: false,
        })
        .expect("register files");
    let _db = registry
        .register_panel(PanelDescriptor {
            id: "vendor:db".into(),
            title: "Database".into(),
            contributor: ContributorKind::ThirdParty,
            placement: PanelPlacement::SidebarOnly,
            order: 40,
            description: None,
            hidden: false,
        })
        .expect("register db");
    let _md = registry
        .register_file_viewer(FileViewerDescriptor {
            id: "markdown".into(),
            title: "Markdown".into(),
            contributor: ContributorKind::Official,
            extensions: vec!["md".into()],
            priority: 10,
        })
        .expect("register markdown viewer");

    let hooks = WorkspaceHooks::new(Arc::clone(&registry));
    let opened = hooks
        .open(
            "demo",
            OpenPanelRequest {
                panel_id: "vendor:db".into(),
                zone: None,
                path: None,
                activate: true,
            },
        )
        .expect("open third-party panel");

    println!("protocol={}", PANEL_PROTOCOL_VERSION);
    println!("registry={}", registry.registry_id());
    println!(
        "sidebar_guide={}",
        registry
            .guide_entries(PanelZone::Sidebar)
            .into_iter()
            .map(|panel| panel.id)
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "viewer_for_readme={}",
        registry
            .resolve_file_viewer("README.md")
            .map(|viewer| viewer.id)
            .unwrap_or_default()
    );
    println!(
        "opened={} zone={} contributor={:?}",
        opened.panel_id,
        opened.zone.as_str(),
        opened.contributor
    );
}
