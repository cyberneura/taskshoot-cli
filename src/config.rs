use std::env;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_yaml::{Mapping, Value};
use wait_timeout::ChildExt;

pub const DEFAULT_API_ORIGIN: &str = "https://taskshoot-api.cyberneura.com";
const OVERRIDE_COMMAND_KEY: &str = "config_override_command";
// `op read` and similar helpers can be slow on the first biometric prompt
const OVERRIDE_TIMEOUT: Duration = Duration::from_secs(120);
// Removed in favour of the config file; only referenced to guide migration
const LEGACY_GETTER_ENV: &str = "TASKSHOOT_CLI_ENV_GETTER_COMMAND";
// Renamed so that every variable this CLI reads shares the TASKSHOOT_CLI_ prefix
const RENAMED_ENV: [(&str, &str); 2] = [
    ("TASKSHOOT_API_KEY", "TASKSHOOT_CLI_API_KEY"),
    ("TASKSHOOT_API_ORIGIN", "TASKSHOOT_CLI_API_ORIGIN"),
];

const CONFIG_TEMPLATE: &str = r#"# Taskshoot CLI config file
# https://github.com/cyberneura/taskshoot-cli
#
# Precedence: command line flags > environment variables > this file.
# Environment variables: TASKSHOOT_CLI_API_KEY, TASKSHOOT_CLI_ORGANIZATION,
# TASKSHOOT_CLI_API_ORIGIN.

# api_key: tssk-xxxxxxxxxxxx
# organization: your-org-code-name

# Override the API origin (use http://127.0.0.1:8008 for local development).
# api_origin: https://taskshoot-api.cyberneura.com

# Keep the API key out of this file by fetching it from a secret store.
#
# config_override_command runs a command (without a shell) whose stdout must be
# YAML, and merges that YAML over this file. Mappings are merged recursively;
# scalars and sequences are replaced wholesale. Any key can be overridden this
# way, and the fetched YAML wins over the values written above.
#
# config_override_command: op read "op://development/taskshoot/config-yaml"
"#;

// Do not derive Debug (it holds api_key; prevents leaking it in future debug output)
#[derive(Clone)]
pub struct Config {
    pub api_origin: String,
    /// Some commands (me / orgs) do not need an org, so the required check is done at use time.
    pub organization: Option<String>,
    pub api_key: String,
}

/// Resolution order:
/// 1. Command line flags (--org)
/// 2. Environment variables (TASKSHOOT_CLI_API_KEY / TASKSHOOT_CLI_ORGANIZATION /
///    TASKSHOOT_CLI_API_ORIGIN) — the CI / agent case
/// 3. ~/.config/taskshoot/config.yml, with the YAML produced by its
///    `config_override_command` merged over it
pub fn resolve(org_override: Option<String>, need_org: bool) -> Result<Config> {
    let env_key = non_empty_env("TASKSHOOT_CLI_API_KEY");
    let env_org = non_empty_env("TASKSHOOT_CLI_ORGANIZATION");
    let env_origin = non_empty_env("TASKSHOOT_CLI_API_ORIGIN");

    let doc = if config_is_needed(
        need_org,
        env_key.is_some(),
        env_origin.is_some(),
        org_override.is_some() || env_org.is_some(),
    ) {
        load_merged_config()?
    } else {
        Mapping::new()
    };

    let api_key = match or_from_config(env_key, &doc, "api_key")? {
        Some(api_key) => api_key,
        None => bail!(missing_api_key_message()?),
    };
    let organization = or_from_config(org_override.or(env_org), &doc, "organization")?;
    let api_origin = or_from_config(env_origin, &doc, "api_origin")?
        .unwrap_or_else(|| DEFAULT_API_ORIGIN.to_string());

    Ok(Config {
        api_origin: api_origin.trim_end_matches('/').to_string(),
        organization,
        api_key,
    })
}

/// Fall back to a config file value only when the higher-precedence source did
/// not supply one.
///
/// `Option::or` would evaluate its argument eagerly, so a malformed value in
/// the file would abort the command even though a flag or environment variable
/// already shadowed it — the opposite of the documented precedence.
fn or_from_config(resolved: Option<String>, doc: &Mapping, key: &str) -> Result<Option<String>> {
    match resolved {
        Some(value) => Ok(Some(value)),
        None => doc_str(doc, key),
    }
}

/// Warn about credentials left under the pre-0.2.0 variable names. Staying
/// silent would let a stale variable look like it is in effect while the key
/// actually comes from somewhere else. Called for every subcommand, including
/// the `config` ones, since those are where the mismatch gets investigated.
pub fn warn_about_renamed_env() {
    for (old, new) in RENAMED_ENV {
        if env::var_os(old).is_some() {
            eprintln!("taskshoot: {old} is no longer read; it was renamed to {new}");
        }
    }
}

