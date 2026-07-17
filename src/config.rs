use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
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
///
/// `need_org=false` のコマンド (me / orgs) は、キーが揃っていれば org 目当てで
/// getter (op read = 認証が走りうる) を起動しない。
pub fn resolve(org_override: Option<String>, need_org: bool) -> Result<Config> {
    let env_key = non_empty_env("TASKSHOOT_API_KEY");
    let env_org = non_empty_env("TASKSHOOT_CLI_ORGANIZATION");
    let env_origin = non_empty_env("TASKSHOOT_API_ORIGIN");

    let mut fetched: HashMap<String, String> = HashMap::new();
    let org_unresolved = env_org.is_none() && org_override.is_none();
    if env_key.is_none() || (need_org && org_unresolved) {
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

/// .loadenv.sh の探索対象: CWD の祖先 → 実行ファイルの祖先 (重複除去)。
/// 実行ファイル側も祖先まで見るのは、target/release や任意の場所から
/// 実行してもリポジトリ内の設定を見つけられるようにするため。
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        dirs.extend(cwd.ancestors().map(|p| p.to_path_buf()));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.extend(parent.ancestors().map(|p| p.to_path_buf()));
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !seen.contains(&dir) {
            seen.push(dir);
        }
    }
    seen
}

/// getter 行を含む最初の .loadenv.sh を探す (実行はしない)。
fn discover_loadenv_candidate() -> Result<Option<PathBuf>> {
    for dir in candidate_dirs() {
        let candidate = dir.join(".loadenv.sh");
        if candidate.is_file() {
            let content = std::fs::read_to_string(&candidate)
                .with_context(|| format!("cannot read {}", candidate.display()))?;
            if extract_getter_line(&content).is_some() {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

fn find_getter_command() -> Result<Option<String>> {
    if let Some(cmd) = non_empty_env(GETTER_ENV) {
        return Ok(Some(cmd));
    }
    let Some(candidate) = discover_loadenv_candidate()? else {
        return Ok(None);
    };
    let candidate = std::fs::canonicalize(&candidate)
        .with_context(|| format!("cannot resolve {}", candidate.display()))?;
    let content = std::fs::read_to_string(&candidate)
        .with_context(|| format!("cannot read {}", candidate.display()))?;
    let Some(cmd) = extract_getter_line(&content) else {
        return Ok(None);
    };
    // 発見した getter コマンドは direnv の allow と同様、明示的に信頼された
    // ファイルのみ実行する (悪意あるリポジトリ配下での任意コマンド実行を防ぐ)。
    if !is_trusted(&candidate, &content) {
        eprintln!(
            "taskshoot: found {} but it is not trusted; run `taskshoot trust {}` \
             to allow executing its getter command",
            candidate.display(),
            candidate.display()
        );
        return Ok(None);
    }
    eprintln!("taskshoot: using {GETTER_ENV} from {}", candidate.display());
    Ok(Some(cmd))
}

fn trust_file_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("taskshoot")
            .join("trusted-loadenv")
    })
}

fn sha256_hex(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// trust ファイルの行形式: `<sha256> <絶対パス>`。
/// ファイル内容が変わるとハッシュ不一致で再信頼が必要になる。
fn is_trusted_entry(trust_content: &str, path: &Path, sha: &str) -> bool {
    trust_content.lines().any(|line| {
        line.trim()
            .split_once(' ')
            .is_some_and(|(hash, entry_path)| hash == sha && Path::new(entry_path.trim()) == path)
    })
}

fn is_trusted(path: &Path, content: &str) -> bool {
    let Some(trust_path) = trust_file_path() else {
        return false;
    };
    let Ok(trust_content) = std::fs::read_to_string(&trust_path) else {
        return false;
    };
    is_trusted_entry(&trust_content, path, &sha256_hex(content))
}

/// trust が不要と**断言できる** env 構成なら、その根拠となる変数名を返す。
/// trust は「.loadenv.sh 探索経路」(解決順 3) 専用の仕組みなので、探索が
/// 走らないと確定する構成でだけ「不要」と言ってよい。
fn external_auth_env() -> Option<&'static str> {
    external_auth_env_with(non_empty_env)
}

/// env 参照を注入できる形にして、グローバル env を書き換えずにテストする
/// (テストは並列実行されるため set_var はレースになる)。
fn external_auth_env_with(lookup: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    // GETTER_ENV があれば find_getter_command が即 return するため、探索は必ず走らない。
    if lookup(GETTER_ENV).is_some() {
        return Some(GETTER_ENV);
    }
    // キーだけでは不十分: resolve() は `need_org && org_unresolved` の時、org を得る目的で
    // getter 探索に入る (org スコープのコマンドで .loadenv.sh の trust が要る)。
    // org も env で解決済みの時だけ探索が走らないと断言できる。
    // (--org 指定でも走らないが、trust は config 解決前に動くのでここでは env のみ見る。
    //  誤って「不要」と言うより、判定を絞る側に倒す。)
    if lookup("TASKSHOOT_API_KEY").is_some() && lookup("TASKSHOOT_CLI_ORGANIZATION").is_some() {
        return Some("TASKSHOOT_API_KEY");
    }
    None
}

/// bare `taskshoot trust` で候補が 1 つも見つからなかった場合の案内。
/// env で認証が構成済みなら「trust は不要」であって異常ではないため、
/// エラーにせず理由を説明して正常終了する。
fn report_no_loadenv_candidate() -> Result<()> {
    // env の「値」は決して出さない (変数名のみ)。認証情報の漏洩を避ける。
    if let Some(source) = external_auth_env() {
        println!(
            "nothing to trust: no .loadenv.sh was found, and none is needed \
             because {source} is already set in the environment."
        );
        println!(
            "`taskshoot trust` only authorizes a discovered .loadenv.sh, which \
             is the lowest-precedence way to supply credentials. Your setup \
             uses a higher-precedence one, so the .loadenv.sh search never runs."
        );
        return Ok(());
    }

    let searched = candidate_dirs()
        .iter()
        .map(|dir| format!("  {}", dir.join(".loadenv.sh").display()))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "no .loadenv.sh exporting {GETTER_ENV} was found.\n\n\
         `taskshoot trust` authorizes an existing .loadenv.sh to run its getter \
         command (direnv-style allow); it does not create one. You only need it \
         when credentials come from a discovered .loadenv.sh -- setting \
         {GETTER_ENV} (or both TASKSHOOT_API_KEY and TASKSHOOT_CLI_ORGANIZATION) \
         in the environment instead makes trust unnecessary.\n\n\
         Searched (ancestors of the current directory, then of the executable):\n\
         {searched}\n\n\
         Pass an explicit path to trust a file outside these locations: \
         `taskshoot trust <path>`"
    )
}

