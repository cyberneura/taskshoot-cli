use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use wait_timeout::ChildExt;

pub const DEFAULT_API_ORIGIN: &str = "https://taskshoot-api.cyberneura.com";
const GETTER_ENV: &str = "TASKSHOOT_CLI_ENV_GETTER_COMMAND";
// op read は初回の生体認証で時間がかかることがある
const GETTER_TIMEOUT: Duration = Duration::from_secs(120);

// Debug は derive しない (api_key を含むため、将来の debug 出力で漏れるのを防ぐ)
#[derive(Clone)]
pub struct Config {
    pub api_origin: String,
    /// org 不要なコマンド (me / orgs) もあるため、必須チェックは使用時に行う。
    pub organization: Option<String>,
    pub api_key: String,
}

/// 解決順:
/// 1. env 直接 (TASKSHOOT_API_KEY / TASKSHOOT_CLI_ORGANIZATION) — CI やエージェントが渡すケース
/// 2. env TASKSHOOT_CLI_ENV_GETTER_COMMAND をシェルなしで実行し env-file 形式の stdout を取り込む
/// 3. .loadenv.sh (カレント→上位→実行ファイル位置) から getter コマンド行だけ抽出して 2 へ
pub fn resolve(org_override: Option<String>) -> Result<Config> {
    let env_key = non_empty_env("TASKSHOOT_API_KEY");
    let env_org = non_empty_env("TASKSHOOT_CLI_ORGANIZATION");
    let env_origin = non_empty_env("TASKSHOOT_API_ORIGIN");

    let mut fetched: HashMap<String, String> = HashMap::new();
    if env_key.is_none() || (env_org.is_none() && org_override.is_none()) {
        if let Some(cmd) = find_getter_command()? {
            fetched = run_getter_command(&cmd)?;
        }
    }

    let api_key = env_key
        .or_else(|| fetched.get("TASKSHOOT_API_KEY").cloned())
        .context(
            "TASKSHOOT_API_KEY is not set. Set it directly, or provide \
             TASKSHOOT_CLI_ENV_GETTER_COMMAND (or a .loadenv.sh exporting it).",
        )?;
    let organization = org_override
        .or(env_org)
        .or_else(|| fetched.get("TASKSHOOT_CLI_ORGANIZATION").cloned());
    let api_origin = env_origin
        .or_else(|| fetched.get("TASKSHOOT_API_ORIGIN").cloned())
        .unwrap_or_else(|| DEFAULT_API_ORIGIN.to_string());

    Ok(Config {
        api_origin: api_origin.trim_end_matches('/').to_string(),
        organization,
        api_key,
    })
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn find_getter_command() -> Result<Option<String>> {
    if let Some(cmd) = non_empty_env(GETTER_ENV) {
        return Ok(Some(cmd));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        dirs.extend(cwd.ancestors().map(|p| p.to_path_buf()));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    for dir in dirs {
        let candidate = dir.join(".loadenv.sh");
        if candidate.is_file() {
            let content = std::fs::read_to_string(&candidate)
                .with_context(|| format!("cannot read {}", candidate.display()))?;
            if let Some(cmd) = extract_getter_line(&content) {
                // 発見ベースの自動実行なので、どのファイルを信頼したかを可視化する
                eprintln!(
                    "taskshoot: using {GETTER_ENV} from {}",
                    candidate.display()
                );
                return Ok(Some(cmd));
            }
        }
    }
    Ok(None)
}

/// .loadenv.sh から `export TASKSHOOT_CLI_ENV_GETTER_COMMAND=...` の行だけ抜き出す。
/// ファイル全体を shell 実行はしない (任意コード実行を避ける)。
/// クォートされた値は閉じクォートまで、無クォートは `#` 以降 (行内コメント) を無視する。
pub fn extract_getter_line(content: &str) -> Option<String> {
    let prefix = format!("{GETTER_ENV}=");
    for line in content.lines() {
        let line = line.trim();
        let rest = line.strip_prefix("export ").unwrap_or(line);
        if let Some(value) = rest.strip_prefix(&prefix) {
            let value = value.trim();
            let extracted = if let Some(inner) = value.strip_prefix('\'') {
                inner.split('\'').next()
            } else if let Some(inner) = value.strip_prefix('"') {
                inner.split('"').next()
            } else {
                value.split('#').next().map(str::trim)
            };
            if let Some(extracted) = extracted {
                if !extracted.is_empty() {
                    return Some(extracted.to_string());
                }
            }
        }
    }
    None
}

fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'\'' && b[b.len() - 1] == b'\'')
            || (b[0] == b'"' && b[b.len() - 1] == b'"'))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        buf
    })
}

