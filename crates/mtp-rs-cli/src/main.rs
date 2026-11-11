mod cli;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    register_test_virtual_device();

    match cli::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {}", err);
            if let Some(help) = err.help() {
                eprintln!("{}", help);
            }
            std::process::ExitCode::from(err.exit_code())
        }
    }
}

#[cfg(all(feature = "virtual-device", debug_assertions))]
fn register_test_virtual_device() {
    let Ok(root) = std::env::var("__MTP_RS_TEST_VIRTUAL_ROOT") else {
        return;
    };
    let serial = std::env::var("__MTP_RS_TEST_VIRTUAL_SERIAL")
        .unwrap_or_else(|_| "mtp-rs-cli-test".to_string());
    let config = mtp_rs::virtual_device::VirtualDeviceConfig {
        manufacturer: "TestCorp".into(),
        model: "CLI Test Device".into(),
        serial,
        storages: vec![mtp_rs::virtual_device::VirtualStorageConfig {
            description: "Internal Storage".into(),
            capacity: 64 * 1024 * 1024,
            backing_dir: std::path::PathBuf::from(root),
            read_only: false,
        }],
        event_poll_interval: std::time::Duration::ZERO,
        watch_backing_dirs: false,
        // Comma-separated storage-relative paths the device refuses to describe,
        // so the CLI's partially-readable-folder output can be tested end to end.
        undescribable_objects: std::env::var("__MTP_RS_TEST_VIRTUAL_UNREADABLE")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        // The device's clock, in minutes east of UTC, and whether its dates carry that offset,
        // so the date handling of `ls`, `get`, and `put` can be tested against a device in
        // another zone than the host's.
        utc_offset: std::env::var("__MTP_RS_TEST_VIRTUAL_UTC_OFFSET_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .and_then(mtp_rs::UtcOffset::from_minutes)
            .unwrap_or(mtp_rs::UtcOffset::UTC),
        dates_include_offset: std::env::var_os("__MTP_RS_TEST_VIRTUAL_DATES_INCLUDE_OFFSET")
            .is_some(),
        ..Default::default()
    };
    let _ = mtp_rs::virtual_device::register_virtual_device(&config);
}

#[cfg(not(all(feature = "virtual-device", debug_assertions)))]
fn register_test_virtual_device() {}
