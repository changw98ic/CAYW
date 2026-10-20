use std::{fs, path::Path, time::Duration};

fn wait(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture was not released"
        );
        std::thread::park_timeout(Duration::from_millis(5));
    }
}

fn main() {
    if Path::new("probe-control.json").is_file() {
        let control: serde_json::Value =
            serde_json::from_slice(&fs::read("probe-control.json").unwrap()).unwrap();
        fs::write(
            control["ready"].as_str().unwrap(),
            serde_json::to_vec(&serde_json::json!({
                "pid":std::process::id(), "cwd":std::env::current_dir().unwrap()
            }))
            .unwrap(),
        )
        .unwrap();
        wait(Path::new(control["release"].as_str().unwrap()));
        return;
    }
    match std::env::args().nth(1).as_deref() {
        // Publish the effect marker before the pid readiness file so observers
        // that wait on effect.pid never race a missing "effect" path.
        Some("effect") => {
            fs::write("effect", "started").unwrap();
            fs::write("effect.pid", std::process::id().to_string()).unwrap();
        }
        Some("wait") => {
            fs::write("effect", "started").unwrap();
            fs::write("effect.pid", std::process::id().to_string()).unwrap();
            loop {
                std::thread::park_timeout(Duration::from_secs(1));
            }
        }
        _ => panic!("unsupported fixture arguments"),
    }
}
