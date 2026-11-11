#![cfg(feature = "virtual-device")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SERIAL: &str = "cli-process-test";

struct CliFixture {
    _tempdir: tempfile::TempDir,
    backing_dir: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let backing_dir = tempdir.path().join("storage");
        std::fs::create_dir(&backing_dir).unwrap();
        Self {
            _tempdir: tempdir,
            backing_dir,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mtp-rs"));
        command
            .env("__MTP_RS_TEST_VIRTUAL_ROOT", &self.backing_dir)
            .env("__MTP_RS_TEST_VIRTUAL_SERIAL", SERIAL);
        command
    }

    fn run_json(&self, args: &[&str]) -> Value {
        self.run_json_env(&[], args)
    }

    /// Like [`run_json`](Self::run_json), with extra environment variables for the CLI process
    /// (the host's `TZ`, or the virtual device's clock).
    fn run_json_env(&self, env: &[(&str, &str)], args: &[&str]) -> Value {
        let output = self.output_env(env, args);
        assert!(
            output.status.success(),
            "command failed: {args:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "stdout is not valid JSON: {err}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn output(&self, args: &[&str]) -> Output {
        self.output_env(&[], args)
    }

    fn output_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = self.command();
        for (key, value) in env {
            command.env(key, value);
        }
        command.args(args).output().unwrap()
    }
}

#[test]
fn devices_json_lists_virtual_device() {
    let fixture = CliFixture::new();

    let value = fixture.run_json(&["--json", "devices"]);
    let devices = value.as_array().expect("devices output is an array");
    let device = devices
        .iter()
        .find(|device| device["serial_number"] == SERIAL)
        .expect("virtual device is listed");

    assert_eq!(device["manufacturer"], "TestCorp");
    assert_eq!(device["product"], "CLI Test Device");
    assert_eq!(device["match_reason"], "known_vid_pid");
}

#[test]
fn file_lifecycle_through_cli_binary_emits_json() {
    let fixture = CliFixture::new();
    let local = fixture._tempdir.path().join("local.txt");
    let downloaded = fixture._tempdir.path().join("downloaded.txt");
    std::fs::write(&local, b"hello from cli process").unwrap();

    let info = fixture.run_json(&["--json", "--device", SERIAL, "info"]);
    assert_eq!(info["serial_number"], SERIAL);
    assert_eq!(info["storages"][0]["description"], "Internal Storage");

    let mkdir = fixture.run_json(&["--json", "--device", SERIAL, "mkdir", "/Upload"]);
    assert_eq!(mkdir["operation"], "mkdir");
    assert_eq!(mkdir["remote_path"], "/Upload");

    let put = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "put",
        path_str(&local),
        "/Upload/remote.txt",
        "--verify",
    ]);
    assert_eq!(put["operation"], "put");
    assert_eq!(put["remote_path"], "/Upload/remote.txt");
    assert_eq!(put["verified"], true);

    let listing = fixture.run_json(&["--json", "--device", SERIAL, "ls", "/Upload"]);
    let objects = listing["objects"].as_array().expect("objects is an array");
    assert!(objects
        .iter()
        .any(|object| object["filename"] == "remote.txt"));

    let get = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "get",
        "/Upload/remote.txt",
        path_str(&downloaded),
    ]);
    assert_eq!(get["operation"], "get");
    assert_eq!(get["kind"], "file");
    assert_eq!(
        std::fs::read_to_string(&downloaded).unwrap(),
        "hello from cli process"
    );

    let rm = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "rm",
        "/Upload/remote.txt",
        "--yes",
    ]);
    assert_eq!(rm["operation"], "rm");
    assert_eq!(rm["remote_path"], "/Upload/remote.txt");
}

#[test]
fn cp_into_another_folder_makes_a_copy() {
    let fixture = CliFixture::new();
    std::fs::write(fixture.backing_dir.join("a.jpg"), b"photo").unwrap();
    std::fs::create_dir(fixture.backing_dir.join("Archive")).unwrap();

    let row = fixture.run_json(&["--json", "--device", SERIAL, "cp", "/a.jpg", "/Archive/"]);
    assert_eq!(row["operation"], "cp");
    assert_ne!(row["handle"], row["source_handle"]);
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("Archive/a.jpg")).unwrap(),
        b"photo"
    );
}

