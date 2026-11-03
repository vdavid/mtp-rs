#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    mtp_rs::fuzzing::fuzz_device_info_parse(data);
});
