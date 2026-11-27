//! Vault management, init, reindex, daemon, doctor, and update CLI handlers.

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::json;

use super::{DaemonCmd, VaultsCmd};
use crate::{
    config::{AppConfig, DEFAULT_SERVE_PORT},
    daemon,
    models::VaultSummary,
    safe_text::sanitize_terminal_inline,
    storage::Storage,
    updater,
    vault::{
        init_starter_vault, merged_vaults, native_obsidian_config_path, resolve_active_vault,
        resolve_registered_vault, vault_from_dir,
    },
};

pub(super) fn handle_vaults(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    action: VaultsCmd,
) -> Result<()> {
    match action {
        VaultsCmd::List => {
            let storage = Storage::open_default()?;
            let active_id = resolve_active_vault(cfg, vault_spec).ok().map(|v| v.id);
            let vaults: Vec<VaultSummary> = merged_vaults(cfg)
                .into_iter()
                .map(|v| {
                    let exists = v.path.is_dir();
                    if exists {
                        let _ = storage.sync_vault(&v);
                    }
                    let active = active_id.as_deref() == Some(v.id.as_str());
                    let note_count = storage.note_count(&v.id).unwrap_or(0);
                    VaultSummary {
                        id: v.id,
                        name: v.name,
                        exists,
                        path: v.path,
                        active,
                        discovered: v.discovered,
                        open_in_obsidian: v.open_in_obsidian,
                        note_count,
                    }
                })
                .collect();
            if as_json {
                println!("{}", serde_json::to_string_pretty(&vaults)?);
            } else if vaults.is_empty() {
                println!("No vaults found. Run `bitefoundry-obsidian init <dir>` or `bitefoundry-obsidian vaults add <path>`.");
            } else {
                for v in vaults {
                    let mark = if v.active { "*" } else { " " };
                    let src = if v.discovered { "auto" } else { "reg " };
                    let path_str = v.path.display().to_string();
                    println!(
                        "{mark} {:<22} [{src}] {:>4} notes  {}",
                        sanitize_terminal_inline(&v.name),
                        v.note_count,
                        sanitize_terminal_inline(&path_str)
                    );
                }
            }
            Ok(())
        }
        VaultsCmd::Add { path, name } => {
            if !path.is_dir() {
                bail!("Directory does not exist: {}", path.display());
            }
            let mut persisted = AppConfig::load_persisted()?;
            let profile = vault_from_dir(&path, name.as_deref());
            let norm_new = crate::config::normalize_path_key(&profile.path);
            if let Some(conflict) = merged_vaults(&persisted).into_iter().find(|v| {
                v.name.eq_ignore_ascii_case(&profile.name)
                    && crate::config::normalize_path_key(&v.path) != norm_new
            }) {
                bail!(
                    "Vault name '{}' is already registered for '{}'. Use a different `--name` or remove the existing vault first.",
                    conflict.name,
                    conflict.path.display()
                );
            }
            persisted.register_vault(profile.clone(), true)?;
            persisted.save()?;
            let storage = Storage::open_default()?;
            let stats = storage.sync_vault(&profile)?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "vault": profile,
                        "stats": stats,
                    }))?
                );
            } else {
                let path_str = profile.path.display().to_string();
                println!(
                    "Added vault '{}' ({}) — indexed {} notes.",
                    sanitize_terminal_inline(&profile.name),
                    sanitize_terminal_inline(&path_str),
                    stats.total_notes
                );
            }
            Ok(())
        }
        VaultsCmd::Use { name } => {
            let mut persisted = AppConfig::load_persisted()?;
            let target = resolve_registered_vault(&persisted, &name)?;
            persisted.active_vault = target.name.clone();
            persisted.save()?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&target)?);
            } else {
                let path_str = target.path.display().to_string();
                println!(
                    "Active vault set to '{}' ({})",
                    sanitize_terminal_inline(&target.name),
                    sanitize_terminal_inline(&path_str)
                );
            }
            Ok(())
        }
        VaultsCmd::Remove { name } => {
            let mut persisted = AppConfig::load_persisted()?;
            let removed = persisted.remove_vault(&name);
            if removed {
                persisted.save()?;
            }
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "removed": removed, "name": name }))?
                );
            } else if removed {
                println!(
                    "Removed vault '{}' from registry.",
                    sanitize_terminal_inline(&name)
                );
            } else {
                println!(
                    "Vault '{}' was not in manual registry.",
                    sanitize_terminal_inline(&name)
                );
            }
            Ok(())
        }
    }
}

pub(super) fn handle_init(as_json: bool, dir: PathBuf) -> Result<()> {
    // Read the stored config first: a broken file must stop the command before the
    // starter vault is created.
    let mut persisted = AppConfig::load_persisted()?;
    let profile = init_starter_vault(&dir)?;
    persisted.upsert_vault(profile.clone(), true);
    persisted.save()?;
    let storage = Storage::open_default()?;
    let stats = storage.sync_vault(&profile)?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "vault": profile,
                "stats": stats,
            }))?
        );
    } else {
        let path_str = profile.path.display().to_string();
        println!(
            "Initialized starter Obsidian vault '{}' at {} ({} notes).",
            sanitize_terminal_inline(&profile.name),
            sanitize_terminal_inline(&path_str),
            stats.total_notes
        );
    }
    Ok(())
}

