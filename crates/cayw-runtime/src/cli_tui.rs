use cayw_runtime::local_cli::LocalPaths;
use std::{error::Error, os::unix::process::CommandExt, path::PathBuf, process::Command};

const USAGE: &str = "cayw tui [--workspace DIR]\n\n打开终端交互界面，自动连接当前工作区的 cayw serve。\n先在项目目录启动 cayw serve，再在另一终端的同一目录运行 cayw tui。\n使用 ~/.cayw 中的本机连接设置；CAYW_HOME 可指定其他数据根。\nCtrl+N 新建会话 | Enter 发送 | Ctrl+X 取消 | Ctrl+Q 退出";

pub fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut arguments = arguments.into_iter();
    let mut workspace = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--workspace" if workspace.is_none() => {
                workspace = Some(PathBuf::from(
                    arguments.next().ok_or("--workspace requires DIR")?,
                ));
            }
            _ => return Err("tui accepts only --workspace; use `cayw tui --help`".into()),
        }
    }
    let workspace = workspace.map_or_else(std::env::current_dir, Ok)?;
    let paths = LocalPaths::from_environment(workspace)?;
    let (address, token) = paths.connect().map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("无法连接当前工作区的服务：{error}。请先在该工作区运行 `cayw serve`。"),
        )
    })?;
    let binary = std::env::current_exe()?.with_file_name("cayw-tui");
    let error = Command::new(&binary)
        .env("CAYW_IPC_ADDR", address.to_string())
        .env("CAYW_IPC_TOKEN", token)
        .exec();
    Err(std::io::Error::new(
        error.kind(),
        format!(
            "无法启动终端界面 {}：{error}。请使用发行包的 install.sh 安装完整程序。",
            binary.display()
        ),
    )
    .into())
}
