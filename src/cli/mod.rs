//! Clap CLI definition & command executor for `bitefoundry-obsidian`.

mod args;
mod normalize;
mod notes;
mod query;
mod vaults;

#[cfg(test)]
mod tests;

pub use args::{Cli, Commands, DaemonCmd, NotesCmd, VaultsCmd};

use anyhow::Result;

use crate::{
    config::AppConfig, mcp::McpServer, serve, shell_alias, storage::Storage, tui, updater,
    vault::resolve_active_vault,
};

pub async fn run(cli: Cli) -> Result<()> {
    updater::cleanup_stale_old_binary();
    let cfg = AppConfig::load();

    match cli.command {
        None => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            tui::run_tui(cfg, vault, storage).await
        }
        Some(Commands::Vaults { action }) => {
            vaults::handle_vaults(&cfg, cli.vault.as_deref(), cli.json, action)
        }
        Some(Commands::Init { dir }) => vaults::handle_init(cli.json, dir),
        Some(Commands::Reindex) => vaults::handle_reindex(&cfg, cli.vault.as_deref(), cli.json),
        Some(Commands::Search { query, limit }) => {
            query::handle_search(&cfg, cli.vault.as_deref(), cli.json, query, limit)
        }
        Some(Commands::Notes { action }) => {
            notes::handle_notes(&cfg, cli.vault.as_deref(), cli.json, action)
        }
        Some(Commands::Daily { date, append, full }) => {
            notes::handle_daily(&cfg, cli.vault.as_deref(), cli.json, date, append, full)
        }
        Some(Commands::Backlinks { note }) => {
            query::handle_backlinks(&cfg, cli.vault.as_deref(), cli.json, note)
        }
        Some(Commands::Links { note }) => {
            query::handle_links(&cfg, cli.vault.as_deref(), cli.json, note)
        }
        Some(Commands::Graph { note, depth }) => {
            query::handle_graph(&cfg, cli.vault.as_deref(), cli.json, note, depth)
        }
        Some(Commands::Orphans) => query::handle_orphans(&cfg, cli.vault.as_deref(), cli.json),
        Some(Commands::Tags { tag }) => {
            query::handle_tags(&cfg, cli.vault.as_deref(), cli.json, tag)
        }
        Some(Commands::Tasks {
            open,
            done,
            note,
            limit,
            cursor,
            all,
        }) => query::handle_tasks(
            &cfg,
            cli.vault.as_deref(),
            cli.json,
            open,
            done,
            note,
            limit,
            cursor,
            all,
        ),
        Some(Commands::ToggleTask {
            note,
            line,
            expected_text,
            expected_checked,
        }) => query::handle_toggle_task(
            &cfg,
            cli.vault.as_deref(),
            cli.json,
            note,
            line,
            expected_text,
            expected_checked,
        ),
        Some(Commands::Daemon { action }) => {
            vaults::handle_daemon(cfg, cli.vault, cli.json, action).await
        }
        Some(Commands::Serve {
            bind,
            port,
            token,
            allow_keyless,
            allow_origin,
            allow_host,
        }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            let opts = serve::ServeOptions {
                token,
                allow_keyless,
                allow_origins: allow_origin,
                allow_hosts: allow_host,
            };
            serve::run_http_server_with_options(cfg, vault, storage, &bind, port, opts).await
        }
        Some(Commands::Mcp) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            let server = McpServer::new(cfg, vault, storage);
            server.run_stdio()
        }
        Some(Commands::Alias { action }) => shell_alias::cmd_alias(action, cli.json),
        Some(Commands::Doctor) => vaults::handle_doctor(&cfg, cli.vault.as_deref(), cli.json),
        Some(Commands::Update { check, force }) => {
            vaults::handle_update(&cfg, cli.json, check, force).await
        }
    }
}
