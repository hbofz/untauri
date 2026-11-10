// `untauri inspect`: what an app is built with and what its frontend calls, read
// straight from the binary. Nothing here runs the app.
//
// - Crate versions come from panic-location paths rustc leaves in release builds
//   (`.../tauri-plugin-fs-2.4.2/src/...`). Each version found is exact, but only crates
//   with a panic site in linked code leave a path, so the list is a lower bound: macro
//   crates, build-only crates and panic-free code never show up. Git-built crates have
//   no version in their path and are reported separately.
// - The IPC list is what the frontend *references* (`plugin:fs|read_file`, `invoke("x")`
//   string literals), not what the app's capabilities *grant*. Tauri compiles the
//   granted set into code, which cannot be read back without emulating it.

use crate::classify::{classify_asset, Kind};
use crate::{recover, target::Loaded};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::ExitCode;

#[derive(Serialize, Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Crate { pub name: String, pub version: String }

#[derive(Serialize)]
struct Report {
    binary: String,
    tauri: Option<String>,
    runtime: Vec<Crate>,
    plugins: Vec<Crate>,
    crates: Vec<Crate>,
    // Dependencies built from a git checkout (no version in their path): repo -> commits
    git: BTreeMap<String, BTreeSet<String>>,
    frontend: Frontend,
    // plugin name -> commands the frontend references
    plugin_commands: BTreeMap<String, BTreeSet<String>>,
    // `invoke("name")` targets: the app's own Rust commands
    app_commands: BTreeSet<String>,
}

#[derive(Serialize)]
struct Frontend { files: usize, bytes: usize, from_table: bool, compressed: bool, source_maps: usize, original_sources: usize }

fn name_byte(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'-' || b == b'_' }

// A full semver version: MAJOR.MINOR.PATCH, then optional -prerelease and +build, each
// dot-separated non-empty identifiers, numbers without leading zeros. Anything looser
// would make cargo-audit reject the whole lockfile.
fn semver(v: &str) -> bool {
    let num = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (p == "0" || !p.starts_with('0'));
    let ids = |s: &str, numeric_rule: bool| s.split('.').all(|id| {
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && (!numeric_rule || !id.bytes().all(|b| b.is_ascii_digit()) || num(id))
    });
    let (rest, build) = match v.split_once('+') { Some((r, b)) => (r, Some(b)), None => (v, None) };
    let (core, pre) = match rest.split_once('-') { Some((c, p)) => (c, Some(p)), None => (rest, None) };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| num(p))
        && pre.is_none_or(|p| ids(p, true)) && build.is_none_or(|b| ids(b, false))
}

// `tauri-plugin-fs-2.4.2` -> (`tauri-plugin-fs`, `2.4.2`): split at the first `-` that
// starts a valid version. One pass: the name may only use name bytes, so stop at the
// first byte that is not one.
fn split_versioned(seg: &str) -> Option<Crate> {
    if !seg.bytes().next().is_some_and(|b| b.is_ascii_alphabetic()) { return None; }
    let name_end = seg.bytes().position(|b| !name_byte(b)).unwrap_or(seg.len());
    for (i, _) in seg[..name_end].match_indices('-') {
        let ver = &seg[i + 1..];
        if ver.bytes().next().is_some_and(|b| b.is_ascii_digit()) && semver(ver) {
            return Some(Crate { name: seg[..i].to_string(), version: ver.to_string() });
        }
    }
    None
}

// Paths under these parents are crates vendored into Rust's own standard library
// (`/rust/deps/hashbrown-0.15.2/src/`), not the app's dependencies.
const STD_DEPS: [&[u8]; 2] = [b"/rust/deps/", b"\\rust\\deps\\"];

