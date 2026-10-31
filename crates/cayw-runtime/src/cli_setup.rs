use cayw_runtime::{HeadlessConfig, ProviderConfig, local_cli::save_config};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs::File,
    io::{self, BufRead, IsTerminal, Read, Write},
    path::Path,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

pub fn offer(path: &Path) -> Result<bool> {
    let mut input = io::stdin().lock();
    let mut output = io::stderr().lock();
    writeln!(output, "尚未配置模型。配置文件：{}", path.display())?;
    if !confirm(&mut input, &mut output, "现在配置模型吗？[Y/n] ")? {
        return Ok(false);
    }
    configure_with(path, &mut input, &mut output)
}

pub fn configure(path: &Path) -> Result<bool> {
    configure_with(path, &mut io::stdin().lock(), &mut io::stderr().lock())
}

fn configure_with(path: &Path, input: &mut impl BufRead, output: &mut impl Write) -> Result<bool> {
    let original = match File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 1024 * 1024 {
                return Err("配置文件超过 1 MiB，未修改文件".into());
            }
            Some(bytes)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut configuration: Value = original
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or_else(|| json!({"permissions": []}));
    let existing: HeadlessConfig = serde_json::from_value(configuration.clone())?;
    if let Some(model) = existing.model() {
        model.validate()?;
        writeln!(
            output,
            "模型已配置：{}。如需更改，请编辑此文件。",
            path.display()
        )?;
        return Ok(true);
    }
    let Some(model) = read_model(input, output)? else {
        writeln!(output, "已取消配置，原配置未改动。")?;
        return Ok(false);
    };
    configuration
        .as_object_mut()
        .ok_or("配置文件必须是 JSON 对象，未修改文件")?
        .insert("model".into(), serde_json::to_value(&model)?);
    let checked: HeadlessConfig = serde_json::from_value(configuration.clone())?;
    checked.into_runtime_config()?;
    writeln!(output, "将保存模型配置：{}", path.display())?;
    if !confirm(input, output, "保存配置吗？[Y/n] ")? {
        writeln!(output, "已取消配置，原配置未改动。")?;
        return Ok(false);
    }
    let mut bytes = serde_json::to_vec_pretty(&configuration)?;
    bytes.push(b'\n');
    save_config(path, original.as_deref(), &bytes)?;
    let value = serde_json::to_value(&model)?;
    let name = value["api_key_env"]
        .as_str()
        .expect("validated environment name");
    writeln!(output, "模型配置已保存：{}", path.display())?;
    writeln!(output, "配置只保存密钥环境变量的名称，不保存密钥。")?;
    writeln!(output, "请在启动服务的终端设置密钥：")?;
    writeln!(output, "  export {name}='你的密钥'")?;
    writeln!(
        output,
        "然后运行 `cayw serve --config {}`。",
        shell_path(path)
    )?;
    Ok(true)
}

fn read_model(input: &mut impl BufRead, output: &mut impl Write) -> Result<Option<ProviderConfig>> {
    writeln!(
        output,
        "选择模型服务：1) OpenAI  2) Anthropic  3) Gemini  4) Magpie"
    )?;
    writeln!(
        output,
        "这里只填写配置。不会发送模型请求，也不会要求输入密钥值。"
    )?;
    let service = loop {
        let Some(value) = answer(input, output, "模型服务 [1-4 或名称]：", None)? else {
            return Ok(None);
        };
        match value.to_ascii_lowercase().as_str() {
            "1" | "openai" => break "openai",
            "2" | "anthropic" => break "anthropic",
            "3" | "gemini" => break "gemini",
            "4" | "magpie" => break "magpie",
            _ => writeln!(output, "请选择 1-4，或输入对应的服务名称。")?,
        }
    };
    let (default_api, default_base, default_key, apis) = match service {
        "openai" => (
            "chat_completions",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            &["chat_completions", "responses"][..],
        ),
        "anthropic" => (
            "messages",
            "https://api.anthropic.com/v1",
            "ANTHROPIC_API_KEY",
            &["messages"][..],
        ),
        "gemini" => (
            "stream_generate_content",
            "https://generativelanguage.googleapis.com/v1beta",
            "GEMINI_API_KEY",
            &["stream_generate_content", "generate_content"][..],
        ),
        _ => (
            "chat_completions",
            "http://127.0.0.1:3425/v1",
            "MAGPIE_API_KEY",
            &[
                "chat_completions",
                "responses",
                "messages",
                "stream_generate_content",
                "generate_content",
            ][..],
        ),
    };
    writeln!(output, "可选接口格式：{}", apis.join(" / "))?;
    let api = loop {
        let Some(value) = answer(
            input,
            output,
            &format!("接口格式 [{default_api}]："),
            Some(default_api),
        )?
        else {
            return Ok(None);
        };
        if apis.contains(&value.as_str()) {
            break value;
        }
        writeln!(output, "该服务不支持这个接口格式，请从上面的列表选择。")?;
    };
    let version = match api.as_str() {
        "messages" => "2023-06-01",
        "stream_generate_content" | "generate_content" => "v1beta",
        _ => "v1",
    };
    let base = if service == "magpie" && version == "v1beta" {
        "http://127.0.0.1:3425/v1beta"
    } else {
        default_base
    };
    let Some(base_url) = answer(input, output, &format!("服务地址 [{base}]："), Some(base))?
    else {
        return Ok(None);
    };
    let Some(id) = answer(input, output, "模型名称（按服务提供的名称填写）：", None)?
    else {
        return Ok(None);
    };
    let Some(api_key_env) = answer(
        input,
        output,
        &format!("密钥环境变量名 [{default_key}]："),
        Some(default_key),
    )?
    else {
        return Ok(None);
    };
    let max_output_tokens = loop {
        let Some(value) = answer(input, output, "单次输出 token 上限 [4096]：", Some("4096"))?
        else {
            return Ok(None);
        };
        if let Ok(limit) = value.parse::<u64>()
            && limit > 0
        {
            break limit;
        }
        writeln!(output, "请输入大于 0 的整数。")?;
    };
    let model: ProviderConfig = serde_json::from_value(json!({
        "service":service, "api":api, "base_url":base_url, "id":id,
        "api_version":version, "api_key_env":api_key_env, "max_output_tokens":max_output_tokens
    }))?;
    model.validate()?;
    Ok(Some(model))
}

fn answer(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    default: Option<&str>,
) -> io::Result<Option<String>> {
    loop {
        write!(output, "{prompt}")?;
        output.flush()?;
        let mut value = String::new();
        if input.take(4097).read_line(&mut value)? == 0 {
            return Ok(None);
        }
        if value.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "输入超过 4096 字节，未保存配置",
            ));
        }
        let value = value.trim();
        if !value.is_empty() {
            return Ok(Some(value.to_owned()));
        }
        if let Some(default) = default {
            return Ok(Some(default.to_owned()));
        }
        writeln!(output, "此项必填。")?;
    }
}

fn confirm(input: &mut impl BufRead, output: &mut impl Write, prompt: &str) -> io::Result<bool> {
    loop {
        let Some(value) = answer(input, output, prompt, Some("y"))? else {
            return Ok(false);
        };
        match value.to_ascii_lowercase().as_str() {
            "y" | "yes" | "是" => return Ok(true),
            "n" | "no" | "否" => return Ok(false),
            _ => writeln!(output, "请输入 y 或 n。")?,
        }
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}
