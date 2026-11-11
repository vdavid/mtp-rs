use bytes::Bytes;
use futures::Stream;
use mtp_rs::mtp::Storage;
use mtp_rs::{NewObjectInfo, ObjectHandle};
use serde::Serialize;
use std::ops::ControlFlow;
use std::path::Path;
use std::pin::Pin;
use tokio::io::AsyncReadExt;

use crate::cli::args::{Cli, PutArgs};
use crate::cli::dates;
use crate::cli::device::open_storage;
use crate::cli::error::{CliError, CliErrorKind};
use crate::cli::output::{finish_progress, print_json, print_progress};
use crate::cli::path::{self, RemotePath};

const CHUNK_SIZE: usize = 256 * 1024;

#[derive(Debug, Serialize)]
struct PutRow {
    operation: &'static str,
    local_path: String,
    remote_path: String,
    filename: String,
    handle: u64,
    bytes: u64,
    replaced: bool,
    verified: bool,
}

pub async fn run(cli: &Cli, args: &PutArgs) -> Result<(), CliError> {
    let metadata = tokio::fs::metadata(&args.local_path)
        .await
        .map_err(|e| CliError::new(CliErrorKind::Other, format!("read local file: {e}")))?;
    if !metadata.is_file() {
        return Err(CliError::new(
            CliErrorKind::Other,
            "local path is not a regular file",
        ));
    }

    let local_filename = args
        .local_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliError::new(CliErrorKind::Other, "local file has no valid filename"))?;
    path::validate_component(local_filename)?;

    let (_device, storage) = open_storage(cli, true).await?;
    let remote_path = RemotePath::parse(&args.remote_path)?;
    let target =
        path::resolve_upload_target(&storage, &remote_path, local_filename, cli.verbose).await?;

    let replaced = target.existing.is_some();
    if let Some(existing) = &target.existing {
        if !args.replace {
            return Err(CliError::new(
                CliErrorKind::RemotePath,
                "remote file already exists; pass --replace to delete it first",
            ));
        }
        storage
            .delete(existing.handle)
            .await
            .map_err(|e| CliError::from_mtp("delete existing remote file", e, cli.verbose))?;
    }

    let file = tokio::fs::File::open(&args.local_path)
        .await
        .map_err(|e| CliError::new(CliErrorKind::Other, format!("open local file: {e}")))?;
    let total_size = metadata.len();
    let stream = file_stream(file);
    let mut info = NewObjectInfo::file(target.filename.clone(), total_size);
    // Sent as UTC, the one form Android reads as the right instant whatever the phone's zone
    // (see `dates::system_time_to_device_date`). Whether the date sticks is the device's call:
    // Samsung ignores it, for one (#68). A filesystem with no mtime sends none.
    if let Some(modified) = metadata
        .modified()
        .ok()
        .and_then(dates::system_time_to_device_date)
    {
        info = info.with_modified(modified);
    }
    let mut last_percent = 101u64;
    let handle = match storage
        .upload_with_progress(target.parent, info, stream, move |progress| {
            print_progress(
                "upload",
                progress.bytes_transferred,
                total_size,
                &mut last_percent,
            );
            ControlFlow::Continue(())
        })
        .await
    {
        Ok(handle) => handle,
        Err(upload_err) => {
            finish_progress();
            // `put` is a one-shot transfer with no resume story, so clean up the
            // partial object the device may still hold rather than leaving junk.
            // Best-effort: if the delete fails (device gone), surface the original
            // upload error anyway.
            if let Some(partial) = upload_err.partial {
                if let Err(cleanup_err) = storage.delete(partial).await {
                    if cli.verbose {
                        eprintln!("warning: failed to delete partial upload: {cleanup_err}");
                    }
                }
            }
            return Err(CliError::from_mtp(
                "upload file",
                upload_err.source,
                cli.verbose,
            ));
        }
    };
    finish_progress();
    let mut verified = false;

    if args.verify {
        verify_remote_matches_local(&storage, handle, &args.local_path, total_size, cli.verbose)
            .await?;
        verified = true;
    }

    let row = PutRow {
        operation: "put",
        local_path: args.local_path.display().to_string(),
        remote_path: remote_path.raw().to_string(),
        filename: target.filename,
        handle: handle.0,
        bytes: total_size,
        replaced,
        verified,
    };

    if cli.json {
        return print_json(&row);
    }

    println!(
        "uploaded {} ({} bytes) handle={}",
        row.filename, row.bytes, row.handle
    );
    if row.verified {
        println!("verified {}", row.filename);
    }
    Ok(())
}

async fn verify_remote_matches_local(
    storage: &Storage,
    handle: ObjectHandle,
    local_path: &Path,
    total_size: u64,
    verbose: bool,
) -> Result<(), CliError> {
    let mut remote = storage
        .download(handle, mtp_rs::ByteRange::Full)
        .await
        .map_err(|e| CliError::from_mtp("verify download", e, verbose))?;
    let mut local = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| CliError::new(CliErrorKind::Verify, format!("verify local file: {e}")))?;
    let mut compared = 0u64;

    while let Some(chunk) = remote.next_chunk().await {
        let bytes = chunk.map_err(|e| CliError::from_mtp("verify download", e, verbose))?;
        let mut local_bytes = vec![0; bytes.len()];
        local
            .read_exact(&mut local_bytes)
            .await
            .map_err(|e| CliError::new(CliErrorKind::Verify, format!("verify local file: {e}")))?;
        if bytes.as_ref() != local_bytes.as_slice() {
            return Err(CliError::new(
                CliErrorKind::Verify,
                "verification failed: uploaded bytes differ from local file",
            ));
        }
        compared += bytes.len() as u64;
    }

    if compared != total_size {
        return Err(CliError::new(
            CliErrorKind::Verify,
            "verification failed: uploaded size differs from local file",
        ));
    }

    Ok(())
}

fn file_stream(
    file: tokio::fs::File,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
    Box::pin(futures::stream::unfold(file, |mut file| async move {
        let mut buf = vec![0; CHUNK_SIZE];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(Bytes::from(buf)), file))
            }
            Err(e) => Some((Err(e), file)),
        }
    }))
}
