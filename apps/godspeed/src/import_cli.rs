//! `godspeed import <t3|claude|codex|cursor> [--dry-run] [--project <path>]`.
//!
//! Imports run inside the engine that owns the workspace (the app, or
//! `godspeed headless`), reached over IPC. A dry run also works with no
//! engine running: it then reads the source alone and can't tell which
//! chats this workspace already has.

use std::path::Path;

use zeron_engine::chat_import::{ImportRoots, preview_source_only};
use zeron_proto::chat_import::{
    ChatImportPreview, ChatImportProgress, ChatImportRequest, ChatImportSource, ChatImportState,
    ChatImportSummary,
};

pub async fn import(
    ipc_port: u16,
    source: ChatImportSource,
    dry_run: bool,
    projects: Vec<String>,
) -> anyhow::Result<()> {
    let projects: Vec<String> = projects.iter().map(|p| normalize(p)).collect();
    let client = zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .ok();
    if dry_run {
        let preview = match &client {
            Some(client) => client
                .call_as::<ChatImportPreview>(
                    zeron_rpc::methods::PREVIEW_CHAT_IMPORT,
                    serde_json::json!({ "source": source }),
                )
                .await
                .map_err(|e| anyhow::anyhow!("PreviewChatImport failed: {e}"))?,
            None => {
                println!(
                    "Godspeed isn't running: reading {} alone (chats already in Godspeed aren't detected).",
                    source.label()
                );
                preview_source_only(&ImportRoots::detect(), source)
            }
        };
        print_preview(&preview, &projects);
        return Ok(());
    }
    let Some(client) = client else {
        anyhow::bail!(
            "no engine listening on 127.0.0.1:{ipc_port}: open Godspeed or run `godspeed headless`, then retry"
        );
    };
    let request = ChatImportRequest {
        source,
        projects: (!projects.is_empty()).then_some(projects),
    };
    let mut items = client
        .subscribe(
            zeron_rpc::methods::RUN_CHAT_IMPORT,
            serde_json::to_value(&request)?,
        )
        .await
        .map_err(|e| anyhow::anyhow!("RunChatImport failed: {e}"))?;
    let mut summary: Option<ChatImportSummary> = None;
    while let Some(item) = items.recv().await {
        match serde_json::from_value::<ChatImportProgress>(item) {
            Ok(ChatImportProgress::Start { total }) => {
                println!("Importing {total} chat(s) from {}…", source.label());
            }
            Ok(ChatImportProgress::Chat {
                index,
                total,
                title,
            }) => println!("  [{}/{total}] {title}", index + 1),
            Ok(ChatImportProgress::Summary { summary: done }) => summary = Some(done),
            Err(err) => tracing::warn!(error = %err, "unreadable import progress item"),
        }
    }
    let summary = summary.ok_or_else(|| anyhow::anyhow!("the import ended without a summary"))?;
    println!(
        "Imported {} · updated {} · skipped {} · {} new space(s) · {} message(s) · {} attachment(s)",
        summary.imported,
        summary.updated,
        summary.skipped,
        summary.spaces_created,
        summary.messages,
        summary.attachments
    );
    for error in &summary.errors {
        eprintln!("  error: {error}");
    }
    if summary.errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{} chat(s) failed to import", summary.errors.len())
    }
}

/// `--project` accepts relative paths; sources record absolute ones.
fn normalize(path: &str) -> String {
    let path = std::path::absolute(Path::new(path)).unwrap_or_else(|_| path.into());
    path.to_string_lossy().trim_end_matches('/').to_string()
}

fn print_preview(preview: &ChatImportPreview, only: &[String]) {
    if !preview.found {
        println!(
            "{}: nothing found at {}",
            preview.source.label(),
            preview.location
        );
        return;
    }
    println!("{} ({})", preview.source.label(), preview.location);
    let mut totals = [0usize; 4];
    let states = [
        ChatImportState::New,
        ChatImportState::Update,
        ChatImportState::Imported,
        ChatImportState::Elsewhere,
    ];
    for project in &preview.projects {
        if !only.is_empty() && !only.contains(&project.path) {
            continue;
        }
        let counts = states.map(|s| project.count(s));
        for (total, n) in totals.iter_mut().zip(counts) {
            *total += n;
        }
        let range = project
            .date_range()
            .map(|(first, last)| {
                format!(
                    " · {} → {}",
                    first.format("%Y-%m-%d"),
                    last.format("%Y-%m-%d")
                )
            })
            .unwrap_or_default();
        println!(
            "  {} — {} chat(s): {} new, {} to update, {} imported, {} elsewhere{range}{}",
            project.path,
            project.chats.len(),
            counts[0],
            counts[1],
            counts[2],
            counts[3],
            if project.has_space {
                " · has space"
            } else {
                ""
            },
        );
    }
    println!(
        "Would import {} and update {}; {} already imported, {} present via another source.",
        totals[0], totals[1], totals[2], totals[3]
    );
    for warning in &preview.warnings {
        eprintln!("  warning: {warning}");
    }
}
