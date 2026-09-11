use mtp_rs::mtp::Storage;
use mtp_rs::{ObjectHandle, ObjectInfo};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

use crate::cli::args::{Cli, GetArgs};
use crate::cli::device::open_storage;
use crate::cli::error::{CliError, CliErrorKind};
use crate::cli::output::{finish_progress, print_json, print_progress};
use crate::cli::path::{self, ExistingRemote, RemotePath};

#[derive(Debug, Serialize)]
struct GetRow {
    operation: &'static str,
    kind: &'static str,
    remote_path: String,
    local_path: String,
    filename: String,
    handle: u64,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct GetFolderRow {
    operation: &'static str,
    kind: &'static str,
    remote_path: String,
    local_path: String,
    /// `None` for the storage root, which has no handle.
    handle: Option<u64>,
    files: usize,
    folders: usize,
    bytes: u64,
    /// Objects the device listed but wouldn't describe, so they weren't downloaded.
    /// Always present (empty when everything was readable), like `ls`.
    skipped: Vec<SkippedRow>,
}

/// One object the device listed but refused to describe.
#[derive(Debug, Serialize)]
struct SkippedRow {
    remote_folder: String,
    handle: u64,
    error: String,
}

/// Everything a folder download will write, gathered before writing any of it.
#[derive(Debug, Default)]
struct FolderPlan {
    /// Local folders below the destination root, parents before children.
    folders: Vec<PathBuf>,
    files: Vec<PlannedFile>,
    skipped: Vec<SkippedRow>,
}

#[derive(Debug)]
struct PlannedFile {
    handle: ObjectHandle,
    remote_path: String,
    local_path: PathBuf,
}

pub async fn run(cli: &Cli, args: &GetArgs) -> Result<(), CliError> {
    // Checked before opening the device, so a refusal costs no USB traffic.
    let destination_exists = tokio::fs::try_exists(&args.local_path)
        .await
        .map_err(|e| CliError::new(CliErrorKind::Other, format!("check local path: {e}")))?;
    if destination_exists && !args.replace {
        return Err(CliError::new(
            CliErrorKind::Other,
            "local path already exists; pass --replace to overwrite it",
        ));
    }

    let (_device, storage) = open_storage(cli, false).await?;
    let path = RemotePath::parse(&args.remote_path)?;
    match path::resolve_existing(&storage, &path, cli.verbose).await? {
        ExistingRemote::Root => run_directory(cli, args, &storage, &path, None).await,
        ExistingRemote::Object(object) if object.is_folder() => {
            run_directory(cli, args, &storage, &path, Some(object)).await
        }
        ExistingRemote::Object(object) => run_file(cli, args, &storage, &path, object).await,
    }
}

async fn run_file(
    cli: &Cli,
    args: &GetArgs,
    storage: &Storage,
    path: &RemotePath,
    object: ObjectInfo,
) -> Result<(), CliError> {
    let bytes = download_file(
        storage,
        object.handle,
        path.raw(),
        &args.local_path,
        args.replace,
        "download",
        cli.verbose,
    )
    .await?;

    let row = GetRow {
        operation: "get",
        kind: "file",
        remote_path: path.raw().to_string(),
        local_path: args.local_path.display().to_string(),
        filename: object.filename,
        handle: object.handle.0,
        bytes,
    };

    if cli.json {
        return print_json(&row);
    }

    println!("downloaded {} ({} bytes)", row.local_path, row.bytes);
    Ok(())
}

/// Downloads a remote folder (`None` is the storage root) and everything below it into
/// `args.local_path`, which becomes the copy of that folder. With `--replace`, an existing local
/// folder is merged into: files the device has are replaced, anything else is left alone.
async fn run_directory(
    cli: &Cli,
    args: &GetArgs,
    storage: &Storage,
    path: &RemotePath,
    folder: Option<ObjectInfo>,
) -> Result<(), CliError> {
    if tokio::fs::metadata(&args.local_path)
        .await
        .is_ok_and(|metadata| !metadata.is_dir())
    {
        return Err(CliError::new(
            CliErrorKind::Other,
            "remote path is a folder but the local path is not a directory",
        ));
    }

    let remote_root = if path.is_root() {
        "/".to_string()
    } else {
        format!("/{}", path.components().join("/"))
    };
    let plan = plan_folder(
        storage,
        folder.as_ref().map(|folder| folder.handle),
        remote_root,
        &args.local_path,
        cli.verbose,
    )
    .await?;

    // Stderr in both modes, as in `ls`: a human running `--json` still learns the copy is
    // incomplete, and scripts read the `skipped` field.
    if !plan.skipped.is_empty() {
        eprintln!(
            "warning: {} could not be read and {} not downloaded:",
            count(plan.skipped.len(), "object"),
            if plan.skipped.len() == 1 {
                "was"
            } else {
                "were"
            },
        );
        for skipped in &plan.skipped {
            eprintln!(
                "  {} handle={} {}",
                skipped.remote_folder, skipped.handle, skipped.error
            );
        }
    }

    for local_folder in std::iter::once(&args.local_path).chain(&plan.folders) {
        tokio::fs::create_dir_all(local_folder).await.map_err(|e| {
            CliError::new(
                CliErrorKind::Other,
                format!("create local folder {}: {e}", local_folder.display()),
            )
        })?;
    }

    let mut bytes = 0;
    for file in &plan.files {
        bytes += download_file(
            storage,
            file.handle,
            &file.remote_path,
            &file.local_path,
            args.replace,
            &file.remote_path,
            cli.verbose,
        )
        .await?;
    }

    let row = GetFolderRow {
        operation: "get",
        kind: "folder",
        remote_path: path.raw().to_string(),
        local_path: args.local_path.display().to_string(),
        handle: folder.map(|folder| folder.handle.0),
        files: plan.files.len(),
        folders: plan.folders.len(),
        bytes,
        skipped: plan.skipped,
    };

    if cli.json {
        return print_json(&row);
    }

    println!(
        "downloaded {} ({}, {}, {} bytes)",
        row.local_path,
        count(row.files, "file"),
        count(row.folders, "folder"),
        row.bytes
    );
    Ok(())
}

/// Walks the whole remote tree before anything is written, so a name that can't be used locally
/// fails the command up front instead of after half the folder has landed on disk.
async fn plan_folder(
    storage: &Storage,
    root: Option<ObjectHandle>,
    remote_root: String,
    local_root: &Path,
    verbose: bool,
) -> Result<FolderPlan, CliError> {
    let mut plan = FolderPlan::default();
    let mut to_visit = vec![(root, remote_root, local_root.to_path_buf())];

    while let Some((parent, remote_folder, local_folder)) = to_visit.pop() {
        // Collect rather than list: an unreadable entry should be reported, not quietly missing
        // from the copy.
        let collection = storage
            .collect_objects(parent)
            .await
            .map_err(|e| CliError::from_mtp(&format!("list {remote_folder}"), e, verbose))?;
        plan.skipped
            .extend(collection.skipped.into_iter().map(|skipped| SkippedRow {
                remote_folder: remote_folder.clone(),
                handle: skipped.handle.0,
                error: skipped.error.to_string(),
            }));

        for object in collection.objects {
            // The name comes from the device and becomes a local path component, so a `..` or a
            // separator in it would write outside the destination.
            path::validate_component(&object.filename).map_err(|_| {
                CliError::new(
                    CliErrorKind::RemotePath,
                    format!(
                        "remote object in {remote_folder} has a name that can't be used locally: {:?}",
                        object.filename
                    ),
                )
            })?;
            let remote_path = if remote_folder == "/" {
                format!("/{}", object.filename)
            } else {
                format!("{remote_folder}/{}", object.filename)
            };
            let local_path = local_folder.join(&object.filename);

            if object.is_folder() {
                plan.folders.push(local_path.clone());
                to_visit.push((Some(object.handle), remote_path, local_path));
            } else {
                plan.files.push(PlannedFile {
                    handle: object.handle,
                    remote_path,
                    local_path,
                });
            }
        }
    }

    Ok(plan)
}

/// Streams one remote file to `local_path` through a temporary sibling, so an interrupted
/// transfer never destroys a file that was already there. Returns the bytes written.
async fn download_file(
    storage: &Storage,
    handle: ObjectHandle,
    remote_path: &str,
    local_path: &Path,
    replace: bool,
    progress_label: &str,
    verbose: bool,
) -> Result<u64, CliError> {
    let destination_exists = match tokio::fs::metadata(local_path).await {
        Ok(metadata) if metadata.is_dir() => {
            return Err(CliError::new(
                CliErrorKind::Other,
                format!("local path is a directory: {}", local_path.display()),
            ));
        }
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(CliError::new(
                CliErrorKind::Other,
                format!("check local path {}: {e}", local_path.display()),
            ));
        }
    };
    if destination_exists && !replace {
        return Err(CliError::new(
            CliErrorKind::Other,
            format!(
                "local file already exists; pass --replace to overwrite it: {}",
                local_path.display()
            ),
        ));
    }

