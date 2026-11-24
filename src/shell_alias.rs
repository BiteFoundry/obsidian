//! Opt-in shell alias generation (`alias snippet`) and idempotent profile management (`alias install/uninstall/status`).

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use serde_json::json;

pub const DEFAULT_OBSIDIAN_ALIAS: &str = "ob";
const TARGET_BIN: &str = "bitefoundry-obsidian";

const BLOCK_START: &str = "# >>> bitefoundry obsidian alias >>>";
const BLOCK_END: &str = "# <<< bitefoundry obsidian alias <<<";

const RESERVED_NAMES: &[&str] = &[
    "bitefoundry-obsidian",
    "if",
    "then",
    "else",
    "elif",
    "fi",
    "for",
    "in",
    "do",
    "done",
    "while",
    "until",
    "case",
    "esac",
    "function",
    "return",
    "exit",
    "end",
    "begin",
    "switch",
    "param",
    "break",
    "continue",
    "cd",
    "ls",
    "pwd",
    "echo",
    "export",
    "set",
    "unset",
    "alias",
    "unalias",
    "source",
    "eval",
    "exec",
    "test",
    "true",
    "false",
];

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    #[value(aliases = ["pwsh", "ps"])]
    Powershell,
    Zsh,
    #[value(alias = "sh")]
    Bash,
    Fish,
}

impl ShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Powershell => "powershell",
            ShellKind::Zsh => "zsh",
            ShellKind::Bash => "bash",
            ShellKind::Fish => "fish",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum AliasCommand {
    /// Install an opt-in short shell alias (default: `ob` -> `bitefoundry-obsidian`) into your shell profile.
    Install {
        /// Target shell (auto-detected if omitted).
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        /// Explicit shell profile file path.
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Alias name for `bitefoundry-obsidian`.
        #[arg(long, default_value = DEFAULT_OBSIDIAN_ALIAS)]
        name: String,
        /// Define the alias even if another command with the same name exists on PATH.
        #[arg(long)]
        force: bool,
        /// Preview the profile path and managed block without modifying any file.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Remove the managed BiteFoundry Obsidian alias block from your shell profile.
    Uninstall {
        /// Target shell (auto-detected if omitted).
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        /// Explicit shell profile file path.
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Preview removal without modifying any file.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Show profile path, installation status, and PATH collision checks.
    Status {
        /// Target shell (auto-detected if omitted).
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        /// Explicit shell profile file path.
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Alias name to check for PATH collisions.
        #[arg(long, default_value = DEFAULT_OBSIDIAN_ALIAS)]
        name: String,
    },
    /// Print the shell initialization snippet defining the short alias (for `eval` / `Invoke-Expression`).
    #[command(visible_alias = "init")]
    Snippet {
        /// Target shell (auto-detected if omitted).
        #[arg(value_enum)]
        shell: Option<ShellKind>,
        /// Alias name for `bitefoundry-obsidian`.
        #[arg(long, default_value = DEFAULT_OBSIDIAN_ALIAS)]
        name: String,
        /// Define the alias even if another command with the same name exists on PATH.
        #[arg(long)]
        force: bool,
    },
}

pub fn detect_shell() -> ShellKind {
    if let Ok(shell_env) = env::var("SHELL") {
        let lower = shell_env.to_ascii_lowercase();
        if lower.contains("fish") {
            return ShellKind::Fish;
        }
        if lower.contains("zsh") {
            return ShellKind::Zsh;
        }
        if lower.contains("bash") {
            return ShellKind::Bash;
        }
    }
    if cfg!(windows) {
        return ShellKind::Powershell;
    }
    if cfg!(target_os = "macos") {
        return ShellKind::Zsh;
    }
    ShellKind::Bash
}

pub fn validate_alias_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("Alias name cannot be empty.");
    }
    let mut chars = trimmed.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        bail!("Invalid alias name '{trimmed}': must start with an ASCII letter or '_'.");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("Invalid alias name '{trimmed}': only ASCII letters, digits, '_', and '-' are allowed.");
    }
    if RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(trimmed))
    {
        bail!("Alias name '{trimmed}' is reserved. Choose a different alias name.");
    }
    Ok(trimmed.to_string())
}