/// Whether the config file can still change the outcome.
///
/// Reading it is cheap, but it may carry a config_override_command that shells
/// out to a secret store and prompts for authentication, so it is skipped when
/// flags and the environment already decide every value it could supply.
///
/// api_origin has to be part of the condition: a config file may point at a
/// local development origin, and skipping without it would silently send
/// requests to production instead.
fn config_is_needed(need_org: bool, has_key: bool, has_origin: bool, has_org: bool) -> bool {
    let org_resolved = !need_org || has_org;
    !(has_key && has_origin && org_resolved)
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// ~/.config/taskshoot
///
/// The same relative location is used on every platform. `dirs::config_dir()`
/// would be more idiomatic per-OS, but it resolves to
/// ~/Library/Application Support on macOS, which does not match the path this
/// project documents. `dirs::home_dir()` is still needed over `$HOME` because
/// Windows does not normally set that variable.
fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine the home directory")?;
    Ok(home.join(".config").join("taskshoot"))
}

/// Prefer config.yml; fall back to config.yaml. When neither exists, return the
/// config.yml path (the one `config init` would create).
///
/// Only one of the two is ever read. Reading both would silently merge a stale
/// file left behind by a rename.
fn config_path_in(dir: &Path) -> PathBuf {
    let yml = dir.join("config.yml");
    if yml.exists() {
        return yml;
    }
    let yaml = dir.join("config.yaml");
    if yaml.exists() {
        return yaml;
    }
    yml
}

/// Path of the config file that will actually be read.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_path_in(&config_dir()?))
}

/// Restrict a config file to the owner. The file can hold a plaintext API key,
/// but a file created with a default umask is world readable.
#[cfg(unix)]
fn tighten_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot stat {}", path.display())),
    };
    // Only touch regular files. Setting 600 on a directory would drop the
    // owner's search bit and make everything below it unreachable.
    if !meta.is_file() {
        return Ok(());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot restrict permissions of {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Read the config file. A missing file is not an error: credentials may come
/// from the environment instead.
fn load_local_config(path: &Path) -> Result<Mapping> {
    if !path.exists() {
        return Ok(Mapping::new());
    }
    tighten_permissions(path)?;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse_mapping(&text, &path.display().to_string())
}

/// Read the config file and merge the YAML produced by its
/// `config_override_command` over it.
fn load_merged_config() -> Result<Mapping> {
    let path = config_path()?;
    let mut doc = load_local_config(&path)?;
    let Some(cmd) = override_command(&doc)? else {
        return Ok(doc);
    };
    let stdout = run_override_command(&cmd)?;
    let overrides = parse_mapping(&stdout, &format!("{OVERRIDE_COMMAND_KEY}: {cmd}"))?;
    merge_mapping(&mut doc, &overrides);
    // The fetched YAML is not re-expanded, so an override command it carries is
    // never run. Put the local one back, both to discard the fetched value and
    // to keep `config show` able to display what actually ran.
    doc.insert(Value::String(OVERRIDE_COMMAND_KEY.into()), Value::String(cmd));
    Ok(doc)
}

fn parse_mapping(text: &str, source: &str) -> Result<Mapping> {
    let value: Value = serde_yaml::from_str(text)
        .with_context(|| format!("failed to parse YAML from {source}"))?;
    match value {
        // A file holding only comments parses as null
        Value::Null => Ok(Mapping::new()),
        Value::Mapping(mapping) => Ok(mapping),
        _ => bail!("{source} is not a YAML mapping"),
    }
}

/// Merge `over` into `base`, with `over` winning. Only mappings are merged
/// recursively; scalars and sequences are replaced wholesale, because sequence
/// elements have no stable identity to merge on.
fn merge_mapping(base: &mut Mapping, over: &Mapping) {
    for (key, over_value) in over {
        match (base.get_mut(key), over_value) {
            (Some(Value::Mapping(base_map)), Value::Mapping(over_map)) => {
                merge_mapping(base_map, over_map);
            }
            _ => {
                base.insert(key.clone(), over_value.clone());
            }
        }
    }
}

/// Read a string value. A key that exists but is not a usable string is an
/// error rather than a silent "unset": treating a typo as absent would fall
/// back to a different credential without telling anyone.
fn doc_str(doc: &Mapping, key: &str) -> Result<Option<String>> {
    let Some(value) = doc.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .with_context(|| format!("`{key}` in the config must be a string"))?
        .trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

fn override_command(doc: &Mapping) -> Result<Option<String>> {
    doc_str(doc, OVERRIDE_COMMAND_KEY)
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        buf
    })
}

/// Run `config_override_command` and return its stdout.
///
/// The command is split with shlex and spawned directly, so no shell is
/// involved and shell metacharacters (pipes, redirects, substitutions) are not
/// interpreted. stdout and stderr are drained on separate threads to avoid a
/// deadlock on a full pipe. An API key already present in the environment is
/// withheld from the child so that the command cannot read it back.
fn run_override_command(cmd: &str) -> Result<String> {
    let argv = shlex::split(cmd)
        .with_context(|| format!("invalid {OVERRIDE_COMMAND_KEY} quoting: {cmd}"))?;
    if argv.is_empty() {
        bail!("{OVERRIDE_COMMAND_KEY} is empty");
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        // Withhold the key under both the current and the pre-0.2.0 name. A
        // setup mid-migration still has the old one exported, and the helper
        // (or its diagnostics) must not be able to read the credential back.
        .env_remove("TASKSHOOT_CLI_API_KEY")
        .env_remove("TASKSHOOT_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {OVERRIDE_COMMAND_KEY}: {}", argv[0]))?;
    let stdout_reader = child.stdout.take().map(spawn_reader);
    let stderr_reader = child.stderr.take().map(spawn_reader);
    let status = match child.wait_timeout(OVERRIDE_TIMEOUT)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{OVERRIDE_COMMAND_KEY} timed out after {}s: {cmd} \
                 (it may be waiting on an authentication prompt)",
                OVERRIDE_TIMEOUT.as_secs()
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
            "{OVERRIDE_COMMAND_KEY} failed (exit {:?}): {cmd}\n{}",
            status.code(),
            brief.trim()
        );
    }
    if stdout.trim().is_empty() {
        bail!("{OVERRIDE_COMMAND_KEY} produced no output: {cmd}");
    }
    Ok(stdout)
}

