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
        let output = self.output(args);
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
        self.command().args(args).output().unwrap()
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
