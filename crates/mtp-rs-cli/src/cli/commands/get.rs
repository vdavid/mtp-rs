use mtp_rs::mtp::Storage;
use mtp_rs::{ObjectHandle, ObjectInfo};
use serde::Serialize;
use std::collections::HashMap;
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
    /// Folders below the destination root, parents before children.
    folders: Vec<PlannedFolder>,
    files: Vec<PlannedFile>,
    skipped: Vec<SkippedRow>,
}

impl FolderPlan {
    /// Every planned folder and file as `(remote path, local path)`.
    fn entries(&self) -> impl Iterator<Item = (&str, &Path)> {
        let folders = self
            .folders
            .iter()
            .map(|folder| (folder.remote_path.as_str(), folder.local_path.as_path()));
        let files = self
            .files
            .iter()
            .map(|file| (file.remote_path.as_str(), file.local_path.as_path()));
        folders.chain(files)
    }
}

#[derive(Debug)]
struct PlannedFolder {
    remote_path: String,
    local_path: PathBuf,
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
    let root_existed = match tokio::fs::metadata(&args.local_path).await {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(CliError::new(
                CliErrorKind::Other,
                "remote path is a folder but the local path is not a directory",
            ));
        }
        Ok(_) => true,
        Err(_) => false,
    };

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

    // The collision check asks the destination's own filesystem, so the root folder has to exist
    // for it. Nothing else gets written until the plan is known to fit.
    create_local_folder(&args.local_path).await?;
    if let Err(err) = check_no_collisions(&plan, &args.local_path).await {
        if !root_existed {
            // Still empty: the case probe removes its own file.
            let _ = tokio::fs::remove_dir(&args.local_path).await;
        }
        return Err(err);
    }

    for local_folder in &plan.folders {
        create_local_folder(&local_folder.local_path).await?;
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
    let walked = walk_folder(storage, root, remote_root, local_root, verbose, &mut plan).await;
    // Ends the progress line either way, so an error starts on a line of its own.
    finish_progress();
    walked.map(|()| plan)
}

async fn walk_folder(
    storage: &Storage,
    root: Option<ObjectHandle>,
    remote_root: String,
    local_root: &Path,
    verbose: bool,
    plan: &mut FolderPlan,
) -> Result<(), CliError> {
    let mut to_visit = vec![(root, remote_root, local_root.to_path_buf())];
    // Listing costs a USB round trip per object (~15 s per 1,000 on Android), so a camera roll
    // would otherwise sit silent for minutes before the first download starts.
    print_listing_progress(plan);

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
            if let Some(problem) = local_name_problem(&object.filename, cfg!(windows)) {
                return Err(CliError::new(
                    CliErrorKind::RemotePath,
                    format!(
                        "remote object in {remote_folder} has a name that can't be used locally ({problem}): {:?}",
                        object.filename
                    ),
                ));
            }
            let remote_path = if remote_folder == "/" {
                format!("/{}", object.filename)
            } else {
                format!("{remote_folder}/{}", object.filename)
            };
            let local_path = local_folder.join(&object.filename);

            if object.is_folder() {
                plan.folders.push(PlannedFolder {
                    remote_path: remote_path.clone(),
                    local_path: local_path.clone(),
                });
                to_visit.push((Some(object.handle), remote_path, local_path));
            } else {
                plan.files.push(PlannedFile {
                    handle: object.handle,
                    remote_path,
                    local_path,
                });
            }
        }
        print_listing_progress(plan);
    }

    Ok(())
}

fn print_listing_progress(plan: &FolderPlan) {
    eprint!(
        "\rlisting: {}, {}",
        count(plan.files.len(), "file"),
        count(plan.folders.len(), "folder")
    );
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

/// Refuses a plan where two entries would land on the same local path, which would otherwise
/// fail halfway through (or, with `--replace`, silently keep only one of them).
async fn check_no_collisions(plan: &FolderPlan, local_root: &Path) -> Result<(), CliError> {
    let case_insensitive = is_case_insensitive(local_root).await.map_err(|e| {
        CliError::new(
            CliErrorKind::Other,
            format!("check local folder {}: {e}", local_root.display()),
        )
    })?;
    match find_collision(plan.entries(), case_insensitive) {
        Some((first, second)) => Err(CliError::new(
            CliErrorKind::RemotePath,
            format!(
                "{first} and {second} would be saved to the same local path; nothing was downloaded"
            ),
        )),
        None => Ok(()),
    }
}

async fn create_local_folder(path: &Path) -> Result<(), CliError> {
    tokio::fs::create_dir_all(path).await.map_err(|e| {
        CliError::new(
            CliErrorKind::Other,
            format!("create local folder {}: {e}", path.display()),
        )
    })
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

/// Why a device-supplied name can't be a file or folder name on this machine, or `None` if it can.
/// `windows` is a parameter rather than a `cfg` so both rule sets are testable on any host.
fn local_name_problem(name: &str, windows: bool) -> Option<&'static str> {
    // A separator or `..` would write outside the destination, and a null byte cuts the path short.
    if name.is_empty() {
        return Some("empty");
    }
    if name == "." || name == ".." {
        return Some("`.` and `..` aren't names");
    }
    if name.contains(['/', '\\']) {
        return Some("contains a path separator");
    }
    if name.contains('\0') {
        return Some("contains a null byte");
    }
    if !windows {
        return None;
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || u32::from(c) < 0x20)
    {
        return Some("contains a character Windows doesn't allow");
    }
    if name.ends_with(['.', ' ']) {
        return Some("ends with a dot or space, which Windows drops");
    }
    // Windows reserves these with any extension too: `con.txt` is the console.
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let port = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || port.is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
        || matches!(port, Some("¹" | "²" | "³"))
    {
        return Some("a reserved device name on Windows");
    }
    None
}

/// The remote paths of the first two entries that would land on the same local path.
fn find_collision<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a Path)>,
    case_insensitive: bool,
) -> Option<(&'a str, &'a str)> {
    let mut seen: HashMap<(Option<&Path>, String), &str> = HashMap::new();
    for (remote_path, local_path) in entries {
        let name = local_path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        let name = if case_insensitive {
            name.to_lowercase()
        } else {
            name.into_owned()
        };
        if let Some(first) = seen.insert((local_path.parent(), name), remote_path) {
            return Some((first, remote_path));
        }
    }
    None
}

