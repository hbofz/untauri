// End-to-end exit-code contract for --strict. A bundle that references a chunk the
// extraction never wrote must exit non-zero under --strict and zero without it. No such
// test existed anywhere; the completeness check's whole point is this exit behaviour.
//
// Mutation check: change `ExitCode::from(3)` in main.rs to `ExitCode::SUCCESS` and the
// strict assertion below fails.

use std::io::Write;
use std::process::Command;

fn br(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    { let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22); w.write_all(input).unwrap(); }
    out
}

fn build_input(path: &std::path::Path) {
    // One recoverable JS record that imports a chunk which is NOT present in the input.
    let mut data = Vec::new();
    data.extend_from_slice(b"/assets/main-AAAA1111.js");
    data.extend_from_slice(&br(br#"import("./ghost-BBBB2222.js");export default 1;"#));
    std::fs::write(path, &data).unwrap();
}

fn run(input: &std::path::Path, out: &std::path::Path, strict: bool) -> i32 {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_untauri"));
    cmd.arg(input).arg("-o").arg(out);
    if strict { cmd.arg("--strict"); }
    cmd.status().unwrap().code().unwrap()
}

#[test]
fn strict_exits_nonzero_when_a_referenced_chunk_is_missing() {
    let tmp = std::env::temp_dir().join(format!("untauri-strict-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let input = tmp.join("fake-binary");
    build_input(&input);

    let without = run(&input, &tmp.join("out-a"), false);
    let with = run(&input, &tmp.join("out-b"), true);

    assert_eq!(without, 0, "a missing reference is a warning, not a failure, without --strict");
    assert_ne!(with, 0, "--strict must fail when a referenced chunk is missing");
    assert_eq!(with, 3, "--strict uses exit code 3 for a completeness gap");

    // sanity: the record itself extracted, so the gap is the reference, not a dead scan
    let manifest = std::fs::read_to_string(tmp.join("out-b/assets.json")).unwrap();
    assert!(manifest.contains("main-AAAA1111.js"), "the JS record should have been recovered");
    assert!(!manifest.contains("ghost-BBBB2222.js"), "the ghost chunk was never in the input");

    let _ = std::fs::remove_dir_all(&tmp);
}
