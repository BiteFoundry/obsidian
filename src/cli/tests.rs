use std::ffi::OsString;

use clap::error::ErrorKind;

use super::normalize::has_potential_hyphen_option_literal;
use super::*;
use crate::shell_alias::AliasCommand;

fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push("bitefoundry-obsidian");
    argv.extend_from_slice(args);
    Cli::try_parse_from_args(argv)
}

#[test]
fn parses_hyphen_values_across_free_text_commands() {
    for sample in ["- 목록 항목", "- first\n- second", "---\nkey: val\n---"] {
        let daily = parse(&["daily", "--append", sample, "--json"]).unwrap();
        assert!(daily.json);
        assert!(matches!(
            daily.command,
            Some(Commands::Daily { date: None, append: Some(ref s), full: false }) if s == sample
        ));

        let daily_eq = parse(&["daily", &format!("--append={sample}")]).unwrap();
        assert!(matches!(
            daily_eq.command,
            Some(Commands::Daily { date: None, append: Some(ref s), full: false }) if s == sample
        ));

        let created_body =
            parse(&["notes", "create", "Test", "--content", sample, "--json"]).unwrap();
        assert!(created_body.json);
        assert!(matches!(
            created_body.command,
            Some(Commands::Notes {
                action: NotesCmd::Create { ref title, content: Some(ref s), .. }
            }) if title == "Test" && s == sample
        ));

        let created_body_eq =
            parse(&["notes", "create", "Test", &format!("--content={sample}")]).unwrap();
        assert!(matches!(
            created_body_eq.command,
            Some(Commands::Notes {
                action: NotesCmd::Create { ref title, content: Some(ref s), .. }
            }) if title == "Test" && s == sample
        ));

        let created_title = parse(&["notes", "create", sample, "--json"]).unwrap();
        assert!(created_title.json);
        assert!(matches!(
            created_title.command,
            Some(Commands::Notes {
                action: NotesCmd::Create { ref title, content: None, .. }
            }) if title == sample
        ));

        let search = parse(&["search", sample, "--limit", "5", "--json"]).unwrap();
        assert!(search.json);
        assert!(matches!(
            search.command,
            Some(Commands::Search { ref query, limit: 5 }) if query == sample
        ));

        let appended = parse(&["notes", "append", "Note", sample, "--json"]).unwrap();
        assert!(appended.json);
        assert!(matches!(
            appended.command,
            Some(Commands::Notes {
                action: NotesCmd::Append { ref note, ref text }
            }) if note == "Note" && text == sample
        ));

        let replaced = parse(&[
            "notes",
            "replace",
            "Note",
            "--find",
            sample,
            "--replace",
            sample,
            "--json",
        ])
        .unwrap();
        assert!(replaced.json);
        assert!(matches!(
            replaced.command,
            Some(Commands::Notes {
                action: NotesCmd::Replace { ref note, ref find, ref replace }
            }) if note == "Note" && find == sample && replace == sample
        ));
    }

    // Global `--vault` whose value matches a subcommand name does not confuse normalization.
    let with_vault = parse(&["--vault", "daily", "daily", "--append", "- 목록 항목"]).unwrap();
    assert_eq!(with_vault.vault.as_deref(), Some("daily"));
    assert!(matches!(
        with_vault.command,
        Some(Commands::Daily { append: Some(ref s), .. }) if s == "- 목록 항목"
    ));

    // Ordinary invocations skip building a second clap command tree.
    let to_os = |args: &[&str]| -> Vec<OsString> { args.iter().map(OsString::from).collect() };
    assert!(!has_potential_hyphen_option_literal(&to_os(&[
        "bitefoundry-obsidian",
        "notes",
        "list",
        "--json",
    ])));
    assert!(!has_potential_hyphen_option_literal(&to_os(&[
        "bitefoundry-obsidian",
        "daily",
        "--append",
        "plain text",
    ])));
    assert!(!has_potential_hyphen_option_literal(&to_os(&[
        "bitefoundry-obsidian",
        "daily",
        "--append=- 목록 항목",
    ])));
    assert!(!has_potential_hyphen_option_literal(&to_os(&[
        "bitefoundry-obsidian",
        "daily",
        "--help",
    ])));
    assert!(!has_potential_hyphen_option_literal(&to_os(&[
        "bitefoundry-obsidian",
        "daily",
        "-h",
    ])));
    assert!(!has_potential_hyphen_option_literal(&to_os(&[
        "bitefoundry-obsidian",
        "notes",
        "append",
        "Note",
        "--",
        "--append",
        "- item",
    ])));
    assert!(has_potential_hyphen_option_literal(&to_os(&[
        "bitefoundry-obsidian",
        "daily",
        "--append",
        "- 목록 항목",
    ])));
}

