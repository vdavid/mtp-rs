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
    // The copy itself keeps the source's name and only gets renamed afterwards, so it can't be
    // made straight into a folder that already holds that name (the library refuses it, #59).
    // A renamed copy goes through a staging folder instead. Decide before `--replace` deletes
    // anything, so a copy that can't happen never costs the destination.
    let siblings = storage
        .list_objects(target.parent)
        .await
        .map_err(|e| CliError::from_mtp("list destination folder", e, cli.verbose))?;
    let replaced_handle = target.existing.as_ref().map(|e| e.handle);
    let clash = siblings.iter().find(|o| {
        Some(o.handle) != replaced_handle && o.filename.to_lowercase() == source_name.to_lowercase()
    });
    if let (Some(clash), false) = (clash, renamed) {
        // Only a letter-case variant of the source's name gets here: same name, other object.
        return Err(CliError::new(
            CliErrorKind::RemotePath,
            format!(
                "the destination folder already holds `{}`, which the device may treat as the \
                 same name",
                clash.filename
            ),
        ));
    }
    let staged = clash.is_some();
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

    let new_handle = if staged {
        staged_copy(
            cli,
            &storage,
            source.handle,
            target.parent,
            &target.filename,
        )
        .await?
    } else {
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
        new_handle
    };

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

/// Make a renamed copy in a folder that already holds the source's name: copy into a fresh
/// hidden folder next to the destination, rename the copy there, move it into the destination,
/// and remove the staging folder. Returns the copy's handle at its final place.
///
/// The staging folder is removed on failure too, copy and all. If that cleanup fails, the error
/// names the folder so the user can remove it.
async fn staged_copy(
    cli: &Cli,
    storage: &mtp_rs::Storage,
    source: ObjectHandle,
    parent: Option<ObjectHandle>,
    filename: &str,
) -> Result<ObjectHandle, CliError> {
    let stage_name = staging_folder_name();
    let stage = storage
        .create_folder(parent, &stage_name)
        .await
        .map_err(|e| CliError::from_mtp("create staging folder", e, cli.verbose))?;

    let result = async {
        let copy = storage
            .copy_object(source, stage, None)
            .await
            .map_err(|e| CliError::from_mtp("copy remote object", e, cli.verbose))?;
        storage
            .rename(copy, filename)
            .await
            .map_err(|e| CliError::from_mtp("rename copied object", e, cli.verbose))?;
        storage
            .move_object(copy, parent.unwrap_or(ObjectHandle::ROOT), None)
            .await
            .map_err(|e| CliError::from_mtp("move copied object", e, cli.verbose))?;
        // A move may hand the object a new handle (WPD ids follow the path), so look it up.
        storage
            .list_objects(parent)
            .await
            .map_err(|e| CliError::from_mtp("list destination folder", e, cli.verbose))?
            .into_iter()
            .find(|o| o.filename == filename)
            .map(|o| o.handle)
            .ok_or_else(|| {
                CliError::new(
                    CliErrorKind::RemotePath,
                    format!("the copy was moved but `{filename}` isn't in the destination"),
                )
            })
    }
    .await;

    if let Err(e) = remove_staging_folder(storage, stage).await {
        let leftover = format!(
            "couldn't remove the staging folder `{stage_name}` next to the destination: {e}; \
             remove it by hand"
        );
        return match result {
            Ok(_) => Err(CliError::new(CliErrorKind::RemotePath, leftover)),
            Err(original) => {
                eprintln!("warning: {leftover}");
                Err(original)
            }
        };
    }
    result
}

/// Delete the staging folder's contents (a copy left by a failed step), then the folder itself.
async fn remove_staging_folder(
    storage: &mtp_rs::Storage,
    stage: ObjectHandle,
) -> Result<(), mtp_rs::Error> {
    for leftover in storage.list_objects(Some(stage)).await? {
        storage.delete(leftover.handle).await?;
    }
    storage.delete(stage).await
}

/// A hidden folder name no other `cp` run picks at the same time.
fn staging_folder_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(".mtp-rs-cp-{}-{nanos:09}", std::process::id())
}