pub fn render_init_script(shell: ShellKind, name: &str, force: bool) -> String {
    match shell {
        ShellKind::Powershell => {
            if force {
                format!(
                    "Remove-Item -Path Alias:{name} -ErrorAction SilentlyContinue\nfunction global:{name} {{ {TARGET_BIN} @args }}\n"
                )
            } else {
                format!(
                    "if (-not (Get-Command '{name}' -CommandType Alias,Function,Application,Cmdlet,ExternalScript -ErrorAction SilentlyContinue)) {{\n    function global:{name} {{ {TARGET_BIN} @args }}\n}}\n"
                )
            }
        }
        ShellKind::Zsh | ShellKind::Bash => {
            if force {
                format!(
                    "unalias {name} 2>/dev/null || true\n{name}() {{ {TARGET_BIN} \"$@\"; }}\n"
                )
            } else {
                format!(
                    "if ! command -v {name} >/dev/null 2>&1; then\n  {name}() {{ {TARGET_BIN} \"$@\"; }}\nfi\n"
                )
            }
        }
        ShellKind::Fish => {
            if force {
                format!(
                    "function {name} --wraps {TARGET_BIN} --description 'BiteFoundry Obsidian'\n    {TARGET_BIN} $argv\nend\n"
                )
            } else {
                format!(
                    "if not type -q {name}\n    function {name} --wraps {TARGET_BIN} --description 'BiteFoundry Obsidian'\n        {TARGET_BIN} $argv\n    end\nend\n"
                )
            }
        }
    }
}

pub fn render_managed_block(shell: ShellKind, name: &str, force: bool) -> String {
    let body = render_init_script(shell, name, force);
    format!("{BLOCK_START}\n{body}{BLOCK_END}")
}

pub fn default_profile_paths(shell: ShellKind) -> Result<Vec<PathBuf>> {
    match shell {
        ShellKind::Powershell => {
            if let Some(p) = env::var_os("PROFILE").filter(|v| !v.is_empty()) {
                return Ok(vec![PathBuf::from(p)]);
            }
            let docs = match dirs::document_dir() {
                Some(d) => d,
                None => dirs::home_dir()
                    .context("Could not determine home directory")?
                    .join("Documents"),
            };
            let ps7 = docs
                .join("PowerShell")
                .join("Microsoft.PowerShell_profile.ps1");
            if cfg!(windows) {
                let ps5 = docs
                    .join("WindowsPowerShell")
                    .join("Microsoft.PowerShell_profile.ps1");
                let mut paths = Vec::new();
                if ps7.exists() || ps7.parent().is_some_and(|p| p.exists()) {
                    paths.push(ps7.clone());
                }
                if ps5.exists() || ps5.parent().is_some_and(|p| p.exists()) {
                    paths.push(ps5);
                }
                if paths.is_empty() {
                    paths.push(ps7);
                }
                Ok(paths)
            } else {
                Ok(vec![ps7])
            }
        }
        ShellKind::Zsh => {
            if let Some(zdot) = env::var_os("ZDOTDIR").filter(|v| !v.is_empty()) {
                return Ok(vec![PathBuf::from(zdot).join(".zshrc")]);
            }
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![home.join(".zshrc")])
        }
        ShellKind::Bash => {
            let home = dirs::home_dir().context("Could not determine home directory")?;
            if cfg!(target_os = "macos") {
                let bashrc = home.join(".bashrc");
                let bash_profile = home.join(".bash_profile");
                if !bashrc.exists() && bash_profile.exists() {
                    return Ok(vec![bash_profile]);
                }
            }
            Ok(vec![home.join(".bashrc")])
        }
        ShellKind::Fish => {
            if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
                return Ok(vec![PathBuf::from(xdg).join("fish").join("config.fish")]);
            }
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![home.join(".config").join("fish").join("config.fish")])
        }
    }
}