/// `taskshoot trust [path]`: .loadenv.sh の getter 実行を許可する (direnv allow 相当)。
/// path 省略時はカレントから探索した最初の候補を対象にする。
pub fn trust_loadenv(path: Option<PathBuf>) -> Result<()> {
    let path = match path {
        Some(path) => path,
        None => match discover_loadenv_candidate()? {
            Some(candidate) => candidate,
            None => return report_no_loadenv_candidate(),
        },
    };
    let path = std::fs::canonicalize(&path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let cmd = extract_getter_line(&content)
        .with_context(|| format!("{} does not export {GETTER_ENV}", path.display()))?;
    let sha = sha256_hex(&content);
    let trust_path = trust_file_path().context("HOME is not set")?;
    if let Some(parent) = trust_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&trust_path).unwrap_or_default();
    if is_trusted_entry(&existing, &path, &sha) {
        println!("already trusted: {}", path.display());
        return Ok(());
    }
    // 同じパスの古いハッシュ行は差し替える
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|line| {
            line.trim()
                .split_once(' ')
                .map(|(_, entry_path)| Path::new(entry_path.trim()) != path)
                .unwrap_or(true)
        })
        .map(str::to_string)
        .collect();
    lines.push(format!("{sha} {}", path.display()));
    std::fs::write(&trust_path, lines.join("\n") + "\n")?;
    println!("trusted: {}", path.display());
    println!("getter command: {cmd}");
    Ok(())
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
        && ((b[0] == b'\'' && b[b.len() - 1] == b'\'') || (b[0] == b'"' && b[b.len() - 1] == b'"'))
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
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
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
    fn external_auth_env_detects_trust_free_setups() {
        // env に何も無い = .loadenv.sh 経路が必要 → trust は意味を持つ
        assert_eq!(external_auth_env_with(|_| None), None);
        // getter を外部 (ラッパー / シェルプロファイル) が export 済み → 探索は必ず
        // スキップされるので trust 不要
        assert_eq!(
            external_auth_env_with(|name| (name == GETTER_ENV).then(|| "cat env".to_string())),
            Some(GETTER_ENV)
        );
        // キー + org が揃っていれば探索は走らない → trust 不要
        assert_eq!(
            external_auth_env_with(|name| matches!(
                name,
                "TASKSHOOT_API_KEY" | "TASKSHOOT_CLI_ORGANIZATION"
            )
            .then(|| "set".to_string())),
            Some("TASKSHOOT_API_KEY")
        );
        // キーのみ・org 未解決は「不要」と断言できない: org スコープのコマンドは
        // org を得るために .loadenv.sh を探索する (resolve の need_org 分岐)。
        assert_eq!(
            external_auth_env_with(
                |name| (name == "TASKSHOOT_API_KEY").then(|| "tssk-dummy".to_string())
            ),
            None
        );
        // getter があれば org 未解決でも探索は走らない (GETTER_ENV が優先)
        assert_eq!(
            external_auth_env_with(|name| (name == GETTER_ENV).then(|| "cat env".to_string())),
            Some(GETTER_ENV)
        );
    }

    #[test]
    fn trust_entry_matches_hash_and_path() {
        let sha = sha256_hex("export X=1\n");
        let trust = format!("{sha} /home/user/proj/.loadenv.sh\n");
        assert!(is_trusted_entry(
            &trust,
            Path::new("/home/user/proj/.loadenv.sh"),
            &sha
        ));
        // パス不一致
        assert!(!is_trusted_entry(
            &trust,
            Path::new("/home/user/other/.loadenv.sh"),
            &sha
        ));
        // 内容が変わった (ハッシュ不一致) → 信頼しない
        assert!(!is_trusted_entry(
            &trust,
            Path::new("/home/user/proj/.loadenv.sh"),
            &sha256_hex("export X=2\n")
        ));
        assert!(!is_trusted_entry("", Path::new("/a"), &sha));
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