/// Guidance for the case where no API key could be resolved. Setups migrating
/// from the removed .loadenv.sh mechanism get a pointer to what replaced it.
fn missing_api_key_message() -> Result<String> {
    let path = config_path()?;
    let mut message = format!(
        "no API key found.\n\n\
         Set TASKSHOOT_CLI_API_KEY in the environment, or write `api_key:` in\n\
         {}\n\
         Run `taskshoot config init` to create that file.",
        path.display()
    );
    if let Some(legacy) = legacy_setup_hint() {
        message.push_str("\n\n");
        message.push_str(&legacy);
    }
    Ok(message)
}

/// Detect a leftover .loadenv.sh setup, which is no longer read.
fn legacy_setup_hint() -> Option<String> {
    if env::var_os(LEGACY_GETTER_ENV).is_some() {
        return Some(format!(
            "{LEGACY_GETTER_ENV} is set but is no longer supported. \
             Move the command to `{OVERRIDE_COMMAND_KEY}:` in the config file; \
             it must now print YAML instead of KEY=VALUE lines."
        ));
    }
    let found = env::current_dir()
        .ok()?
        .ancestors()
        .map(|dir| dir.join(".loadenv.sh"))
        .find(|path| path.is_file())?;
    Some(format!(
        "{} exists but is no longer read. Move its getter command to \
         `{OVERRIDE_COMMAND_KEY}:` in the config file; it must now print YAML \
         instead of KEY=VALUE lines. `taskshoot trust` has been removed.",
        found.display()
    ))
}

/// `taskshoot config path`
///
/// stdout carries the bare path so that `$(taskshoot config path)` can be fed
/// straight to an editor or a file operation; the "not created yet" note goes
/// to stderr rather than corrupting that value.
pub fn print_config_path() -> Result<()> {
    let path = config_path()?;
    println!("{}", path.display());
    if !path.exists() {
        eprintln!("taskshoot: not created yet; run `taskshoot config init`");
    }
    Ok(())
}

/// `taskshoot config init`
pub fn init_config() -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let path = config_path_in(&dir);
    if path.exists() {
        tighten_permissions(&path)?;
        println!("already exists: {}", path.display());
        return Ok(());
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(CONFIG_TEMPLATE.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("created: {}", path.display());
    Ok(())
}

/// `taskshoot config show` — the merged configuration, with the API key masked.
pub fn show_config(json: bool) -> Result<()> {
    let mut doc = load_merged_config()?;
    if let Some(Value::String(api_key)) = doc.get("api_key") {
        let masked = Value::String(mask_secret(api_key));
        doc.insert(Value::String("api_key".into()), masked);
    }
    let value = Value::Mapping(doc);
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", serde_yaml::to_string(&value)?);
    }
    Ok(())
}

