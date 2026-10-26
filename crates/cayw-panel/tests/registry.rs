use cayw_panel::*;
use std::sync::Arc;

fn official_files() -> PanelDescriptor {
    PanelDescriptor {
        id: "files".into(),
        title: "Files".into(),
        contributor: ContributorKind::Official,
        placement: PanelPlacement::Both,
        order: 10,
        description: Some("Workspace file tree".into()),
        hidden: false,
    }
}

fn third_party_db() -> PanelDescriptor {
    PanelDescriptor {
        id: "vendor:db".into(),
        title: "Database".into(),
        contributor: ContributorKind::ThirdParty,
        placement: PanelPlacement::SidebarOnly,
        order: 50,
        description: None,
        hidden: false,
    }
}

fn bottom_terminal() -> PanelDescriptor {
    PanelDescriptor {
        id: "terminal-bottom".into(),
        title: "Terminal".into(),
        contributor: ContributorKind::Official,
        placement: PanelPlacement::BottomOnly,
        order: 20,
        description: None,
        hidden: false,
    }
}

#[test]
fn official_and_third_party_share_one_registry() {
    let registry = Arc::new(PanelRegistry::new());
    let files = registry.register_panel(official_files()).unwrap();
    let db = registry.register_panel(third_party_db()).unwrap();
    let listed = registry.list_panels(None);
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "files");
    assert_eq!(listed[1].id, "vendor:db");
    assert_eq!(listed[0].contributor, ContributorKind::Official);
    assert_eq!(listed[1].contributor, ContributorKind::ThirdParty);
    drop(files);
    drop(db);
    assert!(registry.list().is_empty());
}

#[test]
fn rejects_duplicate_ids_across_kinds() {
    let registry = Arc::new(PanelRegistry::new());
    let _panel = registry.register_panel(official_files()).unwrap();
    let err = registry
        .register_file_viewer(FileViewerDescriptor {
            id: "files".into(),
            title: "Files viewer".into(),
            contributor: ContributorKind::ThirdParty,
            extensions: vec!["txt".into()],
            priority: 1,
        })
        .unwrap_err();
    assert!(matches!(err, PanelError::Conflict(_)));
}

#[test]
fn sidebar_guide_hides_bottom_only_and_hidden() {
    let registry = Arc::new(PanelRegistry::new());
    let _files = registry.register_panel(official_files()).unwrap();
    let _term = registry.register_panel(bottom_terminal()).unwrap();
    let _hidden = registry
        .register_panel(PanelDescriptor {
            id: "secret".into(),
            title: "Secret".into(),
            contributor: ContributorKind::Official,
            placement: PanelPlacement::Both,
            order: 1,
            description: None,
            hidden: true,
        })
        .unwrap();
    let guide = registry.guide_entries(PanelZone::Sidebar);
    assert_eq!(
        guide.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["files"]
    );
    let bottom = registry.guide_entries(PanelZone::Bottom);
    assert_eq!(
        bottom.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["files", "terminal-bottom"]
    );
}

#[test]
fn file_viewer_resolve_prefers_higher_priority_then_id() {
    let registry = Arc::new(PanelRegistry::new());
    let _low = registry
        .register_file_viewer(FileViewerDescriptor {
            id: "z-fallback".into(),
            title: "Fallback".into(),
            contributor: ContributorKind::Official,
            extensions: vec!["md".into(), "markdown".into()],
            priority: 1,
        })
        .unwrap();
    let _high = registry
        .register_file_viewer(FileViewerDescriptor {
            id: "a-rich".into(),
            title: "Rich Markdown".into(),
            contributor: ContributorKind::ThirdParty,
            extensions: vec!["md".into()],
            priority: 10,
        })
        .unwrap();
    let resolved = registry.resolve_file_viewer("docs/README.MD").unwrap();
    assert_eq!(resolved.id, "a-rich");
    assert_eq!(
        registry.resolve_file_viewer("notes.markdown").unwrap().id,
        "z-fallback"
    );
}

#[test]
fn handle_drop_and_generation_fence_unload() {
    let registry = Arc::new(PanelRegistry::new());
    let handle = registry.register_panel(official_files()).unwrap();
    let generation = handle.generation();
    let id = handle.id().to_owned();
    drop(handle);
    assert!(matches!(
        registry.describe(&id),
        Err(PanelError::NotFound(_))
    ));
    let again = registry.register_panel(official_files()).unwrap();
    assert_ne!(again.generation(), generation);
    let description = registry.unload(again.id()).unwrap();
    assert_eq!(description.panel.unwrap().id, "files");
}

#[test]
fn workspace_hooks_open_activate_close_across_zones() {
    let registry = Arc::new(PanelRegistry::new());
    let _files = registry.register_panel(official_files()).unwrap();
    let _term = registry.register_panel(bottom_terminal()).unwrap();
    let _db = registry.register_panel(third_party_db()).unwrap();
    let hooks = WorkspaceHooks::new(Arc::clone(&registry));

    let files = hooks
        .open(
            "session-1",
            OpenPanelRequest {
                panel_id: "files".into(),
                zone: Some(PanelZone::Sidebar),
                path: Some("src/lib.rs".into()),
                activate: true,
            },
        )
        .unwrap();
    assert!(files.active);
    assert_eq!(files.contributor, ContributorKind::Official);

    let db = hooks
        .open(
            "session-1",
            OpenPanelRequest {
                panel_id: "vendor:db".into(),
                zone: None,
                path: None,
                activate: true,
            },
        )
        .unwrap();
    assert_eq!(db.zone, PanelZone::Sidebar);
    let open = hooks
        .list_open("session-1", Some(PanelZone::Sidebar))
        .unwrap();
    assert_eq!(open.len(), 2);
    assert!(
        !open
            .iter()
            .find(|item| item.panel_id == "files")
            .unwrap()
            .active
    );
    assert!(
        open.iter()
            .find(|item| item.panel_id == "vendor:db")
            .unwrap()
            .active
    );

    let term = hooks
        .open(
            "session-1",
            OpenPanelRequest {
                panel_id: "terminal-bottom".into(),
                zone: None,
                path: None,
                activate: true,
            },
        )
        .unwrap();
    assert_eq!(term.zone, PanelZone::Bottom);
    assert!(matches!(
        hooks.open(
            "session-1",
            OpenPanelRequest {
                panel_id: "terminal-bottom".into(),
                zone: Some(PanelZone::Sidebar),
                path: None,
                activate: true,
            },
        ),
        Err(PanelError::ZoneNotAllowed(_))
    ));

    let activated = hooks.activate("session-1", &files.instance_id).unwrap();
    assert!(activated.active);
    let closed = hooks.close("session-1", &db.instance_id).unwrap();
    assert_eq!(closed.panel_id, "vendor:db");
    assert_eq!(hooks.list_open("session-1", None).unwrap().len(), 2);
}

#[test]
fn serde_roundtrip_descriptors() {
    let panel = official_files();
    let json = serde_json::to_value(&panel).unwrap();
    let back: PanelDescriptor = serde_json::from_value(json).unwrap();
    assert_eq!(back, panel);

    let viewer = FileViewerDescriptor {
        id: "markdown".into(),
        title: "Markdown".into(),
        contributor: ContributorKind::Official,
        extensions: vec!["md".into()],
        priority: 5,
    };
    let json = serde_json::to_string(&viewer).unwrap();
    let back: FileViewerDescriptor = serde_json::from_str(&json).unwrap();
    assert_eq!(back, viewer);
}
