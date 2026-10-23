use crate::{Failure, Result};
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

pub const USAGE: &str = "cayw-client --mode sdk|ipc [connection options] COMMAND [arguments]\n\nSDK: --db PATH --workspace DIR --config PATH\n     The host JSON must include model {service,api,id,base_url,api_version,max_output_tokens,api_key_env}. Only its selected credential environment variable is read.\nIPC: --address 127.0.0.1:PORT --token-env ENV\n\ncreate [--label TEXT]\nget|tasks|recover --session ID\ncancel --session ID --task ID\naudit --session ID [--after N --limit N]\nevents --session ID [--after N --limit N --wait-ms N --cursor-file PATH]\nprompt --session ID --text TEXT [--cursor-file PATH]\napprove --session ID --approval ID --decision allow_once|allow_task|deny\n\nOutput is JSONL. A prompt is submitted once. No automatic approval or replay.\nPrompt reads events on a separate connection. Ctrl+C requests cancellation and\ncontinues waiting for the original task and process cleanup. Save the cursor\nonly after a complete contiguous page is written and flushed. A saved cursor\nbelongs to one Session. Reuse the Session ID for subsequent user turns.\n";

pub enum Connection {
    Sdk {
        db: PathBuf,
        workspace: PathBuf,
        config: Option<PathBuf>,
    },
    Ipc {
        address: SocketAddr,
        token_env: String,
    },
}
pub struct Options {
    pub connection: Connection,
    pub command: String,
    pub session: Option<String>,
    pub task: Option<String>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub approval: Option<String>,
    pub decision: Option<String>,
    pub after: Option<u64>,
    pub limit: usize,
    pub wait_ms: u64,
    pub cursor_file: Option<PathBuf>,
}
impl Options {
    pub fn parse(args: Vec<String>) -> Result<Self> {
        let mut flags = BTreeMap::new();
        let mut command = None;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg.starts_with("--") {
                if !matches!(
                    arg.as_str(),
                    "--mode"
                        | "--db"
                        | "--workspace"
                        | "--config"
                        | "--address"
                        | "--token-env"
                        | "--session"
                        | "--task"
                        | "--label"
                        | "--text"
                        | "--approval"
                        | "--decision"
                        | "--after"
                        | "--limit"
                        | "--wait-ms"
                        | "--cursor-file"
                ) {
                    return Err(Failure::invalid("unknown option; use --help"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| Failure::invalid("option requires a value"))?;
                if flags.insert(arg, value).is_some() {
                    return Err(Failure::invalid("duplicate option"));
                }
            } else if command.replace(arg).is_some() {
                return Err(Failure::invalid("expected exactly one command"));
            }
        }
        let command = command.ok_or_else(|| Failure::invalid("command is required; use --help"))?;
        if !matches!(
            command.as_str(),
            "create"
                | "get"
                | "tasks"
                | "audit"
                | "events"
                | "prompt"
                | "approve"
                | "cancel"
                | "recover"
        ) {
            return Err(Failure::invalid("unknown command; use --help"));
        }
        let mode = take(&mut flags, "--mode")?;
        let connection = match mode.as_str() {
            "sdk" => Connection::Sdk {
                db: take(&mut flags, "--db")?.into(),
                workspace: take(&mut flags, "--workspace")?.into(),
                config: flags.remove("--config").map(Into::into),
            },
            "ipc" => {
                let address: SocketAddr = take(&mut flags, "--address")?
                    .parse()
                    .map_err(|_| Failure::invalid("address must be a numeric loopback IP:PORT"))?;
                if !address.ip().is_loopback() || address.port() == 0 {
                    return Err(Failure::invalid(
                        "address must be a numeric loopback IP with a nonzero port",
                    ));
                }
                Connection::Ipc {
                    address,
                    token_env: take(&mut flags, "--token-env")?,
                }
            }
            _ => return Err(Failure::invalid("mode must be sdk or ipc")),
        };
        let session = if command == "create" {
            None
        } else {
            Some(take(&mut flags, "--session")?)
        };
        let task = if command == "cancel" {
            Some(take(&mut flags, "--task")?)
        } else {
            None
        };
        let label = if command == "create" {
            flags.remove("--label")
        } else {
            None
        };
        let text = if command == "prompt" {
            Some(take(&mut flags, "--text")?)
        } else {
            None
        };
        let (approval, decision) = if command == "approve" {
            let approval = take(&mut flags, "--approval")?;
            let decision = take(&mut flags, "--decision")?;
            if !matches!(decision.as_str(), "allow_once" | "allow_task" | "deny") {
                return Err(Failure::invalid(
                    "decision must be allow_once, allow_task or deny",
                ));
            }
            (Some(approval), Some(decision))
        } else {
            (None, None)
        };
        let after = if matches!(command.as_str(), "events" | "audit") {
            flags
                .remove("--after")
                .map(|n| {
                    n.parse()
                        .map_err(|_| Failure::invalid("after must be an unsigned integer"))
                })
                .transpose()?
        } else {
            None
        };
        let limit = if matches!(command.as_str(), "events" | "audit") {
            flags
                .remove("--limit")
                .map(|n| {
                    n.parse()
                        .map_err(|_| Failure::invalid("limit must be an integer from 1 to 1000"))
                })
                .transpose()?
                .unwrap_or(1000)
        } else {
            1000
        };
        if !(1..=1000).contains(&limit) {
            return Err(Failure::invalid("limit must be an integer from 1 to 1000"));
        }
        let wait_ms = if command == "events" {
            flags
                .remove("--wait-ms")
                .map(|n| {
                    n.parse()
                        .map_err(|_| Failure::invalid("wait-ms must be an integer from 0 to 30000"))
                })
                .transpose()?
                .unwrap_or(0)
        } else {
            0
        };
        if wait_ms > 30_000 {
            return Err(Failure::invalid("wait-ms must not exceed 30000"));
        }
        let cursor_file = if matches!(command.as_str(), "events" | "prompt") {
            flags.remove("--cursor-file").map(Into::into)
        } else {
            None
        };
        if !flags.is_empty() {
            return Err(Failure::invalid(
                "option does not apply to this mode or command",
            ));
        }
        Ok(Self {
            connection,
            command,
            session,
            task,
            label,
            text,
            approval,
            decision,
            after,
            limit,
            wait_ms,
            cursor_file,
        })
    }
}
fn take(flags: &mut BTreeMap<String, String>, key: &str) -> Result<String> {
    flags
        .remove(key)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Failure::invalid(format!("{key} requires a nonempty value")))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn cancel_args() -> Vec<String> {
        [
            "--mode",
            "ipc",
            "--address",
            "127.0.0.1:4000",
            "--token-env",
            "TOKEN",
            "cancel",
            "--session",
            "session-a",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn cancel_requires_original_task_id() {
        let error = Options::parse(cancel_args()).err().unwrap();
        assert!(error.message.contains("--task"));
        let mut args = cancel_args();
        args.extend(["--task".into(), "task-a".into()]);
        let options = Options::parse(args).ok().unwrap();
        assert_eq!(options.task.as_deref(), Some("task-a"));
        assert_eq!(options.session.as_deref(), Some("session-a"));
    }
}