pub fn find_path_collision(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    let pathext: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.PS1".to_string())
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    for dir in env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if cfg!(windows) {
            let direct = dir.join(name);
            if direct.is_file() {
                return Some(direct);
            }
            for ext in &pathext {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        } else {
            let candidate = dir.join(name);
            if candidate.is_file() && is_executable_unix(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_unix(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_unix(_path: &Path) -> bool {
    true
}

pub fn extract_managed_block(content: &str) -> Option<String> {
    let normalized = content.replace("\r\n", "\n");
    let start = normalized.find(BLOCK_START)?;
    let rest = &normalized[start..];
    let end_rel = rest.find(BLOCK_END)?;
    let end = start + end_rel + BLOCK_END.len();
    Some(normalized[start..end].to_string())
}

pub fn upsert_managed_block(existing: &str, block: &str) -> (String, bool) {
    let use_crlf = existing.contains("\r\n");
    let normalized = existing.replace("\r\n", "\n");
    let block_norm = block.replace("\r\n", "\n");

    let updated_norm = if let Some(start) = normalized.find(BLOCK_START) {
        let rest = &normalized[start..];
        if let Some(end_rel) = rest.find(BLOCK_END) {
            let end = start + end_rel + BLOCK_END.len();
            let mut out = String::with_capacity(normalized.len() + block_norm.len());
            out.push_str(&normalized[..start]);
            out.push_str(&block_norm);
            out.push_str(&normalized[end..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out
        } else {
            let trimmed = normalized.trim_end_matches('\n');
            format!("{trimmed}\n\n{block_norm}\n")
        }
    } else if normalized.trim().is_empty() {
        format!("{block_norm}\n")
    } else {
        let trimmed = normalized.trim_end_matches('\n');
        format!("{trimmed}\n\n{block_norm}\n")
    };

    let changed = updated_norm != normalized;
    let final_text = if use_crlf {
        updated_norm.replace('\n', "\r\n")
    } else {
        updated_norm
    };
    (final_text, changed)
}

pub fn remove_managed_block(existing: &str) -> (String, bool) {
    let use_crlf = existing.contains("\r\n");
    let normalized = existing.replace("\r\n", "\n");

    let Some(start) = normalized.find(BLOCK_START) else {
        return (existing.to_string(), false);
    };
    let rest = &normalized[start..];
    let Some(end_rel) = rest.find(BLOCK_END) else {
        return (existing.to_string(), false);
    };
    let mut end = start + end_rel + BLOCK_END.len();
    if normalized[end..].starts_with('\n') {
        end += 1;
    }

    let before = normalized[..start].trim_end_matches('\n');
    let after = normalized[end..].trim_start_matches('\n');

    let updated_norm = match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => {
            if after.ends_with('\n') {
                after.to_string()
            } else {
                format!("{after}\n")
            }
        }
        (false, true) => format!("{before}\n"),
        (false, false) => {
            if after.ends_with('\n') {
                format!("{before}\n\n{after}")
            } else {
                format!("{before}\n\n{after}\n")
            }
        }
    };

    let final_text = if use_crlf {
        updated_norm.replace('\n', "\r\n")
    } else {
        updated_norm
    };
    (final_text, true)
}

pub fn cmd_alias(action: AliasCommand, json_out: bool) -> Result<()> {
    match action {
        AliasCommand::Snippet { shell, name, force } => {
            let shell = shell.unwrap_or_else(detect_shell);
            let alias_name = validate_alias_name(&name)?;
            let script = render_init_script(shell, &alias_name, force);
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "action": "snippet",
                        "shell": shell.as_str(),
                        "alias": alias_name,
                        "target": TARGET_BIN,
                        "force": force,
                        "script": script,
                    }))?
                );
            } else {
                print!("{script}");
            }
            Ok(())
        }
        AliasCommand::Install {
            shell,
            profile,
            name,
            force,
            dry_run,
        } => {
            let shell = shell.unwrap_or_else(detect_shell);
            let alias_name = validate_alias_name(&name)?;
            let profile_paths = match profile {
                Some(p) => vec![p],
                None => default_profile_paths(shell)?,
            };

            let collision = find_path_collision(&alias_name);
            let block = render_managed_block(shell, &alias_name, force);

            let mut any_changed = false;
            let mut results = Vec::with_capacity(profile_paths.len());
            for profile_path in &profile_paths {
                let existing = if profile_path.exists() {
                    fs::read_to_string(profile_path).with_context(|| {
                        format!("Failed to read shell profile {}", profile_path.display())
                    })?
                } else {
                    String::new()
                };

                let (updated, changed) = upsert_managed_block(&existing, &block);
                if changed {
                    any_changed = true;
                }

                if !dry_run && changed {
                    if let Some(parent) = profile_path.parent() {
                        if !parent.as_os_str().is_empty() && !parent.exists() {
                            fs::create_dir_all(parent).with_context(|| {
                                format!("Failed to create profile directory {}", parent.display())
                            })?;
                        }
                    }
                    fs::write(profile_path, updated).with_context(|| {
                        format!("Failed to write shell profile {}", profile_path.display())
                    })?;
                }
                results.push((profile_path.clone(), changed));
            }

            let primary_path = &profile_paths[0];
            let reload_hint = reload_instruction(shell, primary_path);
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "action": "install",
                        "shell": shell.as_str(),
                        "profile": primary_path.display().to_string(),
                        "profiles": results.iter().map(|(p, ch)| json!({
                            "path": p.display().to_string(),
                            "changed": ch,
                        })).collect::<Vec<_>>(),
                        "alias": alias_name,
                        "target": TARGET_BIN,
                        "force": force,
                        "dry_run": dry_run,
                        "changed": any_changed,
                        "collision": collision.as_ref().map(|p| p.display().to_string()),
                        "block": block,
                        "reload_hint": reload_hint,
                    }))?
                );
            } else {
                for (profile_path, changed) in &results {
                    if dry_run {
                        println!(
                            "[dry-run] Would {} alias ({alias_name} -> {TARGET_BIN}) in {} ({}).",
                            if *changed { "update" } else { "keep existing" },
                            profile_path.display(),
                            shell.as_str()
                        );
                    } else if *changed {
                        println!(
                            "Installed alias ({alias_name} -> {TARGET_BIN}) in {}.",
                            profile_path.display()
                        );
                    } else {
                        println!(
                            "Alias ({alias_name} -> {TARGET_BIN}) is already up to date in {}.",
                            profile_path.display()
                        );
                    }
                }
                if dry_run {
                    println!("\n{block}");
                } else if any_changed {
                    println!("Reload your shell or run: {reload_hint}");
                }

                if let Some(ref path) = collision {
                    eprintln!(
                        "Warning: '{alias_name}' already exists on PATH ({}). {}",
                        path.display(),
                        if force {
                            "Overriding because --force was passed."
                        } else {
                            "The guarded alias will skip it; pick another name with --name <NAME> or pass --force."
                        }
                    );
                }
            }
            Ok(())
        }
        AliasCommand::Uninstall {
            shell,
            profile,
            dry_run,
        } => {
            let shell = shell.unwrap_or_else(detect_shell);
            let profile_paths = match profile {
                Some(p) => vec![p],
                None => default_profile_paths(shell)?,
            };

            let mut any_removed = false;
            let mut any_existed = false;
            let mut results = Vec::with_capacity(profile_paths.len());
            for profile_path in &profile_paths {
                let (removed, existed) = if profile_path.exists() {
                    let existing = fs::read_to_string(profile_path).with_context(|| {
                        format!("Failed to read shell profile {}", profile_path.display())
                    })?;
                    let (updated, removed) = remove_managed_block(&existing);
                    if removed && !dry_run {
                        fs::write(profile_path, updated).with_context(|| {
                            format!("Failed to write shell profile {}", profile_path.display())
                        })?;
                    }
                    (removed, true)
                } else {
                    (false, false)
                };
                any_removed |= removed;
                any_existed |= existed;
                results.push((profile_path.clone(), existed, removed));
            }

            let primary_path = &profile_paths[0];
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "action": "uninstall",
                        "shell": shell.as_str(),
                        "profile": primary_path.display().to_string(),
                        "profiles": results.iter().map(|(p, ex, rem)| json!({
                            "path": p.display().to_string(),
                            "profile_exists": ex,
                            "removed": rem,
                        })).collect::<Vec<_>>(),
                        "profile_exists": any_existed,
                        "dry_run": dry_run,
                        "removed": any_removed,
                    }))?
                );
            } else {
                for (profile_path, _, removed) in &results {
                    if *removed {
                        if dry_run {
                            println!(
                                "[dry-run] Would remove BiteFoundry Obsidian alias block from {}.",
                                profile_path.display()
                            );
                        } else {
                            println!(
                                "Removed BiteFoundry Obsidian alias block from {}.",
                                profile_path.display()
                            );
                        }
                    } else {
                        println!(
                            "No BiteFoundry Obsidian alias block found in {}.",
                            profile_path.display()
                        );
                    }
                }
            }
            Ok(())
        }
        AliasCommand::Status {
            shell,
            profile,
            name,
        } => {
            let shell = shell.unwrap_or_else(detect_shell);
            let alias_name = validate_alias_name(&name)?;
            let profile_paths = match profile {
                Some(p) => vec![p],
                None => default_profile_paths(shell)?,
            };

            let mut entries = Vec::with_capacity(profile_paths.len());
            for profile_path in &profile_paths {
                let exists = profile_path.exists();
                let block = if exists {
                    let existing = fs::read_to_string(profile_path).with_context(|| {
                        format!("Failed to read shell profile {}", profile_path.display())
                    })?;
                    extract_managed_block(&existing)
                } else {
                    None
                };
                entries.push((profile_path.clone(), exists, block));
            }

            let primary = &entries[0];
            let installed = entries.iter().any(|(_, _, b)| b.is_some());
            let collision = find_path_collision(&alias_name);

            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "shell": shell.as_str(),
                        "profile": primary.0.display().to_string(),
                        "profile_exists": primary.1,
                        "installed": installed,
                        "block": primary.2,
                        "profiles": entries.iter().map(|(p, ex, b)| json!({
                            "path": p.display().to_string(),
                            "profile_exists": ex,
                            "installed": b.is_some(),
                        })).collect::<Vec<_>>(),
                        "checked_alias": alias_name,
                        "collision": collision.as_ref().map(|p| p.display().to_string()),
                    }))?
                );
            } else {
                println!("Shell:   {}", shell.as_str());
                for (profile_path, _, block) in &entries {
                    println!(
                        "Profile: {} ({})",
                        profile_path.display(),
                        if block.is_some() {
                            "installed"
                        } else {
                            "not installed"
                        }
                    );
                }
                if let Some(ref path) = collision {
                    println!("PATH collision ({alias_name}): {}", path.display());
                }
                if let Some(b) = entries.iter().find_map(|(_, _, b)| b.as_ref()) {
                    println!("\n{b}");
                }
            }
            Ok(())
        }
    }
}