/// Keep enough of a secret to tell two keys apart, without printing a usable one.
fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 12 {
        return "***".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(yaml: &str) -> Mapping {
        parse_mapping(yaml, "test").unwrap()
    }

    #[test]
    fn parse_mapping_accepts_comment_only_file() {
        assert!(mapping("# nothing here\n").is_empty());
        assert!(mapping("").is_empty());
    }

    #[test]
    fn parse_mapping_rejects_non_mapping() {
        assert!(parse_mapping("- a\n- b\n", "test").is_err());
        assert!(parse_mapping("api_key: [\n", "test").is_err());
    }

    #[test]
    fn merge_prefers_override_and_recurses_into_mappings() {
        let mut base = mapping("api_key: local\napi_origin: http://127.0.0.1:8008\n");
        merge_mapping(&mut base, &mapping("api_key: fetched\norganization: acme\n"));
        assert_eq!(doc_str(&base, "api_key").unwrap().unwrap(), "fetched");
        assert_eq!(doc_str(&base, "organization").unwrap().unwrap(), "acme");
        // Untouched keys survive the merge
        assert_eq!(
            doc_str(&base, "api_origin").unwrap().unwrap(),
            "http://127.0.0.1:8008"
        );

        let mut nested = mapping("outer:\n  keep: old\n  replace: old\n");
        merge_mapping(&mut nested, &mapping("outer:\n  replace: new\n"));
        let outer = nested.get("outer").unwrap().as_mapping().unwrap();
        assert_eq!(doc_str(outer, "keep").unwrap().unwrap(), "old");
        assert_eq!(doc_str(outer, "replace").unwrap().unwrap(), "new");
    }

    #[test]
    fn merge_replaces_sequences_wholesale() {
        let mut base = mapping("items:\n  - a\n  - b\n");
        merge_mapping(&mut base, &mapping("items:\n  - c\n"));
        let items = base.get("items").unwrap().as_sequence().unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn doc_str_treats_blank_and_null_as_unset() {
        let doc = mapping("blank: '   '\nnothing: ~\nvalue: ok\n");
        assert_eq!(doc_str(&doc, "blank").unwrap(), None);
        assert_eq!(doc_str(&doc, "nothing").unwrap(), None);
        assert_eq!(doc_str(&doc, "missing").unwrap(), None);
        assert_eq!(doc_str(&doc, "value").unwrap().unwrap(), "ok");
    }

    #[test]
    fn doc_str_rejects_non_string() {
        let doc = mapping("api_key: 12345\n");
        assert!(doc_str(&doc, "api_key").is_err());
    }

    #[test]
    fn override_command_is_read_as_a_string() {
        let doc = mapping("config_override_command: op read \"op://x/y\"\n");
        assert_eq!(
            override_command(&doc).unwrap().unwrap(),
            "op read \"op://x/y\""
        );
        assert_eq!(override_command(&mapping("api_key: k\n")).unwrap(), None);
        assert!(override_command(&mapping("config_override_command: []\n")).is_err());
    }

    #[test]
    fn or_from_config_ignores_a_shadowed_bad_value() {
        let doc = mapping("api_key: 12345\n");
        // A higher-precedence source wins without the file value being parsed
        assert_eq!(
            or_from_config(Some("from-env".into()), &doc, "api_key")
                .unwrap()
                .unwrap(),
            "from-env"
        );
        // The same bad value is still reported when it is the one being used
        assert!(or_from_config(None, &doc, "api_key").is_err());
    }

    #[test]
    fn config_is_needed_only_skips_when_nothing_could_change() {
        // Every value already resolved: the file cannot contribute
        assert!(!config_is_needed(true, true, true, true));
        // me / orgs / notifications need no org, so the org is irrelevant
        assert!(!config_is_needed(false, true, true, false));

        // Any missing piece means the file must be read
        assert!(config_is_needed(true, false, true, true)); // no key
        assert!(config_is_needed(true, true, false, true)); // no origin
        assert!(config_is_needed(true, true, true, false)); // org still unresolved
        assert!(config_is_needed(false, true, false, true)); // no origin, org not needed
        assert!(config_is_needed(false, false, true, true)); // no key, org not needed
        assert!(config_is_needed(true, false, false, false));
    }

    #[test]
    fn config_path_prefers_yml_over_yaml() {
        let dir = std::env::temp_dir().join(format!("taskshoot-cfg-{}", std::process::id()));
        // Start clean: a directory left behind by an earlier panic would make
        // the "neither file exists" assertion below fail.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Neither file present: the path `config init` would create
        assert_eq!(config_path_in(&dir), dir.join("config.yml"));

        std::fs::write(dir.join("config.yaml"), "").unwrap();
        assert_eq!(config_path_in(&dir), dir.join("config.yaml"));

        std::fs::write(dir.join("config.yml"), "").unwrap();
        assert_eq!(config_path_in(&dir), dir.join("config.yml"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mask_secret_keeps_only_the_edges() {
        assert_eq!(mask_secret("tssk-abcdefghijkl"), "tssk...ijkl");
        assert_eq!(mask_secret("short"), "***");
    }
}
