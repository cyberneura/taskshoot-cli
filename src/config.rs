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
// `op read` can be slow on the first biometric authentication
const GETTER_TIMEOUT: Duration = Duration::from_secs(120);

// Do not derive Debug (it holds api_key; prevents leaking it in future debug output)
#[derive(Clone)]
pub struct Config {
    pub api_origin: String,
    /// Some commands (me / orgs) do not need an org, so the required check is done at use time.
    pub organization: Option<String>,
    pub api_key: String,
}

/// Resolution order:
/// 1. Direct env (TASKSHOOT_API_KEY / TASKSHOOT_CLI_ORGANIZATION) — the CI/agent case
/// 2. Run env TASKSHOOT_CLI_ENV_GETTER_COMMAND without a shell and take its
///    env-file-formatted stdout
/// 3. Extract just the getter command line from a .loadenv.sh (current dir →
///    ancestors → executable location) and fall through to 2
///
/// Commands with `need_org=false` (me / orgs) do not launch the getter
/// (`op read` = auth may run) just to obtain an org when the key is already available.
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

/// Search targets for .loadenv.sh: ancestors of the CWD → ancestors of the
/// executable (deduplicated). The executable side also walks up to its ancestors
/// so that running from target/release or any location can still find the
/// configuration inside the repository.
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

/// Find the first .loadenv.sh containing a getter line (does not execute it).
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
    // Like direnv's allow, a discovered getter command is only executed from an
    // explicitly trusted file (prevents arbitrary command execution under a
    // malicious repository).
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

/// Line format of the trust file: `<sha256> <absolute path>`.
/// If the file contents change, the hash mismatch requires re-trusting.
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

/// If the env setup lets us **assert** that trust is unnecessary, return the
/// variable name that justifies it. Trust is a mechanism specific to the
/// ".loadenv.sh search path" (resolution order 3), so it is only safe to say
/// "unnecessary" for setups where the search is guaranteed not to run.
fn external_auth_env() -> Option<&'static str> {
    external_auth_env_with(non_empty_env)
}

/// Made injectable for env lookups so it can be tested without mutating the
/// global env (tests run in parallel, so set_var would race).
fn external_auth_env_with(lookup: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    // If GETTER_ENV is set, find_getter_command returns immediately, so the search never runs.
    if lookup(GETTER_ENV).is_some() {
        return Some(GETTER_ENV);
    }
    // The key alone is not enough: when `need_org && org_unresolved`, resolve()
    // enters the getter search to obtain the org (org-scoped commands need the
    // .loadenv.sh trust). Only when the org is also resolved from env can we
    // assert the search will not run.
    // (It also won't run with --org, but trust runs before config resolution, so
    //  we only look at env here. We err toward narrowing the check rather than
    //  wrongly claiming "unnecessary".)
    if lookup("TASKSHOOT_API_KEY").is_some() && lookup("TASKSHOOT_CLI_ORGANIZATION").is_some() {
        return Some("TASKSHOOT_API_KEY");
    }
    None
}

/// Guidance for when a bare `taskshoot trust` finds no candidate at all.
/// If auth is already configured via env, "trust is unnecessary" is not an
/// error, so we explain why and exit successfully instead of erroring.
fn report_no_loadenv_candidate() -> Result<()> {
    // Never print env "values" (variable names only). Avoids leaking credentials.
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

/// `taskshoot trust [path]`: authorize a .loadenv.sh to run its getter
/// (equivalent to direnv allow). When path is omitted, targets the first
/// candidate discovered from the current directory.
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
    // Replace any older hash line for the same path
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

/// Extract only the `export TASKSHOOT_CLI_ENV_GETTER_COMMAND=...` line from a
/// .loadenv.sh. Does not shell-execute the whole file (avoids arbitrary code
/// execution). A quoted value is read up to its closing quote; an unquoted value
/// ignores everything from `#` onward (an inline comment).
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

/// Run the getter command without going through a shell (shlex split + direct
/// spawn). Shell metacharacters are not interpreted, so there is no room for
/// command injection. stdout/stderr are collected on separate threads (avoids a
/// wait deadlock from a clogged pipe). An API key already in env is not passed to
/// the getter child process (prevents the key from being read via a discovered
/// .loadenv.sh's command).
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

/// Parse env-file format (KEY=VALUE, `#` comment lines, optional `export ` prefix).
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
        // The `export` prefix is optional and double quotes are also allowed
        let sh2 = "TASKSHOOT_CLI_ENV_GETTER_COMMAND=\"echo X=1\"\n";
        assert_eq!(extract_getter_line(sh2).as_deref(), Some("echo X=1"));
    }

    #[test]
    fn external_auth_env_detects_trust_free_setups() {
        // Nothing in env = the .loadenv.sh path is needed → trust is meaningful
        assert_eq!(external_auth_env_with(|_| None), None);
        // The getter is exported externally (a wrapper / shell profile) → the
        // search is always skipped, so trust is unnecessary
        assert_eq!(
            external_auth_env_with(|name| (name == GETTER_ENV).then(|| "cat env".to_string())),
            Some(GETTER_ENV)
        );
        // With both the key and org present, the search does not run → trust unnecessary
        assert_eq!(
            external_auth_env_with(|name| matches!(
                name,
                "TASKSHOOT_API_KEY" | "TASKSHOOT_CLI_ORGANIZATION"
            )
            .then(|| "set".to_string())),
            Some("TASKSHOOT_API_KEY")
        );
        // Key-only with org unresolved cannot be asserted "unnecessary": org-scoped
        // commands search .loadenv.sh to obtain the org (resolve's need_org branch).
        assert_eq!(
            external_auth_env_with(
                |name| (name == "TASKSHOOT_API_KEY").then(|| "tssk-dummy".to_string())
            ),
            None
        );
        // With the getter set, the search does not run even if org is unresolved (GETTER_ENV wins)
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
        // Path mismatch
        assert!(!is_trusted_entry(
            &trust,
            Path::new("/home/user/other/.loadenv.sh"),
            &sha
        ));
        // Contents changed (hash mismatch) → not trusted
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