// Every `<name>-<version>/src/` (or `\src\` on Windows) path in the binary.
pub fn crates(data: &[u8]) -> Vec<Crate> {
    let mut out = BTreeSet::new();
    for pat in [&b"/src/"[..], &b"\\src\\"[..]] {
        let mut i = 0;
        while let Some(p) = find(&data[i..], pat) {
            let end = i + p;
            let mut start = end;
            // A crate dir name is short; cap the walk so a long run of name bytes stays linear.
            while start > 0 && end - start < 128 && (name_byte(data[start - 1]) || data[start - 1] == b'.' || data[start - 1] == b'+') { start -= 1; }
            let std_dep = STD_DEPS.iter().any(|d| data[..start].ends_with(d));
            if let (false, Ok(seg)) = (std_dep, std::str::from_utf8(&data[start..end])) {
                if let Some(c) = split_versioned(seg) { out.insert(c); }
            }
            i = end + pat.len();
        }
    }
    out.into_iter().collect()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// `.../git/checkouts/<repo>-<16 hex>/<short commit>/...`: a dependency built from git.
fn git_deps(data: &[u8]) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for pat in [&b"checkouts/"[..], &b"checkouts\\"[..]] {
        let mut i = 0;
        while let Some(p) = find(&data[i..], pat) {
            let s = i + p + pat.len();
            let seg = |from: usize| -> Option<(&[u8], usize)> {
                let n = data.get(from..)?.iter().take_while(|&&b| name_byte(b) || b == b'.').count();
                matches!(data.get(from + n), Some(b'/') | Some(b'\\')).then(|| (&data[from..from + n], from + n + 1))
            };
            if let Some((dir, next)) = seg(s) {
                if let (Some(dash), Some((commit, _))) = (dir.iter().rposition(|&b| b == b'-'), seg(next)) {
                    let (repo, hash) = (&dir[..dash], &dir[dash + 1..]);
                    let hex = |v: &[u8]| !v.is_empty() && v.iter().all(u8::is_ascii_hexdigit);
                    if !repo.is_empty() && hash.len() == 16 && hex(hash) && hex(commit) {
                        out.entry(String::from_utf8_lossy(repo).into_owned()).or_default()
                            .insert(String::from_utf8_lossy(commit).into_owned());
                    }
                }
            }
            i = s;
        }
    }
    out
}

// `plugin:<name>|<command>` and `invoke("<command>")` string literals in frontend code.
// Both are lower bounds: minifiers often call invoke through an alias (`i(`cmd`)`), and
// those calls cannot be told apart from any other function call.
fn ipc(text: &[u8], plugins: &mut BTreeMap<String, BTreeSet<String>>, app: &mut BTreeSet<String>) {
    let mut i = 0;
    while let Some(p) = find(&text[i..], b"plugin:") {
        let s = i + p + 7;
        let n_end = s + text[s..].iter().take_while(|&&b| name_byte(b)).count();
        if text.get(n_end) == Some(&b'|') {
            let c_end = n_end + 1 + text[n_end + 1..].iter().take_while(|&&b| name_byte(b)).count();
            if n_end > s && c_end > n_end + 1 {
                let name = String::from_utf8_lossy(&text[s..n_end]).into_owned();
                plugins.entry(name).or_default().insert(String::from_utf8_lossy(&text[n_end + 1..c_end]).into_owned());
            }
        }
        i = s;
    }
    let mut i = 0;
    while let Some(p) = find(&text[i..], b"invoke") {
        let at = i + p;
        let mut s = at + 6;
        // Not part of a longer name (`_invoke`, `$invoke`) and not a method on `this`
        // (regenerator's `this._invoke("next")`, Leaflet's `this.invoke("setZIndex")`).
        let before = at.checked_sub(1).map(|j| text[j]);
        let longer_name = before.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$');
        let on_this = before == Some(b'.') && text[..at - 1].ends_with(b"this");
        if longer_name || on_this { i = at + 6; continue; }
        if text.get(s) == Some(&b')') { s += 1; } // `(0,x.invoke)("cmd")`
        if text.get(s) == Some(&b'(') {
            if let Some(&q) = text.get(s + 1).filter(|&&q| q == b'"' || q == b'\'' || q == b'`') {
                let c = s + 2;
                let e = c + text[c..].iter().take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_').count();
                if e > c && text.get(e) == Some(&q) { app.insert(String::from_utf8_lossy(&text[c..e]).into_owned()); }
            }
        }
        i = i + p + 6;
    }
}

fn binary_kind(data: &[u8]) -> String {
    use object::Object;
    match object::File::parse(data) {
        Ok(f) => format!("{:?} {:?}", f.format(), f.architecture()),
        Err(_) => "unknown format".to_string(),
    }
}

fn lockfile(crates: &[Crate]) -> String {
    let mut s = String::from("# Generated by untauri inspect from crate paths in the binary.\nversion = 3\n");
    for c in crates {
        s += &format!("\n[[package]]\nname = \"{}\"\nversion = \"{}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n", c.name, c.version);
    }
    s
}

