#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: Vec<&str>| {
    let mut ctx = rink_core::simple_context().unwrap();
    for line in &data {
        let _ = rink_core::one_line(&mut ctx, line);
    }
});