/// Names left in `dir` that look like `cp`'s staging folder.
fn staging_leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".mtp-rs-cp-"))
        .collect()
}

/// #59: a device copies under the source's name, and Android answers a copy into the source's
/// own folder with OK and makes nothing. So a renamed copy next to the original goes through a
/// staging folder: copy there, rename, move back.
#[test]
fn cp_within_one_folder_makes_a_renamed_copy_through_a_staging_folder() {
    let fixture = CliFixture::new();
    std::fs::create_dir(fixture.backing_dir.join("Pics")).unwrap();
    std::fs::write(fixture.backing_dir.join("Pics/a.jpg"), b"photo").unwrap();

    let row = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "cp",
        "/Pics/a.jpg",
        "/Pics/b.jpg",
    ]);
    assert_eq!(row["renamed"], true);
    assert_eq!(row["filename"], "b.jpg");
    assert_ne!(row["handle"], row["source_handle"]);
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("Pics/a.jpg")).unwrap(),
        b"photo"
    );
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("Pics/b.jpg")).unwrap(),
        b"photo"
    );
    assert_eq!(
        staging_leftovers(&fixture.backing_dir.join("Pics")),
        Vec::<String>::new()
    );
}

#[test]
fn cp_within_the_root_makes_a_renamed_copy() {
    let fixture = CliFixture::new();
    std::fs::write(fixture.backing_dir.join("a.jpg"), b"photo").unwrap();

    fixture.run_json(&["--json", "--device", SERIAL, "cp", "/a.jpg", "/b.jpg"]);
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("b.jpg")).unwrap(),
        b"photo"
    );
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("a.jpg")).unwrap(),
        b"photo"
    );
    assert_eq!(
        staging_leftovers(&fixture.backing_dir),
        Vec::<String>::new()
    );
}

#[test]
fn cp_replace_within_one_folder_replaces_the_destination() {
    let fixture = CliFixture::new();
    std::fs::write(fixture.backing_dir.join("a.jpg"), b"photo").unwrap();
    std::fs::write(
        fixture.backing_dir.join("b.jpg"),
        b"old one, longer than the photo",
    )
    .unwrap();

    fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "cp",
        "/a.jpg",
        "/b.jpg",
        "--replace",
    ]);
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("a.jpg")).unwrap(),
        b"photo"
    );
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("b.jpg")).unwrap(),
        b"photo"
    );
    assert_eq!(
        staging_leftovers(&fixture.backing_dir),
        Vec::<String>::new()
    );
}

#[test]
fn cp_without_replace_keeps_an_existing_destination() {
    let fixture = CliFixture::new();
    std::fs::write(fixture.backing_dir.join("a.jpg"), b"photo").unwrap();
    std::fs::write(fixture.backing_dir.join("b.jpg"), b"keep me").unwrap();

    let output = fixture.output(&["--device", SERIAL, "cp", "/a.jpg", "/b.jpg"]);
    assert!(
        !output.status.success(),
        "cp onto an existing name needs --replace"
    );
    assert_eq!(
        std::fs::read(fixture.backing_dir.join("b.jpg")).unwrap(),
        b"keep me"
    );
    assert_eq!(
        staging_leftovers(&fixture.backing_dir),
        Vec::<String>::new()
    );
}