/// Whether names that differ only in case reach the same file inside `dir`, which must exist.
/// Asks the filesystem rather than going by OS: Linux is case-sensitive, but the FAT and exFAT
/// cards and sticks that photos often get copied to aren't, and a macOS volume can be either.
async fn is_case_insensitive(dir: &Path) -> std::io::Result<bool> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let probe = format!(".mtp-rs-case-probe-{nonce}-{}", std::process::id());
    let lower = dir.join(&probe);
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lower)
        .await?;
    let upper_exists = tokio::fs::try_exists(dir.join(probe.to_uppercase())).await;
    tokio::fs::remove_file(&lower).await?;
    upper_exists
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_are_paths_are_refused_everywhere() {
        for windows in [false, true] {
            for name in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
                assert!(
                    local_name_problem(name, windows).is_some(),
                    "{name:?} should be refused (windows: {windows})"
                );
            }
        }
    }

    #[test]
    fn names_windows_cannot_store_are_refused_only_there() {
        for name in [
            "a:b",
            "what?",
            "star*",
            "pipe|",
            "quote\"",
            "<tag>",
            "tab\tname",
            "trailing.",
            "trailing ",
            "CON",
            "con.txt",
            "LPT1",
            "COM¹.log",
            "nul",
        ] {
            assert!(
                local_name_problem(name, true).is_some(),
                "{name:?} should be refused on Windows"
            );
            assert_eq!(
                local_name_problem(name, false),
                None,
                "{name:?} is fine elsewhere"
            );
        }
    }

    #[test]
    fn ordinary_names_pass_everywhere() {
        for windows in [false, true] {
            for name in [
                "IMG_0001.jpg",
                ".hidden",
                "Screenshot 2026-09-12 at 23.20.png",
                "résumé.pdf",
                "CONSOLE.txt",
                "com10",
            ] {
                assert_eq!(
                    local_name_problem(name, windows),
                    None,
                    "{name:?} (windows: {windows})"
                );
            }
        }
    }

    #[test]
    fn names_differing_only_in_case_collide_only_when_case_is_ignored() {
        let entries = [
            ("/DCIM/IMG.jpg", Path::new("out/IMG.jpg")),
            ("/DCIM/img.JPG", Path::new("out/img.JPG")),
        ];
        assert_eq!(find_collision(entries, false), None);
        assert_eq!(
            find_collision(entries, true),
            Some(("/DCIM/IMG.jpg", "/DCIM/img.JPG"))
        );
    }

    #[test]
    fn a_repeated_name_collides_even_when_case_matters() {
        let entries = [
            ("/DCIM/a.jpg", Path::new("out/a.jpg")),
            ("/DCIM/a.jpg", Path::new("out/a.jpg")),
        ];
        assert_eq!(
            find_collision(entries, false),
            Some(("/DCIM/a.jpg", "/DCIM/a.jpg"))
        );
    }

    #[test]
    fn the_same_name_in_different_folders_does_not_collide() {
        let entries = [
            ("/A/x.jpg", Path::new("out/A/x.jpg")),
            ("/B/X.jpg", Path::new("out/B/X.jpg")),
        ];
        assert_eq!(find_collision(entries, true), None);
    }

    #[tokio::test]
    async fn case_probe_matches_the_platform_default_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let insensitive = is_case_insensitive(dir.path()).await.unwrap();
        // Holds for the default filesystems of each OS (APFS, NTFS, ext4), which is what CI runs.
        assert_eq!(insensitive, cfg!(any(target_os = "macos", windows)));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
