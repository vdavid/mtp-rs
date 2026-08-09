use serde::Serialize;

use crate::cli::args::{Cli, LsArgs};
use crate::cli::device::open_storage;
use crate::cli::error::CliError;
use crate::cli::helpers::folder_parent;
use crate::cli::output::{print_json, ObjectRow};
use crate::cli::path::RemotePath;

#[derive(Debug, Serialize)]
struct LsRow {
    path: String,
    recursive: bool,
    objects: Vec<ObjectRow>,
    /// Handles the device enumerated but wouldn't describe. Always present (empty
    /// when everything was readable) so a consumer can check one field instead of
    /// having to know the key is sometimes absent.
    skipped: Vec<SkippedRow>,
}

/// One object the device listed but refused to describe.
#[derive(Debug, Serialize)]
struct SkippedRow {
    handle: u64,
    error: String,
}

pub async fn run(cli: &Cli, args: &LsArgs) -> Result<(), CliError> {
    let (_device, storage) = open_storage(cli, false).await?;
    let path = RemotePath::parse(&args.remote_path)?;
    let (parent, listed_path) = folder_parent(&storage, &path, cli.verbose).await?;
    // Collect rather than list: a folder with an unreadable entry should say so,
    // not quietly come back one file short.
    let collection = if args.recursive {
        storage
            .collect_objects_recursive(parent)
            .await
            .map_err(|e| CliError::from_mtp("list remote folder", e, cli.verbose))?
    } else {
        storage
            .collect_objects(parent)
            .await
            .map_err(|e| CliError::from_mtp("list remote folder", e, cli.verbose))?
    };
    let rows: Vec<ObjectRow> = collection.objects.iter().map(ObjectRow::from).collect();
    let skipped: Vec<SkippedRow> = collection
        .skipped
        .iter()
        .map(|s| SkippedRow {
            handle: s.handle.0,
            error: s.error.to_string(),
        })
        .collect();

    // Always on stderr, in both modes: it keeps stdout clean for pipes and for
    // the JSON parser, and a human running `--json` still learns the listing is
    // short. Scripts read the `skipped` field instead.
    if !skipped.is_empty() {
        eprintln!(
            "warning: {} object{} could not be read and {} left out:",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" },
            if skipped.len() == 1 { "was" } else { "were" },
        );
        for s in &skipped {
            eprintln!("  handle={} {}", s.handle, s.error);
        }
    }

    if cli.json {
        return print_json(&LsRow {
            path: listed_path,
            recursive: args.recursive,
            objects: rows,
            skipped,
        });
    }

    for row in rows {
        let kind = if row.kind == "folder" { "DIR " } else { "FILE" };
        println!(
            "{} {:>12} handle={} {}",
            kind, row.size, row.handle, row.filename
        );
    }
    Ok(())
}