#[test]
fn get_downloads_a_folder_tree() {
    let fixture = CliFixture::new();
    let dcim = fixture.backing_dir.join("DCIM");
    std::fs::create_dir_all(dcim.join("Camera").join("Burst")).unwrap();
    std::fs::create_dir_all(dcim.join("Empty")).unwrap();
    std::fs::write(dcim.join("index.txt"), b"top level").unwrap();
    std::fs::write(dcim.join("Camera").join("IMG_0001.jpg"), vec![7u8; 200_000]).unwrap();
    std::fs::write(dcim.join("Camera").join("Burst").join("empty.bin"), b"").unwrap();
    let local = fixture._tempdir.path().join("photos");

    let get = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "get",
        "/DCIM",
        path_str(&local),
    ]);
    assert_eq!(get["operation"], "get");
    assert_eq!(get["kind"], "folder");
    assert_eq!(get["files"], 3);
    assert_eq!(get["folders"], 3);
    assert_eq!(get["bytes"], 200_000 + "top level".len());
    assert_eq!(get["skipped"].as_array().unwrap().len(), 0);

    assert_eq!(
        std::fs::read(local.join("index.txt")).unwrap(),
        b"top level"
    );
    assert_eq!(
        std::fs::read(local.join("Camera").join("IMG_0001.jpg")).unwrap(),
        vec![7u8; 200_000]
    );
    assert_eq!(
        std::fs::read(local.join("Camera").join("Burst").join("empty.bin")).unwrap(),
        b""
    );
    assert!(local.join("Empty").is_dir());
    assert_eq!(
        std::fs::read_dir(&local).unwrap().count(),
        3,
        "no temp files should be left behind"
    );
}

#[test]
fn get_folder_shows_listing_progress_before_downloading() {
    let fixture = CliFixture::new();
    let dcim = fixture.backing_dir.join("DCIM");
    std::fs::create_dir_all(dcim.join("Camera")).unwrap();
    std::fs::write(dcim.join("Camera").join("a.jpg"), b"a").unwrap();
    std::fs::write(dcim.join("Camera").join("b.jpg"), b"b").unwrap();
    let local = fixture._tempdir.path().join("photos");

    let output = fixture.output(&["--device", SERIAL, "get", "/DCIM", path_str(&local)]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("listing: 2 files, 1 folder\n"),
        "a big tree lists for minutes on Android, so the walk must say what it's doing\nstderr:\n{stderr}"
    );
}

#[test]
fn get_folder_refuses_an_existing_destination_unless_replace_merges() {
    let fixture = CliFixture::new();
    let music = fixture.backing_dir.join("Music");
    std::fs::create_dir_all(&music).unwrap();
    std::fs::write(music.join("song.mp3"), b"from device").unwrap();
    let local = fixture._tempdir.path().join("Music");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("song.mp3"), b"stale local copy").unwrap();
    std::fs::write(local.join("mine.txt"), b"local only").unwrap();

    let output = fixture.output(&["--device", SERIAL, "get", "/Music", path_str(&local)]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--replace"));
    assert_eq!(
        std::fs::read(local.join("song.mp3")).unwrap(),
        b"stale local copy"
    );

    let get = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "get",
        "/Music",
        path_str(&local),
        "--replace",
    ]);
    assert_eq!(get["files"], 1);
    assert_eq!(
        std::fs::read(local.join("song.mp3")).unwrap(),
        b"from device"
    );
    assert_eq!(
        std::fs::read(local.join("mine.txt")).unwrap(),
        b"local only"
    );
}