/// getter コマンドをシェルを介さず実行する (shlex 分割 + 直接 spawn)。
/// シェルメタ文字が解釈されないためコマンドインジェクションの余地が無い。
/// stdout/stderr は別スレッドで回収する (パイプ詰まりでの wait デッドロック回避)。
/// 既に env にある API キーは getter 子プロセスへ渡さない (発見した .loadenv.sh の
/// コマンド経由でキーが読まれるのを防ぐ)。
fn run_getter_command(cmd: &str) -> Result<HashMap<String, String>> {
    let argv =
        shlex::split(cmd).with_context(|| format!("invalid getter command quoting: {cmd}"))?;
    if argv.is_empty() {
        bail!("getter command is empty");
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .env_remove("TASKSHOOT_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run getter command: {}", argv[0]))?;
    let stdout_reader = child.stdout.take().map(spawn_reader);
    let stderr_reader = child.stderr.take().map(spawn_reader);
    let status = match child.wait_timeout(GETTER_TIMEOUT)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "getter command timed out after {}s: {cmd}",
                GETTER_TIMEOUT.as_secs()
            );
        }
    };
    let stdout = stdout_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if !status.success() {
        let stderr = stderr_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let brief: String = stderr.chars().take(300).collect();
        bail!(
            "getter command failed (exit {:?}): {}",
            status.code(),
            brief.trim()
        );
    }
    let vars = parse_env_file(&stdout);
    if vars.is_empty() {
        bail!("getter command produced no KEY=VALUE output");
    }
    Ok(vars)
}

/// env-file 形式 (KEY=VALUE、`#` コメント行、`export ` プレフィックス可) をパースする。
pub fn parse_env_file(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                continue;
            }
            map.insert(key.to_string(), strip_quotes(value.trim()).to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_file_basic() {
        let vars = parse_env_file(
            "# comment\n\
             TASKSHOOT_CLI_ORGANIZATION=cyberneura\n\
             \n\
             TASKSHOOT_API_KEY=tssk-abc123\n",
        );
        assert_eq!(vars["TASKSHOOT_CLI_ORGANIZATION"], "cyberneura");
        assert_eq!(vars["TASKSHOOT_API_KEY"], "tssk-abc123");
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn parse_env_file_export_and_quotes() {
        let vars = parse_env_file(
            "export A=\"double quoted\"\n\
             B='single quoted'\n\
             C=plain # not stripped as comment\n",
        );
        assert_eq!(vars["A"], "double quoted");
        assert_eq!(vars["B"], "single quoted");
        assert_eq!(vars["C"], "plain # not stripped as comment");
    }

    #[test]
    fn parse_env_file_skips_invalid_keys() {
        let vars = parse_env_file("BAD KEY=1\n=novalue\nOK_1=yes\n");
        assert_eq!(vars.len(), 1);
        assert_eq!(vars["OK_1"], "yes");
    }

    #[test]
    fn extract_getter_line_variants() {
        let sh = "#!/bin/sh\n\
                  export TASKSHOOT_CLI_ENV_GETTER_COMMAND='op read \"op://development/taskshoot/cli\"'\n";
        assert_eq!(
            extract_getter_line(sh).as_deref(),
            Some("op read \"op://development/taskshoot/cli\"")
        );
        assert_eq!(extract_getter_line("export OTHER=1\n"), None);
        // export なし・ダブルクォートも許容
        let sh2 = "TASKSHOOT_CLI_ENV_GETTER_COMMAND=\"echo X=1\"\n";
        assert_eq!(extract_getter_line(sh2).as_deref(), Some("echo X=1"));
    }

    #[test]
    fn extract_getter_line_ignores_trailing_comment() {
        let quoted = "export TASKSHOOT_CLI_ENV_GETTER_COMMAND='op read \"op://x/y\"' # 1Password\n";
        assert_eq!(
            extract_getter_line(quoted).as_deref(),
            Some("op read \"op://x/y\"")
        );
        let unquoted = "TASKSHOOT_CLI_ENV_GETTER_COMMAND=my-getter --json # comment\n";
        assert_eq!(
            extract_getter_line(unquoted).as_deref(),
            Some("my-getter --json")
        );
    }
}
