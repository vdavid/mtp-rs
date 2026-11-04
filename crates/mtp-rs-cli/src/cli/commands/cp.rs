use mtp_rs::ObjectHandle;
use serde::Serialize;

use crate::cli::args::{Cli, CopyArgs};
use crate::cli::device::open_storage;
use crate::cli::error::{CliError, CliErrorKind};
use crate::cli::helpers::{
    ensure_not_same_object, ensure_rename_supported, existing_object, resolve_object_destination,
};
use crate::cli::output::print_json;
use crate::cli::path::RemotePath;

#[derive(Debug, Serialize)]
struct CopyRow {
    operation: &'static str,
    source_path: String,
    destination_path: String,
    filename: String,
    source_handle: u64,
    handle: u64,
    kind: &'static str,
    replaced: bool,
    renamed: bool,
}

pub async fn run(cli: &Cli, args: &CopyArgs) -> Result<(), CliError> {
    let (device, storage) = open_storage(cli, true).await?;
    let source_path = RemotePath::parse(&args.source_path)?;
    let destination_path = RemotePath::parse(&args.destination_path)?;
    let source = existing_object(&storage, &source_path, cli.verbose).await?;
    let source_name = source.filename.clone();
    let source_kind = if source.is_folder() { "folder" } else { "file" };
    let target =
        resolve_object_destination(&storage, &destination_path, &source_name, cli.verbose).await?;
    let renamed = target.filename != source_name;

    if renamed {
        ensure_rename_supported(&device)?;
    }
    let replaced = target.existing.is_some();
    // The copy itself keeps the source's name and only gets renamed afterwards, so the
    // destination folder mustn't hold that name (the library refuses it, #59). Check before
    // `--replace` deletes anything: otherwise a copy that can't happen costs the destination.
    let siblings = storage
        .list_objects(target.parent)
        .await
        .map_err(|e| CliError::from_mtp("list destination folder", e, cli.verbose))?;
    let replaced_handle = target.existing.as_ref().map(|e| e.handle);
    if let Some(clash) = siblings.iter().find(|o| {
        Some(o.handle) != replaced_handle && o.filename.to_lowercase() == source_name.to_lowercase()
    }) {
        return Err(CliError::new(
            CliErrorKind::RemotePath,
            format!(
                "the destination folder already holds `{}`, and an MTP copy keeps the source's \
                 name until it's renamed; copy into another folder first, then rename and move it",
                clash.filename
            ),
        ));
    }
    if let Some(existing) = &target.existing {
        ensure_not_same_object(source.handle, existing.handle)?;
        if !args.replace {
            return Err(CliError::new(
                CliErrorKind::RemotePath,
                "destination already exists; pass --replace to delete it first",
            ));
        }
        storage
            .delete(existing.handle)
            .await
            .map_err(|e| CliError::from_mtp("delete existing destination", e, cli.verbose))?;
    }

    let parent = target.parent.unwrap_or(ObjectHandle::ROOT);
    let new_handle = storage
        .copy_object(source.handle, parent, None)
        .await
        .map_err(|e| CliError::from_mtp("copy remote object", e, cli.verbose))?;
    if renamed {
        storage
            .rename(new_handle, &target.filename)
            .await
            .map_err(|e| CliError::from_mtp("rename copied object", e, cli.verbose))?;
    }

    let row = CopyRow {
        operation: "cp",
        source_path: source_path.raw().to_string(),
        destination_path: destination_path.raw().to_string(),
        filename: target.filename,
        source_handle: source.handle.0,
        handle: new_handle.0,
        kind: source_kind,
        replaced,
        renamed,
    };

    if cli.json {
        return print_json(&row);
    }

    println!(
        "copied {} -> {} handle={}",
        row.source_path, row.destination_path, row.handle
    );
    Ok(())
}