pub(super) fn handle_reindex(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    let stats = storage.reindex_vault(&vault)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!(
            "Reindexed '{}' in {}ms: {} notes ({} updated, {} removed), {} links, {} tags, {} tasks.",
            sanitize_terminal_inline(&stats.vault_name),
            stats.elapsed_ms,
            stats.total_notes,
            stats.updated,
            stats.removed,
            stats.total_links,
            stats.total_tags,
            stats.total_tasks,
        );
    }
    Ok(())
}

pub(super) async fn handle_daemon(
    cfg: AppConfig,
    vault_spec: Option<String>,
    as_json: bool,
    action: DaemonCmd,
) -> Result<()> {
    match action {
        DaemonCmd::Start => {
            let st = daemon::start_daemon(vault_spec.as_deref()).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!(
                    "bitefoundry-obsidian daemon running (pid={}, vault={})",
                    st.pid.unwrap_or(0),
                    sanitize_terminal_inline(st.vault_name.as_deref().unwrap_or("default"))
                );
            }
            Ok(())
        }
        DaemonCmd::Stop => {
            let stopped = daemon::stop_daemon().await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "stopped": stopped }))?
                );
            } else if stopped {
                println!("bitefoundry-obsidian daemon stopped.");
            } else {
                println!("bitefoundry-obsidian daemon was not running.");
            }
            Ok(())
        }
        DaemonCmd::Status => {
            let st = daemon::daemon_status().await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else if st.running {
                println!(
                    "Running (pid={}, vault={}, notes={}, uptime={}s)",
                    st.pid.unwrap_or(0),
                    sanitize_terminal_inline(st.vault_name.as_deref().unwrap_or("—")),
                    st.total_notes.unwrap_or(0),
                    st.uptime_secs.unwrap_or(0)
                );
            } else {
                println!("bitefoundry-obsidian daemon is stopped.");
            }
            Ok(())
        }
        DaemonCmd::Run => daemon::run_daemon_foreground(cfg, vault_spec).await,
    }
}

pub(super) fn handle_doctor(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let native_cfg = native_obsidian_config_path();
    let vaults = merged_vaults(cfg);
    let active = resolve_active_vault(cfg, vault_spec).ok();
    let caps = crate::img_preview::detect_graphics_caps(cfg.image_protocol);
    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "config_file": crate::config::config_file_path(),
        "db_path": crate::config::db_path(),
        "native_obsidian_json": native_cfg,
        "native_obsidian_json_exists": native_cfg.as_ref().is_some_and(|p| p.is_file()),
        "discovered_and_registered_vaults": vaults.len(),
        "active_vault": active.as_ref().map(|v| json!({ "name": v.name, "path": v.path })),
        "image_protocol": caps.protocol.label(),
        "default_serve_port": DEFAULT_SERVE_PORT,
    });
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("@bitefoundry/obsidian v{}", env!("CARGO_PKG_VERSION"));
        let cfg_path = crate::config::config_file_path().display().to_string();
        let db_path = crate::config::db_path().display().to_string();
        let native_str = native_cfg
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not found".into());
        println!("  Config:        {}", sanitize_terminal_inline(&cfg_path));
        println!("  SQLite Index:  {}", sanitize_terminal_inline(&db_path));
        println!("  obsidian.json: {}", sanitize_terminal_inline(&native_str));
        println!("  Vaults found:  {}", vaults.len());
        if let Some(v) = active {
            let v_path = v.path.display().to_string();
            println!(
                "  Active vault:  {} ({})",
                sanitize_terminal_inline(&v.name),
                sanitize_terminal_inline(&v_path)
            );
        }
        println!("  Image proto:   {}", caps.protocol.label());
    }
    Ok(())
}

pub(super) async fn handle_update(
    cfg: &AppConfig,
    as_json: bool,
    check: bool,
    force: bool,
) -> Result<()> {
    let res = updater::check_or_apply_update(&cfg.github_repo, check, force).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&res)?);
    } else if res.updated {
        println!(
            "Updated obsidian {} -> {}",
            sanitize_terminal_inline(&res.current_version),
            sanitize_terminal_inline(&res.latest_version)
        );
    } else if res.update_available {
        println!(
            "Update available: {} -> {} ({})",
            sanitize_terminal_inline(&res.current_version),
            sanitize_terminal_inline(&res.latest_version),
            sanitize_terminal_inline(&res.release_url)
        );
    } else {
        println!(
            "Already up to date (v{}).",
            sanitize_terminal_inline(&res.current_version)
        );
    }
    Ok(())
}
