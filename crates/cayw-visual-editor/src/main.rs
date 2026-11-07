//! Command-line entry point for the optional visual editor service.
//!
//! The bearer token is read from an environment variable and is never printed,
//! stored in a URL or accepted as a command-line value. On startup the process
//! prints exactly one JSON line with the loopback URLs and then runs until
//! `SIGINT` or `SIGTERM`.

use cayw_visual_editor::{EditorConfig, start};
use std::{env, io::Write, net::SocketAddr, path::PathBuf, process::ExitCode};

const USAGE: &str = "\
cayw-visual-editor — edit an existing project page in place

Usage:
  cayw-visual-editor --project PATH --state-dir PATH [OPTIONS]

Required:
  --project PATH        Project root that contains the page and its assets
  --state-dir PATH      Audit directory; must be outside the project

Options:
  --page NAME           Page path relative to the project (default: index.html)
  --addr ADDR           Editor loopback address (default: 127.0.0.1:0)
  --preview-addr ADDR   Preview loopback address (default: 127.0.0.1:0)
  --framework NAME      html (default), react or vue; frameworks require Vite
  --node PATH           Node executable for frameworks (default: node)
  --companion PATH      Optional framework compiler script location
  --token-env NAME      Environment variable holding the editor token
                        (default: CAYW_EDITOR_TOKEN; at least 32 visible ASCII)
  -h, --help            Show this help

The token must contain at least 32 visible ASCII characters. The process prints
one JSON line with editor_url, preview_url and normal_preview_url;
it never prints the token.
";

struct Options {
    project: PathBuf,
    page: String,
    state_dir: PathBuf,
    addr: SocketAddr,
    preview_addr: SocketAddr,
    token_env: String,
    framework: Option<String>,
    node: PathBuf,
    companion: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let options = match parse(arguments.into_iter()) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("cayw-visual-editor: {message}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let token = match env::var(&options.token_env) {
        Ok(token) => token,
        Err(_) => {
            eprintln!(
                "cayw-visual-editor: environment variable {} is not set",
                options.token_env
            );
            return ExitCode::from(2);
        }
    };
    let handle = match start(EditorConfig {
        project_root: options.project,
        page: options.page,
        state_dir: options.state_dir,
        editor_addr: options.addr,
        preview_addr: options.preview_addr,
        token,
        framework: options.framework,
        node: options.node,
        companion: options.companion,
        codeintel: None,
    })
    .await
    {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("cayw-visual-editor: {error}");
            return ExitCode::FAILURE;
        }
    };

    let line = serde_json::json!({
        "editor_url": &handle.editor_url,
        "preview_url": &handle.preview_url,
        "normal_preview_url": &handle.normal_preview_url,
    });
    println!("{line}");
    let _ = std::io::stdout().flush();

    shutdown_signal().await;
    handle.shutdown().await;
    ExitCode::SUCCESS
}

fn parse(mut arguments: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut project = None;
    let mut page = "index.html".to_owned();
    let mut state_dir = None;
    let mut addr = "127.0.0.1:0".to_owned();
    let mut preview_addr = "127.0.0.1:0".to_owned();
    let mut token_env = "CAYW_EDITOR_TOKEN".to_owned();
    let mut framework = None;
    let mut node = PathBuf::from("node");
    let mut companion = cayw_visual_editor::default_companion();
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--project" => project = Some(PathBuf::from(value()?)),
            "--page" => page = value()?,
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--addr" => addr = value()?,
            "--preview-addr" => preview_addr = value()?,
            "--token-env" => token_env = value()?,
            "--framework" => {
                let selected = value()?;
                framework = match selected.as_str() {
                    "html" => None,
                    "react" | "vue" => Some(selected),
                    _ => return Err("--framework must be html, react or vue".to_owned()),
                };
            }
            "--node" => node = PathBuf::from(value()?),
            "--companion" => companion = PathBuf::from(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let project = project.ok_or("--project is required")?;
    let state_dir = state_dir.ok_or("--state-dir is required")?;
    let addr = addr
        .parse()
        .map_err(|_| format!("--addr must be a socket address, got {addr}"))?;
    let preview_addr = preview_addr
        .parse()
        .map_err(|_| format!("--preview-addr must be a socket address, got {preview_addr}"))?;
    if token_env.is_empty() {
        return Err("--token-env must name an environment variable".to_owned());
    }
    Ok(Options {
        project,
        page,
        state_dir,
        addr,
        preview_addr,
        token_env,
        framework,
        node,
        companion,
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(arguments: &[&str]) -> Result<Options, String> {
        parse(arguments.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn defaults_match_the_documented_cli() {
        let parsed = options(&["--project", "/tmp/p", "--state-dir", "/tmp/s"]).unwrap();
        assert_eq!(parsed.page, "index.html");
        assert_eq!(parsed.addr.port(), 0);
        assert_eq!(parsed.preview_addr.port(), 0);
        assert_eq!(parsed.token_env, "CAYW_EDITOR_TOKEN");
        assert!(parsed.framework.is_none());
        assert_eq!(parsed.node, PathBuf::from("node"));
    }

    #[test]
    fn missing_values_are_rejected() {
        assert!(options(&["--project"]).is_err());
        assert!(options(&["--project", "/tmp/p"]).is_err());
        assert!(options(&["--state-dir", "/tmp/s"]).is_err());
    }

    #[test]
    fn unknown_options_are_rejected_without_accepting_a_token_value() {
        assert!(options(&["--token", "secret"]).is_err());
        assert!(
            options(&[
                "--project",
                "/tmp/p",
                "--state-dir",
                "/tmp/s",
                "--page",
                "site/index.html"
            ])
            .is_ok()
        );
    }
}