#[test]
fn get_folder_refuses_a_local_file_as_the_destination() {
    let fixture = CliFixture::new();
    std::fs::create_dir_all(fixture.backing_dir.join("Music")).unwrap();
    let local = fixture._tempdir.path().join("not-a-dir");
    std::fs::write(&local, b"keep me").unwrap();

    let output = fixture.output(&[
        "--device",
        SERIAL,
        "get",
        "/Music",
        path_str(&local),
        "--replace",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a directory"));
    assert_eq!(std::fs::read(&local).unwrap(), b"keep me");
}

#[test]
fn get_folder_reports_objects_the_device_will_not_describe() {
    let fixture = CliFixture::new();
    let docs = fixture.backing_dir.join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(docs.join(name), b"x").unwrap();
    }
    let local = fixture._tempdir.path().join("Docs");

    let mut command = fixture.command();
    command.env("__MTP_RS_TEST_VIRTUAL_UNREADABLE", "Docs/b.txt");
    let output = command
        .args([
            "--json",
            "--device",
            SERIAL,
            "get",
            "/Docs",
            path_str(&local),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "one unreadable object must not fail the whole download\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["files"], 2);
    let skipped = value["skipped"].as_array().expect("skipped is an array");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["remote_folder"], "/Docs");
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not be read"));
    assert!(local.join("a.txt").is_file() && local.join("c.txt").is_file());
    assert!(!local.join("b.txt").exists());
}

#[test]
fn no_prop_list_lists_folders_per_handle_with_the_same_result() {
    let fixture = CliFixture::new();
    let docs = fixture.backing_dir.join("Docs");
    std::fs::create_dir_all(docs.join("Sub")).unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(docs.join(name), b"x").unwrap();
    }

    // Per-operation trace lines name each operation the CLI sent.
    let ls = |extra: &[&str]| {
        let mut command = fixture.command();
        command.env("RUST_LOG", "mtp_rs=trace");
        let output = command
            .args(["--json", "--device", SERIAL])
            .args(extra)
            .args(["ls", "/Docs"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (value, stderr.contains("GetObjectPropList"))
    };

    let (fast, used_prop_list) = ls(&[]);
    assert!(used_prop_list, "the virtual device advertises it");
    let (slow, used_prop_list) = ls(&["--no-prop-list"]);
    assert!(!used_prop_list);
    assert_eq!(fast, slow);
}

#[test]
fn doctor_probe_cancel_finds_a_file_below_the_storage_root() {
    let fixture = CliFixture::new();
    // An Android MTP root holds only directories, so the probe has to look
    // deeper to find anything to cancel.
    let camera = fixture.backing_dir.join("DCIM").join("Camera");
    std::fs::create_dir_all(&camera).unwrap();
    std::fs::create_dir_all(fixture.backing_dir.join("Download")).unwrap();
    std::fs::write(camera.join("IMG_0001.jpg"), vec![7u8; 200_000]).unwrap();

    let value = fixture.run_json(&["--json", "--device", SERIAL, "doctor", "--probe-cancel"]);
    let probe = &value["cancel_probe"];
    assert_eq!(probe["outcome"], "healthy", "probe was {probe}");
    let detail = probe["detail"].as_str().unwrap();
    assert!(
        detail.contains("/DCIM/Camera/IMG_0001.jpg"),
        "detail should name the file it probed, got: {detail}"
    );
}

#[test]
fn doctor_probe_cancel_falls_back_to_a_tiny_file() {
    let fixture = CliFixture::new();
    // A 36-byte file wedged a Galaxy S23 Ultra, so a small file is worth
    // probing: never skip just because nothing mid-size is around.
    let download = fixture.backing_dir.join("Download");
    std::fs::create_dir_all(&download).unwrap();
    std::fs::write(
        download.join("tiny.txt"),
        b"36 bytes is plenty to wedge a phone.",
    )
    .unwrap();

    let value = fixture.run_json(&["--json", "--device", SERIAL, "doctor", "--probe-cancel"]);
    let probe = &value["cancel_probe"];
    assert_eq!(probe["outcome"], "healthy", "probe was {probe}");
    assert!(probe["detail"]
        .as_str()
        .unwrap()
        .contains("/Download/tiny.txt"));
}

#[test]
fn doctor_probe_path_pins_the_file_and_implies_the_probe() {
    let fixture = CliFixture::new();
    let pictures = fixture.backing_dir.join("Pictures");
    std::fs::create_dir_all(&pictures).unwrap();
    std::fs::write(pictures.join("pinned.bin"), vec![3u8; 4_096]).unwrap();
    std::fs::write(pictures.join("other.bin"), vec![4u8; 400_000]).unwrap();

    let value = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "doctor",
        "--probe-path",
        "/Pictures/pinned.bin",
    ]);
    let probe = &value["cancel_probe"];
    assert_eq!(probe["outcome"], "healthy", "probe was {probe}");
    assert!(probe["detail"]
        .as_str()
        .unwrap()
        .contains("/Pictures/pinned.bin"));
}

#[test]
fn doctor_probe_path_reports_a_missing_file() {
    let fixture = CliFixture::new();
    std::fs::create_dir_all(fixture.backing_dir.join("Download")).unwrap();

    let value = fixture.run_json(&[
        "--json",
        "--device",
        SERIAL,
        "doctor",
        "--probe-path",
        "/Download/nope.bin",
    ]);
    let probe = &value["cancel_probe"];
    assert_eq!(probe["outcome"], "skipped", "probe was {probe}");
    assert!(probe["detail"]
        .as_str()
        .unwrap()
        .contains("/Download/nope.bin"));
}

#[test]
fn doctor_probe_cancel_skip_message_says_what_it_searched() {
    let fixture = CliFixture::new();
    std::fs::create_dir_all(fixture.backing_dir.join("DCIM").join("Camera")).unwrap();

    let value = fixture.run_json(&["--json", "--device", SERIAL, "doctor", "--probe-cancel"]);
    let probe = &value["cancel_probe"];
    assert_eq!(probe["outcome"], "skipped", "probe was {probe}");
    let detail = probe["detail"].as_str().unwrap();
    assert!(
        detail.contains("folder") && detail.contains("--probe-path"),
        "the skip should say what it searched and how to pin a file, got: {detail}"
    );
}

#[test]
fn parser_errors_cross_process_boundary() {
    let fixture = CliFixture::new();

    let output = fixture.output(&["--device", "one", "--location", "0x1", "info"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be used with")
            || String::from_utf8_lossy(&output.stderr).contains("unexpected argument")
    );

    let output = fixture.output(&["--known", "not-a-vid-pid", "devices"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected VID:PID"));
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("test path is valid UTF-8")
}

#[test]
fn ls_reports_objects_the_device_will_not_describe() {
    let fixture = CliFixture::new();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(fixture.backing_dir.join(name), b"x").unwrap();
    }

    // Model Sphaira: the device enumerates b.txt but won't describe it.
    let mut command = fixture.command();
    command.env("__MTP_RS_TEST_VIRTUAL_UNREADABLE", "b.txt");
    let output = command
        .args(["--json", "--device", SERIAL, "ls", "/"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "one unreadable object must not fail the whole listing\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let objects = value["objects"].as_array().expect("objects is an array");
    let names: Vec<&str> = objects
        .iter()
        .map(|o| o["filename"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"a.txt") && names.contains(&"c.txt"));
    assert!(!names.contains(&"b.txt"));

    // The omission is reported, not silent: in JSON for scripts...
    let skipped = value["skipped"].as_array().expect("skipped is an array");
    assert_eq!(skipped.len(), 1, "the unreadable object must be reported");
    assert!(skipped[0]["handle"].is_number());

    // ...and on stderr for humans, leaving stdout clean for pipes.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not be read"),
        "expected a warning on stderr, got:\n{stderr}"
    );
}

#[test]
fn ls_always_reports_a_skipped_field_even_when_nothing_was_skipped() {
    // A consumer should be able to read one field unconditionally rather than
    // having to know the key is sometimes absent.
    let fixture = CliFixture::new();
    std::fs::write(fixture.backing_dir.join("only.txt"), b"x").unwrap();

    let value = fixture.run_json(&["--json", "--device", SERIAL, "ls", "/"]);
    assert_eq!(
        value["skipped"].as_array().expect("skipped is present"),
        &Vec::<Value>::new()
    );
}

// Dates (#69, #70). `JULY_1_10_UTC` is 2026-07-01T10:00:00Z: noon in Stockholm (CEST, UTC+2), so
// a device on UTC+2 writes it as `2026-07-01T12:00:00`.

const JULY_1_10_UTC: u64 = 1_782_900_000;
const DEVICE_ON_UTC_PLUS_2: (&str, &str) = ("__MTP_RS_TEST_VIRTUAL_UTC_OFFSET_MINUTES", "120");
const DEVICE_SENDS_OFFSET: (&str, &str) = ("__MTP_RS_TEST_VIRTUAL_DATES_INCLUDE_OFFSET", "1");

fn at(secs: u64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)
}

/// Sets a fixture's date. Opens for writing on Windows, where a read-only handle can't set times
/// (and folders aren't dated there: the tests that date folders are Unix-only).
fn set_mtime(path: &Path, secs: u64) {
    let file = if path.is_dir() {
        std::fs::File::open(path).unwrap()
    } else {
        std::fs::OpenOptions::new().write(true).open(path).unwrap()
    };
    file.set_modified(at(secs)).unwrap();
}

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

fn find_object<'a>(listing: &'a Value, filename: &str) -> &'a Value {
    listing["objects"]
        .as_array()
        .expect("objects is an array")
        .iter()
        .find(|object| object["filename"] == filename)
        .unwrap_or_else(|| panic!("{filename} is listed: {listing}"))
}

/// A device with one file, `a.txt`, dated [`JULY_1_10_UTC`].
fn fixture_with_dated_file() -> CliFixture {
    let fixture = CliFixture::new();
    let file = fixture.backing_dir.join("a.txt");
    std::fs::write(&file, b"a").unwrap();
    set_mtime(&file, JULY_1_10_UTC);
    fixture
}

#[test]
fn ls_json_shows_dates_with_no_zone_the_device_did_not_send() {
    let fixture = fixture_with_dated_file();

    let listing = fixture.run_json_env(
        &[DEVICE_ON_UTC_PLUS_2],
        &["--json", "--device", SERIAL, "ls", "/"],
    );
    let object = find_object(&listing, "a.txt");
    assert_eq!(object["modified"], "2026-07-01T12:00:00");
    // Always present; whether it's a date depends on the host filesystem keeping birth times.
    let created = object.get("created").expect("created is always present");
    assert!(created.is_null() || created.is_string(), "{created}");
}

#[test]
fn ls_json_shows_the_offset_when_the_device_sends_one() {
    let fixture = fixture_with_dated_file();

    let listing = fixture.run_json_env(
        &[DEVICE_ON_UTC_PLUS_2, DEVICE_SENDS_OFFSET],
        &["--json", "--device", SERIAL, "ls", "/"],
    );
    assert_eq!(
        find_object(&listing, "a.txt")["modified"],
        "2026-07-01T12:00:00+02:00"
    );
}

#[test]
fn ls_long_adds_a_modified_column() {
    let fixture = fixture_with_dated_file();

    let output = fixture.output_env(
        &[DEVICE_ON_UTC_PLUS_2],
        &["--device", SERIAL, "ls", "--long", "/"],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.ends_with(" a.txt"))
        .unwrap_or_else(|| panic!("a.txt is listed:\n{stdout}"));
    assert!(line.contains(" 2026-07-01 12:00:00 "), "{line}");

    let short = fixture.output_env(&[DEVICE_ON_UTC_PLUS_2], &["--device", SERIAL, "ls", "/"]);
    assert!(
        !String::from_utf8_lossy(&short.stdout).contains("2026-07-01"),
        "plain ls stays narrow"
    );
}

#[test]
fn get_reads_a_zoneless_date_in_the_host_zone() {
    let fixture = fixture_with_dated_file();

    // Host and device agree on the zone: the copy gets the original instant.
    let same_zone = fixture._tempdir.path().join("same.txt");
    fixture.run_json_env(
        &[DEVICE_ON_UTC_PLUS_2, ("TZ", "Europe/Stockholm")],
        &[
            "--json",
            "--device",
            SERIAL,
            "get",
            "/a.txt",
            path_str(&same_zone),
        ],
    );
    assert_eq!(mtime(&same_zone), at(JULY_1_10_UTC));

    // The host is on UTC, so it reads the device's `12:00:00` as noon UTC.
    let utc_host = fixture._tempdir.path().join("utc.txt");
    fixture.run_json_env(
        &[DEVICE_ON_UTC_PLUS_2, ("TZ", "UTC")],
        &[
            "--json",
            "--device",
            SERIAL,
            "get",
            "/a.txt",
            path_str(&utc_host),
        ],
    );
    assert_eq!(mtime(&utc_host), at(JULY_1_10_UTC + 2 * 3600));
}

#[test]
fn get_converts_a_date_with_an_offset_exactly() {
    let fixture = fixture_with_dated_file();

    let local = fixture._tempdir.path().join("a.txt");
    fixture.run_json_env(
        &[
            DEVICE_ON_UTC_PLUS_2,
            DEVICE_SENDS_OFFSET,
            ("TZ", "Asia/Kolkata"),
        ],
        &[
            "--json",
            "--device",
            SERIAL,
            "get",
            "/a.txt",
            path_str(&local),
        ],
    );
    assert_eq!(mtime(&local), at(JULY_1_10_UTC));
}

#[test]
fn get_replace_dates_the_new_copy() {
    let fixture = fixture_with_dated_file();
    let local = fixture._tempdir.path().join("a.txt");
    std::fs::write(&local, b"old").unwrap();

    fixture.run_json_env(
        &[("TZ", "UTC")],
        &[
            "--json",
            "--device",
            SERIAL,
            "get",
            "/a.txt",
            path_str(&local),
            "--replace",
        ],
    );
    assert_eq!(std::fs::read(&local).unwrap(), b"a");
    assert_eq!(mtime(&local), at(JULY_1_10_UTC));
}

// Unix-only because the fixture dates folders, which needs a Windows-specific open.
#[cfg(unix)]
#[test]
fn get_folder_keeps_the_dates_of_files_and_folders() {
    let fixture = CliFixture::new();
    let dcim = fixture.backing_dir.join("DCIM");
    let camera = dcim.join("Camera");
    let empty = dcim.join("Empty");
    std::fs::create_dir_all(&camera).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(dcim.join("index.txt"), b"top").unwrap();
    std::fs::write(camera.join("IMG_0001.jpg"), b"photo").unwrap();
    // Files first: writing a file into a folder moves the folder's own date.
    set_mtime(&dcim.join("index.txt"), JULY_1_10_UTC);
    set_mtime(&camera.join("IMG_0001.jpg"), JULY_1_10_UTC + 60);
    set_mtime(&camera, JULY_1_10_UTC + 120);
    set_mtime(&empty, JULY_1_10_UTC + 180);
    set_mtime(&dcim, JULY_1_10_UTC + 240);
    let local = fixture._tempdir.path().join("photos");

    fixture.run_json_env(
        &[("TZ", "UTC")],
        &[
            "--json",
            "--device",
            SERIAL,
            "get",
            "/DCIM",
            path_str(&local),
        ],
    );
    assert_eq!(mtime(&local.join("index.txt")), at(JULY_1_10_UTC));
    assert_eq!(
        mtime(&local.join("Camera").join("IMG_0001.jpg")),
        at(JULY_1_10_UTC + 60)
    );
    assert_eq!(mtime(&local.join("Camera")), at(JULY_1_10_UTC + 120));
    assert_eq!(mtime(&local.join("Empty")), at(JULY_1_10_UTC + 180));
    assert_eq!(mtime(&local), at(JULY_1_10_UTC + 240));
}

#[test]
fn put_sends_the_local_files_date() {
    let fixture = CliFixture::new();
    let local = fixture._tempdir.path().join("report.pdf");
    std::fs::write(&local, b"pdf").unwrap();
    set_mtime(&local, JULY_1_10_UTC);

    // The host's zone doesn't matter: the date goes out as UTC.
    fixture.run_json_env(
        &[DEVICE_ON_UTC_PLUS_2, ("TZ", "America/New_York")],
        &[
            "--json",
            "--device",
            SERIAL,
            "put",
            path_str(&local),
            "/report.pdf",
        ],
    );
    assert_eq!(
        mtime(&fixture.backing_dir.join("report.pdf")),
        at(JULY_1_10_UTC)
    );
    let listing = fixture.run_json_env(
        &[DEVICE_ON_UTC_PLUS_2],
        &["--json", "--device", SERIAL, "ls", "/"],
    );
    assert_eq!(
        find_object(&listing, "report.pdf")["modified"],
        "2026-07-01T12:00:00"
    );
}
