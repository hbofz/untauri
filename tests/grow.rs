// End-to-end proof that `repack` can grow a file past its original size: build the
// fixture app (tests/fixture/app.rs, a Tauri-shaped asset table it prints at run time),
// extract it, make app.js much bigger, repack, and RUN the patched binary. Only the
// operating system's own loader accepting it, and the program printing the new bytes,
// shows the new segment and the repointed table entry are right.
//
// On macOS it runs twice: built for macOS 11 (classic rebase opcodes) and for macOS 13
// (chained fixups), the two ways a pointer slot is stored. A patched Mach-O is re-signed
// ad hoc before it runs, as the user is told to do; Apple Silicon kills it otherwise.
//
// Mutation check: skip the relocation addend update in grow::point and the patched
// binary prints the old app.js (or crashes); skip the program header rewrite and it
// fails to load.

use std::path::{Path, PathBuf};
use std::process::Command;

fn untauri(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_untauri")).args(args).output().unwrap()
}

// Run a patched binary, re-signing it first on macOS.
fn launch(bin: &Path) -> std::process::Output {
    if cfg!(target_os = "macos") {
        // dyld's own malformedness check. The loader tolerates some mistakes it reports
        // (a chained-fixups segment count that is off), so running alone doesn't prove them.
        if let Ok(v) = Command::new("dyld_info").arg("-validate_only").arg(bin).output() {
            assert!(v.status.success(), "dyld_info rejects {}: {}", bin.display(), String::from_utf8_lossy(&v.stdout));
        }
        let s =Command::new("codesign").args(["--force", "-s", "-"]).arg(bin).output().unwrap();
        assert!(s.status.success(), "codesign rejected {}: {}", bin.display(), String::from_utf8_lossy(&s.stderr));
        let v = Command::new("codesign").args(["--verify", "--strict"]).arg(bin).output().unwrap();
        assert!(v.status.success(), "codesign --verify --strict failed on {}: {}", bin.display(), String::from_utf8_lossy(&v.stderr));
    }
    Command::new(bin).output().unwrap()
}

#[test]
fn grown_file_is_served_by_the_patched_binary() {
    if cfg!(target_os = "macos") {
        grow_and_run("11.0");
        grow_and_run("13.0");
    } else {
        grow_and_run("");
    }
}

fn grow_and_run(macos_target: &str) {
    let dir: PathBuf = std::env::temp_dir().join(format!("untauri-grow-{}-{macos_target}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = |name: &str| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let (app, patched, out) = (exe("app"), exe("patched"), dir.join("out"));

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture/app.rs");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let mut cmd = Command::new(rustc);
    if !macos_target.is_empty() { cmd.env("MACOSX_DEPLOYMENT_TARGET", macos_target); }
    let built = cmd.arg("-O").arg(&src).arg("-o").arg(&app).status().unwrap();
    assert!(built.success(), "building the fixture app failed");

    let x = untauri(&[app.as_os_str(), "-o".as_ref(), out.as_os_str(), "--tree".as_ref()]);
    assert!(x.status.success(), "extract failed: {}", String::from_utf8_lossy(&x.stderr));
    let grown = format!("console.log('fixture app');{}", ";console.log('grown');".repeat(200));
    std::fs::write(out.join("app.js"), &grown).unwrap();

    let r = untauri(&["repack".as_ref(), app.as_os_str(), out.as_os_str(), "-o".as_ref(), patched.as_os_str()]);
    let stderr = String::from_utf8_lossy(&r.stderr);
    {
        assert!(r.status.success(), "repack failed: {stderr}");
        assert!(stderr.contains("grown"), "app.js should have been moved, not patched in place: {stderr}");
        let run = launch(&patched);
        assert!(run.status.success(), "the patched binary did not run");
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(stdout.lines().any(|l| l == format!("/app.js {grown}")), "patched binary printed:\n{stdout}");
        assert!(stdout.lines().any(|l| l == "/style.css body{margin:0}"), "an untouched file changed:\n{stdout}");

        // A second repack of the grown binary extends the same segment.
        let (out2, patched2) = (dir.join("out2"), exe("patched2"));
        let x = untauri(&[patched.as_os_str(), "-o".as_ref(), out2.as_os_str(), "--tree".as_ref()]);
        assert!(x.status.success(), "extracting the grown binary failed: {}", String::from_utf8_lossy(&x.stderr));
        let css = format!("body{{margin:0}}/*{}*/", "c".repeat(3000));
        std::fs::write(out2.join("style.css"), &css).unwrap();
        let r = untauri(&["repack".as_ref(), patched.as_os_str(), out2.as_os_str(), "-o".as_ref(), patched2.as_os_str()]);
        assert!(r.status.success(), "second repack failed: {}", String::from_utf8_lossy(&r.stderr));
        let check = |bin: &Path| {
            let run = launch(bin);
            assert!(run.status.success(), "{} did not run", bin.display());
            let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
            assert!(stdout.lines().any(|l| l == format!("/app.js {grown}")), "{} lost the first growth:\n{stdout}", bin.display());
            assert!(stdout.lines().any(|l| l == format!("/style.css {css}")), "{} lost the second growth:\n{stdout}", bin.display());
        };
        check(&patched2);

        // objcopy rewrites the file and moves symbol and debug sections after ours. A repack
        // after that must leave them intact (it once cut off everything after its segment).
        // ELF only: these are the Linux packaging tools.
        let elf = cfg!(any(target_os = "linux", target_os = "android"));
        let (copied, out3, patched3) = (exe("copied"), dir.join("out3"), exe("patched3"));
        if elf && Command::new("objcopy").arg(&patched2).arg(&copied).status().is_ok_and(|s| s.success()) {
            let syms = |bin: &Path| Command::new("readelf").args(["-sW"]).arg(bin).output().map(|o| (o.stdout, o.stderr)).unwrap();
            let x = untauri(&[copied.as_os_str(), "-o".as_ref(), out3.as_os_str(), "--tree".as_ref()]);
            assert!(x.status.success());
            std::fs::write(out3.join("index.html"), "<!doctype html>".to_string() + &"<p>more</p>".repeat(400)).unwrap();
            let r = untauri(&["repack".as_ref(), copied.as_os_str(), out3.as_os_str(), "-o".as_ref(), patched3.as_os_str()]);
            assert!(r.status.success(), "repack after objcopy failed: {}", String::from_utf8_lossy(&r.stderr));
            let (before, after) = (syms(&copied), syms(&patched3));
            assert!(after.1.is_empty(), "readelf complains after the repack: {}", String::from_utf8_lossy(&after.1));
            assert_eq!(before.0, after.0, "the symbol table changed");
            check(&patched3);
        }

        // Packaging tools strip binaries. The grown data has a section of its own, so it
        // survives: without one, strip drops the segment and the app crashes.
        let stripped = exe("stripped");
        if elf && Command::new("strip").arg("-o").arg(&stripped).arg(&patched2).status().is_ok_and(|s| s.success()) {
            check(&stripped);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