fn reload_instruction(shell: ShellKind, profile_path: &Path) -> String {
    match shell {
        ShellKind::Powershell => format!(". \"{}\"", profile_path.display()),
        ShellKind::Zsh | ShellKind::Bash | ShellKind::Fish => {
            format!("source \"{}\"", profile_path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_alias_names_and_rejects_reserved_or_invalid() {
        assert_eq!(validate_alias_name("ob").unwrap(), "ob");
        assert_eq!(validate_alias_name("obs").unwrap(), "obs");
        assert_eq!(validate_alias_name("my_obs-1").unwrap(), "my_obs-1");
        assert!(validate_alias_name("").is_err());
        assert!(validate_alias_name("1bad").is_err());
        assert!(validate_alias_name("ob;rm").is_err());
        assert!(validate_alias_name("bitefoundry-obsidian").is_err());
        assert!(validate_alias_name("if").is_err());
    }

    #[test]
    fn renders_scripts_for_all_shells() {
        let ps = render_init_script(ShellKind::Powershell, "ob", false);
        assert!(ps.contains("function global:ob { bitefoundry-obsidian @args }"));
        assert!(ps.contains("Get-Command 'ob'"));

        let ps_force = render_init_script(ShellKind::Powershell, "ob", true);
        assert!(!ps_force.contains("Get-Command"));
        assert!(ps_force.contains("Remove-Item -Path Alias:ob -ErrorAction SilentlyContinue"));
        assert!(ps_force.contains("function global:ob { bitefoundry-obsidian @args }"));

        let zsh = render_init_script(ShellKind::Zsh, "ob", false);
        assert!(zsh.contains("ob() { bitefoundry-obsidian \"$@\"; }"));
        assert!(zsh.contains("command -v ob"));

        let fish = render_init_script(ShellKind::Fish, "ob", false);
        assert!(fish.contains("function ob --wraps bitefoundry-obsidian"));
        assert!(fish.contains("not type -q ob"));
    }

    #[test]
    fn powershell_force_script_removes_existing_alias_before_function() {
        let script = render_init_script(ShellKind::Powershell, "ob", true);
        let removal = "Remove-Item -Path Alias:ob -ErrorAction SilentlyContinue";
        let removal_at = script.find(removal).unwrap();
        let function_at = script.find("function global:ob {").unwrap();
        assert!(removal_at < function_at);
    }

    #[test]
    fn powershell_guard_checks_existing_functions() {
        let script = render_init_script(ShellKind::Powershell, "ob", false);
        let guard = script
            .lines()
            .find(|line| line.contains("Get-Command 'ob'"))
            .unwrap();
        let kinds = guard
            .split("-CommandType ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap();
        assert!(kinds.split(',').any(|kind| kind == "Function"));
    }

    #[test]
    fn upsert_and_remove_managed_block_are_idempotent() {
        let block = render_managed_block(ShellKind::Zsh, "ob", false);
        let initial = "export EDITOR=nvim\n";

        let (once, changed1) = upsert_managed_block(initial, &block);
        assert!(changed1);
        assert!(once.starts_with("export EDITOR=nvim\n\n# >>> bitefoundry obsidian alias >>>"));
        assert!(extract_managed_block(&once).is_some());

        let (twice, changed2) = upsert_managed_block(&once, &block);
        assert!(!changed2);
        assert_eq!(once, twice);

        let updated_block = render_managed_block(ShellKind::Zsh, "obs", true);
        let (replaced, changed3) = upsert_managed_block(&once, &updated_block);
        assert!(changed3);
        assert!(replaced.contains("obs() { bitefoundry-obsidian \"$@\"; }"));
        assert!(!replaced.contains("ob() { bitefoundry-obsidian \"$@\"; }"));

        let (removed, was_removed) = remove_managed_block(&replaced);
        assert!(was_removed);
        assert_eq!(removed, "export EDITOR=nvim\n");

        let (removed_again, was_removed_again) = remove_managed_block(&removed);
        assert!(!was_removed_again);
        assert_eq!(removed_again, "export EDITOR=nvim\n");
    }

    #[test]
    fn install_status_uninstall_roundtrip_on_temp_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile.ps1");
        fs::write(&profile, "# existing profile\n").unwrap();

        cmd_alias(
            AliasCommand::Install {
                shell: Some(ShellKind::Powershell),
                profile: Some(profile.clone()),
                name: "ob".into(),
                force: false,
                dry_run: true,
            },
            false,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&profile).unwrap(), "# existing profile\n");

        cmd_alias(
            AliasCommand::Install {
                shell: Some(ShellKind::Powershell),
                profile: Some(profile.clone()),
                name: "ob".into(),
                force: false,
                dry_run: false,
            },
            false,
        )
        .unwrap();
        let installed = fs::read_to_string(&profile).unwrap();
        assert!(installed.contains("function global:ob { bitefoundry-obsidian @args }"));

        cmd_alias(
            AliasCommand::Status {
                shell: Some(ShellKind::Powershell),
                profile: Some(profile.clone()),
                name: "ob".into(),
            },
            false,
        )
        .unwrap();

        cmd_alias(
            AliasCommand::Uninstall {
                shell: Some(ShellKind::Powershell),
                profile: Some(profile.clone()),
                dry_run: false,
            },
            false,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&profile).unwrap(), "# existing profile\n");
    }
}
