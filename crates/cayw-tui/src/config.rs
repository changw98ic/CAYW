//! Explicit host connection settings. No local model credentials are read.
use std::{fmt, net::SocketAddr};

pub const USAGE: &str = "cayw-tui [--addr IP:PORT]\n\nConnect to a running host using CAYW_IPC_ADDR and CAYW_IPC_TOKEN.\n--addr overrides CAYW_IPC_ADDR. The token is accepted only from the environment.\n\nCtrl+N new session | Tab focus | Enter send | Alt+Enter newline\nCtrl+X cancel | Alt+O once | Alt+T task | Alt+D deny | Ctrl+F fork\nPgUp/PgDn history | Ctrl+R reconnect | Ctrl+Q quit";

pub struct Config {
    pub address: SocketAddr,
    token: String,
}

impl Config {
    pub fn from_environment() -> Result<Self, String> {
        parse(std::env::args().skip(1), |name| std::env::var(name).ok())
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("address", &self.address)
            .field("token", &"[redacted]")
            .finish()
    }
}

pub fn parse(
    args: impl IntoIterator<Item = String>,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<Config, String> {
    let mut address = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" if address.is_none() => {
                address = Some(args.next().ok_or("--addr requires IP:PORT")?);
            }
            "--help" | "-h" => return Err(USAGE.into()),
            _ => return Err("Unknown or duplicate argument. Use --help for usage.".into()),
        }
    }
    let address = address
        .or_else(|| environment("CAYW_IPC_ADDR"))
        .ok_or("Set CAYW_IPC_ADDR to the running host's IP:PORT.")?
        .parse::<SocketAddr>()
        .map_err(|_| "IPC address must be a numeric IP:PORT.")?;
    if address.port() == 0 || !address.ip().is_loopback() {
        return Err("IPC address requires a loopback IP and a nonzero port.".into());
    }
    let token = environment("CAYW_IPC_TOKEN")
        .filter(|value| !value.trim().is_empty() && !value.chars().any(char::is_control))
        .ok_or("Set CAYW_IPC_TOKEN to the host-configured token.")?;
    Ok(Config { address, token })
}