#[test]
fn preserves_known_flags_missing_values_help_equals_and_positional_separator() {
    // Explicit `=` and `--` support flag-looking literal values.
    let daily_eq = parse(&["daily", "--append=--json"]).unwrap();
    assert!(!daily_eq.json);
    assert!(matches!(
        daily_eq.command,
        Some(Commands::Daily { append: Some(ref s), .. }) if s == "--json"
    ));

    let create_eq = parse(&["notes", "create", "Test", "--content=--help"]).unwrap();
    assert!(matches!(
        create_eq.command,
        Some(Commands::Notes {
            action: NotesCmd::Create { content: Some(ref s), .. }
        }) if s == "--help"
    ));

    let replace_eq = parse(&[
        "notes",
        "replace",
        "Note",
        "--find=--json",
        "--replace=--help",
    ])
    .unwrap();
    assert!(matches!(
        replace_eq.command,
        Some(Commands::Notes {
            action: NotesCmd::Replace { ref find, ref replace, .. }
        }) if find == "--json" && replace == "--help"
    ));

    let search_sep = parse(&["search", "--", "--json"]).unwrap();
    assert!(!search_sep.json);
    assert!(matches!(
        search_sep.command,
        Some(Commands::Search { ref query, .. }) if query == "--json"
    ));

    let create_sep = parse(&["notes", "create", "--", "--help"]).unwrap();
    assert!(matches!(
        create_sep.command,
        Some(Commands::Notes {
            action: NotesCmd::Create { ref title, .. }
        }) if title == "--help"
    ));

    let append_sep = parse(&["notes", "append", "Note", "--", "--json"]).unwrap();
    assert!(!append_sep.json);
    assert!(matches!(
        append_sep.command,
        Some(Commands::Notes {
            action: NotesCmd::Append { ref text, .. }
        }) if text == "--json"
    ));

    // Missing values followed by real flags must fail rather than swallow the flag.
    for missing_value_argv in [
        &["daily", "--append", "--json"][..],
        &["daily", "--append", "--date", "2026-11-20"][..],
        &["daily", "--append", "--vault", "Work"][..],
        &["daily", "--append", "--help"][..],
        &["daily", "--append", "-h"][..],
        &["daily", "--append", "--"][..],
        &["notes", "create", "Test", "--content", "--json"][..],
        &["notes", "create", "Test", "--content", "--tags", "a"][..],
        &["notes", "create", "Test", "--content", "--help"][..],
        &["notes", "create", "--json"][..],
        &["notes", "create", "--content", "- item"][..],
        &["search", "--json"][..],
        &["search", "--limit", "5"][..],
        &["notes", "append", "Note", "--json"][..],
        &["notes", "replace", "Note", "--find", "--replace", "new"][..],
        &[
            "notes",
            "replace",
            "Note",
            "--find",
            "old",
            "--replace",
            "--json",
        ][..],
        &[
            "notes",
            "replace",
            "Note",
            "--find",
            "old",
            "--replace",
            "--help",
        ][..],
        // Non-text options keep standard clap rejection for hyphen-prefixed values.
        &["daily", "--date", "-2026-11-20"][..],
        &["search", "foo", "--limit", "-1"][..],
    ] {
        let err = parse(missing_value_argv)
            .expect_err(&format!("expected parse error for {missing_value_argv:?}"));
        assert_ne!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "unexpected help for {missing_value_argv:?}"
        );
    }

    // Actual help invocations (standalone or after a completed hyphen value) print help.
    for help_argv in [
        &["daily", "--help"][..],
        &["daily", "-h"][..],
        &["daily", "--append", "- 목록 항목", "--help"][..],
        &["notes", "create", "--help"][..],
        &["notes", "create", "- 목록 항목", "--help"][..],
        &[
            "notes",
            "create",
            "Test",
            "--content",
            "- 목록 항목",
            "--help",
        ][..],
        &["search", "--help"][..],
        &["search", "-h"][..],
        &["search", "- 목록 항목", "--help"][..],
        &["notes", "append", "--help"][..],
        &["notes", "append", "Note", "--help"][..],
        &["notes", "append", "Note", "- 목록 항목", "--help"][..],
        &["notes", "replace", "--help"][..],
        &[
            "notes",
            "replace",
            "Note",
            "--find",
            "- 목록 항목",
            "--replace",
            "new",
            "--help",
        ][..],
    ] {
        let err =
            parse(help_argv).expect_err(&format!("expected DisplayHelp for {help_argv:?}"));
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected DisplayHelp for {help_argv:?}, got {err}"
        );
    }
}

#[test]
fn parses_alias_subcommands_with_global_json() {
    let inst = parse(&["alias", "install", "--name", "ob", "--dry-run", "--json"]).unwrap();
    assert!(inst.json);
    assert!(matches!(
        inst.command,
        Some(Commands::Alias {
            action: AliasCommand::Install {
                ref name,
                dry_run: true,
                force: false,
                ..
            }
        }) if name == "ob"
    ));

    let status = parse(&["alias", "status", "--shell", "zsh"]).unwrap();
    assert!(matches!(
        status.command,
        Some(Commands::Alias {
            action: AliasCommand::Status {
                shell: Some(crate::shell_alias::ShellKind::Zsh),
                ref name,
                ..
            }
        }) if name == "ob"
    ));

    let snippet = parse(&["alias", "init", "pwsh", "--force"]).unwrap();
    assert!(matches!(
        snippet.command,
        Some(Commands::Alias {
            action: AliasCommand::Snippet {
                shell: Some(crate::shell_alias::ShellKind::Powershell),
                force: true,
                ..
            }
        })
    ));

    for (flag, expected) in [
        ("tcsh", crate::shell_alias::ShellKind::Tcsh),
        ("csh", crate::shell_alias::ShellKind::Csh),
        ("sh", crate::shell_alias::ShellKind::Sh),
        ("dash", crate::shell_alias::ShellKind::Sh),
        ("ash", crate::shell_alias::ShellKind::Sh),
        ("ksh", crate::shell_alias::ShellKind::Sh),
    ] {
        let parsed = parse(&["alias", "snippet", flag]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::Alias {
                action: AliasCommand::Snippet { shell: Some(s), .. }
            }) if s == expected
        ));
    }
}
