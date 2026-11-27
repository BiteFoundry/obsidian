//! Hyphen-prefixed free-text argument normalization for [`Cli`].

use std::ffi::OsString;

use clap::{CommandFactory, Parser};

use super::{Cli, Commands, NotesCmd};

impl Cli {
    /// Parses `std::env::args_os()`, allowing leading-hyphen free-text values on supported
    /// options without swallowing known flags or `--help`.
    pub fn parse_args() -> Self {
        Self::parse_from_args(std::env::args_os())
    }

    /// Parses CLI arguments with hyphen-value normalization and exits on clap error.
    pub fn parse_from_args<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        match Self::try_parse_from_args(itr) {
            Ok(cli) => cli,
            Err(err) => err.exit(),
        }
    }

    /// Parses CLI arguments with hyphen-value normalization, returning clap errors directly.
    pub fn try_parse_from_args<I, T>(itr: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let cli = <Self as Parser>::try_parse_from(normalize_cli_args(itr))?;
        let detail_flag = match &cli.command {
            Some(Commands::Notes {
                action: NotesCmd::Read { compact, full, .. },
            }) => *compact || *full,
            Some(Commands::Daily { full, .. }) => *full,
            _ => false,
        };
        if detail_flag && !cli.json {
            return Err(<Self as clap::CommandFactory>::command().error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--compact and --full require --json",
            ));
        }
        Ok(cli)
    }
}

fn is_hyphen_text_option(cmd_path: &[&str], long_name: &str) -> bool {
    matches!(
        (cmd_path, long_name),
        (["daily"], "append")
            | (["notes", "create"], "content")
            | (["notes", "replace"], "find" | "replace")
            | (["toggle-task"], "expected-text")
    )
}

fn hyphen_positional_slots(cmd_path: &[&str]) -> usize {
    match cmd_path {
        ["notes", "read" | "replace" | "edit" | "delete"] => 1,
        ["notes", "append" | "move"] => 2,
        ["toggle-task"] => 1,
        _ => 0,
    }
}

fn looks_like_flag_typo(token: &str) -> bool {
    if let Some(rest) = token.strip_prefix("--") {
        let name = rest.split('=').next().unwrap_or(rest);
        return !name.is_empty()
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    }
    if let Some(rest) = token.strip_prefix('-') {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphabetic());
    }
    false
}

fn is_known_flag(cmd: &clap::Command, token: &str) -> bool {
    if token == "--" {
        return true;
    }
    if let Some(rest) = token.strip_prefix("--") {
        let name = rest.split('=').next().unwrap_or(rest);
        if name.is_empty() {
            return false;
        }
        return cmd.get_arguments().any(|arg| {
            arg.get_long() == Some(name)
                || arg
                    .get_all_aliases()
                    .is_some_and(|aliases| aliases.contains(&name))
        });
    }
    if let Some(rest) = token.strip_prefix('-') {
        if rest.is_empty() {
            return false;
        }
        return rest.chars().all(|ch| {
            cmd.get_arguments().any(|arg| {
                arg.get_short() == Some(ch)
                    || arg
                        .get_all_short_aliases()
                        .is_some_and(|aliases| aliases.contains(&ch))
            })
        });
    }
    false
}

pub(super) fn has_potential_hyphen_option_literal(raw: &[OsString]) -> bool {
    let mut args = raw.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        let Some(token) = arg.to_str() else {
            continue;
        };
        if token == "--" {
            return false;
        }
        if matches!(
            token,
            "--append" | "--content" | "--find" | "--replace" | "--expected-text"
        ) && args
            .peek()
            .and_then(|next| next.to_str())
            .is_some_and(|next| next != "--" && next.len() > 1 && next.starts_with('-'))
        {
            return true;
        }
    }
    false
}

fn has_potential_hyphen_note_positional(raw: &[OsString]) -> bool {
    let mut saw_target_cmd = false;
    for arg in raw.iter().skip(1) {
        let Some(token) = arg.to_str() else {
            continue;
        };
        if token == "--" {
            return false;
        }
        if matches!(token, "notes" | "toggle-task") {
            saw_target_cmd = true;
            continue;
        }
        if saw_target_cmd
            && token.len() > 1
            && token.starts_with('-')
            && !looks_like_flag_typo(token)
        {
            return true;
        }
    }
    false
}