pub fn run(loaded: &Loaded, json: bool, lock: Option<&Path>) -> ExitCode {
    let data = &loaded.data;
    let all = crates(data);
    // Every version of a crate, in case two are linked.
    let find_ver = |n: &str| {
        let v: Vec<&str> = all.iter().filter(|c| c.name == n).map(|c| c.version.as_str()).collect();
        (!v.is_empty()).then(|| v.join(", "))
    };
    let runtime: Vec<Crate> = all.iter()
        .filter(|c| matches!(c.name.as_str(), "tauri-runtime" | "tauri-runtime-wry" | "tauri-utils" | "wry" | "tao"))
        .cloned().collect();
    let plugins: Vec<Crate> = all.iter().filter(|c| c.name.starts_with("tauri-plugin-")).cloned().collect();

    let git = git_deps(data);
    // A git-built tauri has no version in its path; name the commit instead.
    let tauri = find_ver("tauri").or_else(|| git.get("tauri").map(|c| format!("git {}", c.iter().cloned().collect::<Vec<_>>().join(", "))));
    let rec = recover(loaded, false);
    let (mut plugin_commands, mut app_commands) = (BTreeMap::new(), BTreeSet::new());
    let mut bytes = 0;
    let plan = crate::sources::plan(&rec.assets, &Default::default());
    let (maps, original_sources) = (plan.stats.maps, plan.files.len());
    for a in &rec.assets {
        bytes += a.bytes.len();
        if matches!(classify_asset(a.name.as_deref(), &a.bytes), Kind::Js | Kind::Html) {
            ipc(&a.bytes, &mut plugin_commands, &mut app_commands);
        }
    }

    let report = Report {
        binary: binary_kind(data),
        tauri, git,
        runtime, plugins,
        frontend: Frontend { files: rec.assets.len(), bytes, from_table: rec.from_table, compressed: rec.compressed, source_maps: maps, original_sources },
        plugin_commands, app_commands,
        crates: all,
    };

    // An asset table alone is not proof (other Rust binaries hold similar arrays); a
    // Tauri app always links tauri, its runtime or its plugins.
    if report.tauri.is_none() && report.runtime.is_empty() && report.plugins.is_empty() {
        eprintln!("no tauri, wry, tao or tauri-plugin crates found: this does not look like a Tauri app.");
        return ExitCode::from(2);
    }

    if let Some(p) = lock {
        if let Err(e) = std::fs::write(p, lockfile(&report.crates)) {
            eprintln!("error writing {}: {e}", p.display());
            return ExitCode::from(1);
        }
        eprintln!("wrote {} crates to {} (audit with: cargo audit -f {})", report.crates.len(), p.display(), p.display());
        eprintln!("note: this is a lower bound (only crates with panic paths in the binary){}; a clean audit does not prove the app is clean.",
            if report.git.is_empty() { String::new() } else { format!(", and git-built deps ({}) are left out", report.git.keys().cloned().collect::<Vec<_>>().join(", ")) });
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        print_report(&report);
    }
    ExitCode::SUCCESS
}

