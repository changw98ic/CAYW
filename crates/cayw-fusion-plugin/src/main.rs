//! `cayw-fusion-plugin`: External plugin process for CAYW protocol v1.
//!
//! Usage: `cayw-fusion-plugin [--allow-fault-injection] [--max-line-bytes N]`
//! or `cayw-fusion-plugin --print-manifest`. Speaks JSON lines on stdin/stdout;
//! stderr carries diagnostics only.

use cayw_fusion_plugin::{ServeOptions, manifest, serve};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut options = ServeOptions::default();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--print-manifest" => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manifest()).expect("manifest JSON")
                );
                return ExitCode::SUCCESS;
            }
            "--allow-fault-injection" => options.allow_fault_injection = true,
            "--max-line-bytes" => match arguments.next().and_then(|value| value.parse().ok()) {
                Some(limit) if limit > 0 => options.max_line_bytes = limit,
                _ => return usage("--max-line-bytes needs a positive integer"),
            },
            other => return usage(&format!("unknown argument {other}")),
        }
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cayw-fusion-plugin: cannot start runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(serve(tokio::io::stdin(), tokio::io::stdout(), options));
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("cayw-fusion-plugin: {error}");
            1
        }
    };
    // Tokio's stdin may still own a blocked read on a helper thread; exit
    // directly instead of waiting for it during runtime shutdown.
    std::process::exit(code)
}

fn usage(message: &str) -> ExitCode {
    eprintln!(
        "cayw-fusion-plugin: {message}\nusage: cayw-fusion-plugin [--allow-fault-injection] [--max-line-bytes N] | --print-manifest"
    );
    ExitCode::from(2)
}
