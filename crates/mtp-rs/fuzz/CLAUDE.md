# fuzz: cargo-fuzz targets over the device-facing parsers

One `[[bin]]` per entry point in `src/fuzzing.rs` (the `fuzzing` feature), each fed bytes a device chose. A target must
return or reject, never panic, and most also assert a property (a round trip, or "never claims more than the buffer
held"). The function docs in `src/fuzzing.rs` say what each one checks and why it matters.

## Files

- `fuzz_targets/*.rs`: six-line wrappers that call `mtp_rs::fuzzing::<name>`
- `corpus/<target>/seed_*.bin`: committed seeds, built in `src/fuzzing.rs` (`mod seeds`) and written by
  `just fuzz-seeds`. Everything else a run adds is gitignored. The `every_seed_reaches_its_target` unit test (run by
  `just test-all`) proves each seed gets past its parser, so every property runs on the seeds.
- `Cargo.lock`: its own workspace, so the main grouped Renovate PR doesn't refresh it

## Adding a target

1. Add the function to `src/fuzzing.rs` and a line to its module doc.
2. Add the wrapper and a `[[bin]]` here. The weekly workflow reads the `[[bin]]` names, so it needs no edit.
3. Add seeds to `mod seeds` and an arm to `every_seed_reaches_its_target`, then `just fuzz-seeds`.
4. A crash it finds becomes a unit test next to the parser (see `response_container_length_below_header_is_an_error`)
   and a seed.

## Rules for a target (OSS Verify)

These targets are written to qualify for [OSS Verify](https://theorem.dev/oss-verify/), which proves that a target
can't crash on any input. Its model is one Linux process whose only data is the input, so a target must:

- start no thread and no async runtime, read no clock, use no network or environment, and open no file (so no
  `PtpSession`: its timeouts run on `futures-timer`'s thread);
- take its "random" choices from the input bytes;
- bound every loop that comes from the input (a device-chosen count walks at most the buffer it's in);
- express a property as `assert!` (kept in release builds; the panic aborts), never `debug_assert!`;
- not install a panic hook or catch panics.

OSS Verify builds with rustc 1.90 (nightly-2025-07-16), below our 1.91 MSRV, and lowers `rust-version` with a build
patch. Keep the parsers compiling on 1.90; the weekly `oss-verify-toolchain` job catches a slip.