fn print_report(r: &Report) {
    let list = |v: &[Crate], strip: &str| v.iter().map(|c| format!("{} {}", c.name.trim_start_matches(strip), c.version)).collect::<Vec<_>>().join(", ");
    println!("binary    {}", r.binary);
    println!("tauri     {}", r.tauri.as_deref().unwrap_or("not found"));
    if !r.runtime.is_empty() { println!("runtime   {}", list(&r.runtime, "")); }
    let no_plugins = if r.git.is_empty() { "none found" } else { "none with a version (see git deps)" };
    println!("plugins   {}", if r.plugins.is_empty() { no_plugins.to_string() } else { list(&r.plugins, "tauri-plugin-") });
    println!("crates    {} found with versions, a lower bound (--lockfile to audit them)", r.crates.len());
    let f = &r.frontend;
    println!("frontend  {} files, {} KB, {}{}", f.files, f.bytes / 1024,
        if !f.from_table { "found by scanning (best-effort)" } else if f.compressed { "brotli" } else { "uncompressed" },
        if f.source_maps > 0 { format!(", {} source maps with {} original files (extract with --sources)", f.source_maps, f.original_sources) } else { String::new() });
    if !r.git.is_empty() {
        let g: Vec<String> = r.git.iter().map(|(repo, c)| format!("{repo} @ {}", c.iter().cloned().collect::<Vec<_>>().join(", "))).collect();
        println!("git deps  {}", g.join(", "));
    }
    let n: usize = r.plugin_commands.values().map(|s| s.len()).sum();
    println!();
    println!("IPC referenced by the frontend: {} plugin commands, {} literal invoke() calls.", n, r.app_commands.len());
    println!("Both are lower bounds, and referenced is not granted: the app's capabilities decide what runs.");
    for (p, cmds) in &r.plugin_commands {
        println!("  {p}: {}", cmds.iter().cloned().collect::<Vec<_>>().join(", "));
    }
    if !r.app_commands.is_empty() {
        println!("  invoke(): {}", r.app_commands.iter().cloned().collect::<Vec<_>>().join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_crate_name_from_version() {
        let c = |s: &str| split_versioned(s).map(|c| (c.name, c.version));
        assert_eq!(c("tauri-plugin-fs-2.4.2"), Some(("tauri-plugin-fs".into(), "2.4.2".into())));
        assert_eq!(c("tauri-2.0.0-rc.3"), Some(("tauri".into(), "2.0.0-rc.3".into())));
        assert_eq!(c("h2-0.4.1"), Some(("h2".into(), "0.4.1".into())));
        assert_eq!(c("core"), None);
        assert_eq!(c("utf-8"), None);
        assert_eq!(c("-1.2.3"), None);
        assert_eq!(c("lib.rstauri-2.8.5"), None, "a crate name has no dots");
        assert_eq!(c("foo-1.0.0+build.5"), Some(("foo".into(), "1.0.0+build.5".into())));
        for bad in ["foo-1.2.3-", "bar-01.2.3", "baz-1.0.0-rc..1", "qux-1.0.0+", "a-1.0.0-01"] {
            assert_eq!(c(bad), None, "{bad} is not a version cargo accepts");
        }
    }

    #[test]
    fn skips_std_vendored_crates_and_stays_linear() {
        let data = b"\x00/rust/deps/hashbrown-0.15.2/src/raw.rs\x00/x/hashbrown-0.14.0/src/lib.rs";
        let v: Vec<_> = crates(data).into_iter().map(|c| c.version).collect();
        assert_eq!(v, vec!["0.14.0"]);
        let long = format!("{}/src/", "a-".repeat(200_000));
        let t = std::time::Instant::now();
        crates(long.as_bytes());
        assert!(t.elapsed().as_secs() < 2, "walk-back must stay linear");
    }

    // Mutation check: drop the Windows `\src\` pattern and the second crate is lost; drop
    // the name-byte walk-back and names come out truncated.
    #[test]
    fn finds_crates_in_unix_and_windows_paths() {
        let data = b"\x00/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tauri-2.8.5/src/app.rs\x00C:\\Users\\u\\.cargo\\registry\\src\\index.crates.io-1949cf8c6b5b557f\\wry-0.53.3\\src\\lib.rs\x00/rustc/abc/library/core/src/fmt.rs";
        let names: Vec<_> = crates(data).into_iter().map(|c| format!("{} {}", c.name, c.version)).collect();
        assert_eq!(names, vec!["tauri 2.8.5", "wry 0.53.3"]);
    }

    #[test]
    fn finds_git_checkouts() {
        let data = b"/home/u/.cargo/git/checkouts/tauri-69fbbe4d0942e697/d9bc695/crates/tauri/src/app.rs\x00checkouts/notes-zz/abc/";
        let g = git_deps(data);
        assert_eq!(g.len(), 1, "the second path has no 16-hex repo hash");
        assert!(g["tauri"].contains("d9bc695"));
    }

    #[test]
    fn finds_plugin_and_app_commands() {
        let js = br#"a("plugin:fs|read_file",{});b('plugin:dialog|open');(0,r.invoke)("get_settings");invoke(`save`);invoke(x);"plugin:|bad";this._invoke("next");this.invoke("setZIndex");$invoke("no")"#;
        let (mut p, mut a) = (BTreeMap::new(), BTreeSet::new());
        ipc(js, &mut p, &mut a);
        assert_eq!(p.keys().cloned().collect::<Vec<_>>(), vec!["dialog", "fs"]);
        assert!(p["fs"].contains("read_file"));
        assert_eq!(a.into_iter().collect::<Vec<_>>(), vec!["get_settings", "save"]);
    }
}