    // Create the local file before starting the download: failing after it started would
    // abandon a transfer the device is still sending.
    let temp_path = temp_download_path(local_path);
    let mut out = tokio::fs::File::create(&temp_path).await.map_err(|e| {
        CliError::new(
            CliErrorKind::Other,
            format!("create local file {}: {e}", temp_path.display()),
        )
    })?;
    let mut last_percent = 101u64;

    let download_result = async {
        let mut download = storage
            .download(handle, mtp_rs::ByteRange::Full)
            .await
            .map_err(|e| {
                CliError::from_mtp(&format!("start download of {remote_path}"), e, verbose)
            })?;
        while let Some(chunk) = download.next_chunk().await {
            let bytes = chunk
                .map_err(|e| CliError::from_mtp(&format!("download {remote_path}"), e, verbose))?;
            out.write_all(&bytes).await.map_err(|e| {
                CliError::new(CliErrorKind::Transfer, format!("write local file: {e}"))
            })?;
            print_progress(
                progress_label,
                download.bytes_received(),
                download.size(),
                &mut last_percent,
            );
        }
        // An empty file yields no chunks; report it anyway so every file gets a progress line.
        print_progress(
            progress_label,
            download.bytes_received(),
            download.size(),
            &mut last_percent,
        );
        out.flush()
            .await
            .map_err(|e| CliError::new(CliErrorKind::Transfer, format!("flush local file: {e}")))?;
        Ok::<u64, CliError>(download.bytes_received())
    }
    .await;
    drop(out);

    let bytes = match download_result {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            finish_progress();
            return Err(err);
        }
    };

    if destination_exists {
        tokio::fs::remove_file(local_path)
            .await
            .map_err(|e| CliError::new(CliErrorKind::Other, format!("replace local file: {e}")))?;
    }
    tokio::fs::rename(&temp_path, local_path)
        .await
        .map_err(|e| CliError::new(CliErrorKind::Other, format!("replace local file: {e}")))?;
    finish_progress();
    Ok(bytes)
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn temp_download_path(destination: &Path) -> PathBuf {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{name}.mtp-rs-{nonce}-{}.tmp", std::process::id()))
}
