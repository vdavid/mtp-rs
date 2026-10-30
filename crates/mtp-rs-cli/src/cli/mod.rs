//! CLI entry point and subcommand dispatch.
//!
//! Each subcommand lives in its own file under `commands/`. Shared concerns
//! are split across:
//! - `args`: clap definitions
//! - `error`: `CliError` and exit-code mapping
//! - `path`: remote-path parsing and resolution
//! - `device`: opening a device and selecting a storage
//! - `helpers`: cross-command utilities (folder lookups, rename guards, etc.)
//! - `output`: JSON / progress writers and the row structs reused across
//!   subcommands.

mod args;
mod commands;
mod device;
mod error;
mod helpers;
mod output;
mod path;

use args::{Cli, Command};
use clap::Parser;

pub use error::CliError;

pub async fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    init_diagnostics(cli.trace);

    match &cli.command {
        Command::Devices => commands::devices::run(&cli),
        Command::Info => commands::info::run(&cli).await,
        Command::Ls(args) => commands::ls::run(&cli, args).await,
        Command::Put(args) => commands::put::run(&cli, args).await,
        Command::Get(args) => commands::get::run(&cli, args).await,
        Command::Mkdir(args) => commands::mkdir::run(&cli, args).await,
        Command::Rm(args) => commands::rm::run(&cli, args).await,
        Command::Rename(args) => commands::rename::run(&cli, args).await,
        Command::Mv(args) => commands::mv::run(&cli, args).await,
        Command::Cp(args) => commands::cp::run(&cli, args).await,
        Command::Doctor(args) => commands::doctor::run(&cli, args).await,
        Command::Reset => commands::reset::run(&cli).await,
    }
}