fn normalize_cli_args<I, T>(itr: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw: Vec<OsString> = itr.into_iter().map(Into::into).collect();
    if !has_potential_hyphen_option_literal(&raw) && !has_potential_hyphen_note_positional(&raw) {
        return raw;
    }

    let mut root = <Cli as CommandFactory>::command();
    root.build();

    let mut out = Vec::with_capacity(raw.len() + 1);
    out.push(raw[0].clone());

    let mut cmd: &clap::Command = &root;
    let mut cmd_path: Vec<&str> = Vec::new();
    let mut deferred_positionals: Vec<OsString> = Vec::new();
    let mut has_hyphen_positional = false;
    let mut i = 1usize;

    while i < raw.len() {
        let slots = hyphen_positional_slots(&cmd_path);
        let Some(token) = raw[i].to_str() else {
            if slots > 0 {
                deferred_positionals.push(raw[i].clone());
            } else {
                out.push(raw[i].clone());
            }
            i += 1;
            continue;
        };

        if token == "--" {
            if has_hyphen_positional {
                out.push(raw[i].clone());
                out.append(&mut deferred_positionals);
                out.extend_from_slice(&raw[i + 1..]);
            } else {
                out.append(&mut deferred_positionals);
                out.extend_from_slice(&raw[i..]);
            }
            return out;
        }

        if let Some(long_rest) = token.strip_prefix("--") {
            let (name, attached) = match long_rest.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long_rest, None),
            };
            let matched_arg = cmd.get_arguments().find(|arg| {
                arg.get_long() == Some(name)
                    || arg
                        .get_all_aliases()
                        .is_some_and(|aliases| aliases.contains(&name))
            });
            if let Some(arg_def) = matched_arg {
                if attached.is_none() && arg_def.get_action().takes_values() {
                    if let Some(next_os) = raw.get(i + 1) {
                        let next_str = next_os.to_str();
                        let next_starts_with_dash =
                            next_str.is_some_and(|s| s.len() > 1 && s.starts_with('-'));
                        if is_hyphen_text_option(&cmd_path, name) {
                            if next_starts_with_dash && !is_known_flag(cmd, next_str.unwrap()) {
                                let mut combined = raw[i].clone();
                                combined.push("=");
                                combined.push(next_os);
                                out.push(combined);
                                i += 2;
                                continue;
                            }
                            if !next_starts_with_dash {
                                out.push(raw[i].clone());
                                out.push(next_os.clone());
                                i += 2;
                                continue;
                            }
                            out.push(raw[i].clone());
                            i += 1;
                            continue;
                        } else if !next_starts_with_dash {
                            out.push(raw[i].clone());
                            out.push(next_os.clone());
                            i += 2;
                            continue;
                        }
                    }
                }
                out.push(raw[i].clone());
                i += 1;
                continue;
            }
            if slots > 0 && deferred_positionals.len() < slots && !looks_like_flag_typo(token) {
                deferred_positionals.push(raw[i].clone());
                has_hyphen_positional = true;
                i += 1;
                continue;
            }
            out.push(raw[i].clone());
            i += 1;
            continue;
        }

        if token.len() > 1 && token.starts_with('-') {
            if !is_known_flag(cmd, token)
                && slots > 0
                && deferred_positionals.len() < slots
                && !looks_like_flag_typo(token)
            {
                deferred_positionals.push(raw[i].clone());
                has_hyphen_positional = true;
                i += 1;
                continue;
            }
            out.push(raw[i].clone());
            i += 1;
            continue;
        }

        if slots == 0 {
            if let Some(sub) = cmd.get_subcommands().find(|sc| {
                sc.get_name() == token || sc.get_all_aliases().any(|alias| alias == token)
            }) {
                cmd_path.push(sub.get_name());
                cmd = sub;
            }
            out.push(raw[i].clone());
        } else {
            deferred_positionals.push(raw[i].clone());
        }
        i += 1;
    }

    if has_hyphen_positional {
        out.push(OsString::from("--"));
    }
    out.extend(deferred_positionals);
    out
}
