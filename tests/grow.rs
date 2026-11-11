// End-to-end proof that `repack` can grow a file past its original size: build the
// fixture app (tests/fixture/app.rs, a Tauri-shaped asset table it prints at run time),
// extract it, make app.js much bigger, repack, and RUN the patched binary. Only the
// operating system's own loader accepting it, and the program printing the new bytes,
// shows the new segment and the repointed table entry are right.
//
// On formats growth does not support yet, repack must refuse cleanly and write nothing.
//
// Mutation check: skip the relocation addend update in grow::point and the patched
// binary prints the old app.js (or crashes); skip the program header rewrite and it
// fails to load.

use std::path::{Path, PathBuf};
use std::process::Command;

fn untauri(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_untauri")).args(args).output().unwrap()
}

#[test]
fn grown_file_is_served_by_the_patched_binary() {
    let dir: PathBuf = std::env::temp_dir().join(format!("untauri-grow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = |name: &str| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let (app, patched, out) = (exe("app"), exe("patched"), dir.join("out"));

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture/app.rs");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let built = Command::new(rustc).arg("-O").arg(&src).arg("-o").arg(&app).status().unwrap();
    assert!(built.success(), "building the fixture app failed");

    let x = untauri(&[app.as_os_str(), "-o".as_ref(), out.as_os_str(), "--tree".as_ref()]);
    assert!(x.status.success(), "extract failed: {}", String::from_utf8_lossy(&x.stderr));
    let grown = format!("console.log('fixture app');{}", ";console.log('grown');".repeat(200));
    std::fs::write(out.join("app.js"), &grown).unwrap();

    let r = untauri(&["repack".as_ref(), app.as_os_str(), out.as_os_str(), "-o".as_ref(), patched.as_os_str()]);
    let stderr = String::from_utf8_lossy(&r.stderr);
    if cfg!(any(target_os = "linux", target_os = "android")) {
        assert!(r.status.success(), "repack failed: {stderr}");
        assert!(stderr.contains("grown"), "app.js should have been moved, not patched in place: {stderr}");
        let run = Command::new(&patched).output().unwrap();
        assert!(run.status.success(), "the patched binary did not run");
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(stdout.lines().any(|l| l == format!("/app.js {grown}")), "patched binary printed:\n{stdout}");
        assert!(stdout.lines().any(|l| l == "/style.css body{margin:0}"), "an untouched file changed:\n{stdout}");
    } else {
        assert!(!r.status.success(), "growth is not supported on this format yet, so repack must refuse");
        assert!(stderr.contains("Linux and Android"), "unexpected message: {stderr}");
        assert!(!patched.exists(), "nothing may be written when repack refuses");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