/// Install a `tracing` subscriber that renders the library's device-protocol
/// diagnostics to stderr, for bug reports (issue #18).
///
/// Filter precedence: a set `RUST_LOG` wins (full control); otherwise `--trace`
/// selects `mtp_rs=debug`; otherwise nothing is emitted. Writing to stderr keeps
/// stdout clean for `--json` and piped output. Called once at startup; a second
/// call is a harmless no-op (the global subscriber is already set).
fn init_diagnostics(trace_flag: bool) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = match EnvFilter::try_from_default_env() {
        Ok(from_env) => from_env,
        Err(_) if trace_flag => EnvFilter::new("mtp_rs=debug"),
        Err(_) => return, // no RUST_LOG and no --trace: stay quiet
    };

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::args::Cli;
    // The arg-struct types + PathBuf are only referenced by the virtual-device
    // test fixtures below; gate the imports so a `--no-default-features` test
    // build doesn't warn on them as unused.
    #[cfg(feature = "virtual-device")]
    use super::args::{
        Command, CopyArgs, GetArgs, LsArgs, MoveArgs, PutArgs, RemotePathArg, RenameArgs, RmArgs,
    };
    use super::*;
    use clap::CommandFactory;
    #[cfg(feature = "virtual-device")]
    use std::path::PathBuf;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[cfg(feature = "virtual-device")]
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[cfg(feature = "virtual-device")]
    struct VirtualCliFixture {
        _tempdir: tempfile::TempDir,
        serial: String,
        location_id: u64,
    }

    #[cfg(feature = "virtual-device")]
    impl Drop for VirtualCliFixture {
        fn drop(&mut self) {
            mtp_rs::virtual_device::unregister_virtual_device(self.location_id);
        }
    }

    #[cfg(feature = "virtual-device")]
    fn virtual_cli_fixture() -> VirtualCliFixture {
        let tempdir = tempfile::tempdir().unwrap();
        let backing_dir = tempdir.path().join("storage");
        std::fs::create_dir(&backing_dir).unwrap();
        let serial = format!(
            "cli-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let config = mtp_rs::virtual_device::VirtualDeviceConfig {
            manufacturer: "TestCorp".into(),
            model: "CLI Device".into(),
            serial: serial.clone(),
            storages: vec![mtp_rs::virtual_device::VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 64 * 1024 * 1024,
                backing_dir,
                read_only: false,
            }],
            event_poll_interval: Duration::ZERO,
            watch_backing_dirs: false,
            ..Default::default()
        };
        let info = mtp_rs::virtual_device::register_virtual_device(&config);

        VirtualCliFixture {
            _tempdir: tempdir,
            serial,
            location_id: info.location_id,
        }
    }

    #[cfg(feature = "virtual-device")]
    fn test_cli(serial: &str, json: bool) -> Cli {
        Cli {
            device: Some(serial.to_string()),
            location: None,
            storage: None,
            known: Vec::new(),
            timeout: 30,
            no_prop_list: false,
            json,
            verbose: false,
            trace: false,
            command: Command::Doctor(super::args::DoctorArgs {
                probe_cancel: false,
                probe_path: None,
            }),
        }
    }

    #[cfg(feature = "virtual-device")]
    #[tokio::test]
    async fn virtual_device_cli_file_lifecycle() {
        let fixture = virtual_cli_fixture();
        let cli = test_cli(&fixture.serial, true);
        let local = fixture._tempdir.path().join("local.txt");
        let downloaded = fixture._tempdir.path().join("downloaded.txt");
        let copied = fixture._tempdir.path().join("copied.txt");
        let moved = fixture._tempdir.path().join("moved.txt");

        commands::devices::run(&cli).unwrap();
        commands::info::run(&cli).await.unwrap();
        commands::ls::run(
            &cli,
            &LsArgs {
                remote_path: "/".to_string(),
                recursive: false,
            },
        )
        .await
        .unwrap();
        commands::mkdir::run(
            &cli,
            &RemotePathArg {
                remote_path: "/Upload".to_string(),
            },
        )
        .await
        .unwrap();

        tokio::fs::write(&local, b"hello virtual cli")
            .await
            .unwrap();
        commands::put::run(
            &cli,
            &PutArgs {
                local_path: local.clone(),
                remote_path: "/Upload/remote.txt".to_string(),
                replace: false,
                verify: true,
            },
        )
        .await
        .unwrap();
        commands::ls::run(
            &cli,
            &LsArgs {
                remote_path: "/Upload".to_string(),
                recursive: false,
            },
        )
        .await
        .unwrap();
        commands::get::run(
            &cli,
            &GetArgs {
                remote_path: "/Upload/remote.txt".to_string(),
                local_path: downloaded.clone(),
                replace: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&downloaded).await.unwrap(),
            "hello virtual cli"
        );

        tokio::fs::write(&local, b"updated virtual cli")
            .await
            .unwrap();
        commands::put::run(
            &cli,
            &PutArgs {
                local_path: local.clone(),
                remote_path: "/Upload/remote.txt".to_string(),
                replace: true,
                verify: true,
            },
        )
        .await
        .unwrap();
        commands::get::run(
            &cli,
            &GetArgs {
                remote_path: "/Upload/remote.txt".to_string(),
                local_path: downloaded.clone(),
                replace: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&downloaded).await.unwrap(),
            "updated virtual cli"
        );

        commands::rename::run(
            &cli,
            &RenameArgs {
                remote_path: "/Upload/remote.txt".to_string(),
                new_name: "renamed.txt".to_string(),
            },
        )
        .await
        .unwrap();
        commands::mkdir::run(
            &cli,
            &RemotePathArg {
                remote_path: "/Archive".to_string(),
            },
        )
        .await
        .unwrap();
        commands::cp::run(
            &cli,
            &CopyArgs {
                source_path: "/Upload/renamed.txt".to_string(),
                destination_path: "/Archive/copied.txt".to_string(),
                replace: false,
            },
        )
        .await
        .unwrap();
        commands::get::run(
            &cli,
            &GetArgs {
                remote_path: "/Archive/copied.txt".to_string(),
                local_path: copied.clone(),
                replace: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&copied).await.unwrap(),
            "updated virtual cli"
        );

        commands::mv::run(
            &cli,
            &MoveArgs {
                source_path: "/Archive/copied.txt".to_string(),
                destination_path: "/Upload/moved.txt".to_string(),
                replace: false,
            },
        )
        .await
        .unwrap();
        commands::get::run(
            &cli,
            &GetArgs {
                remote_path: "/Upload/moved.txt".to_string(),
                local_path: moved.clone(),
                replace: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&moved).await.unwrap(),
            "updated virtual cli"
        );
        commands::rm::run(
            &cli,
            &RmArgs {
                remote_path: "/Upload/moved.txt".to_string(),
                yes: true,
            },
        )
        .await
        .unwrap();
        assert!(commands::get::run(
            &cli,
            &GetArgs {
                remote_path: "/Upload/moved.txt".to_string(),
                local_path: PathBuf::from(&moved),
                replace: true,
            },
        )
        .await
        .is_err());
    }

    #[test]
    fn parent_path_for_one_component_is_root() {
        assert_eq!(helpers::parent_path_string(&["Music".to_string()]), "/");
    }

    #[test]
    fn parent_path_for_nested_component_is_parent() {
        assert_eq!(
            helpers::parent_path_string(&["GARMIN".to_string(), "APPS".to_string()]),
            "/GARMIN"
        );
    }
}
