// Rebuild original source files from source maps shipped in the frontend.
//
// A source map lists the files a bundle was built from (`sources`) and, often, their
// full text (`sourcesContent`). When an app ships its `.map` files, that text is the
// original, unminified TypeScript/JSX, so writing it out recovers the source tree.
//
// Paths in `sources` are attacker-controlled (`../../../../etc/x`, `webpack:///C:/x`,
// `CON`), so each is reduced to a relative path that cannot leave `<out>/sources/`.

use crate::scan::Asset;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

#[derive(Default, Debug)]
pub struct Stats {
    // every `.map` asset, readable or not
    pub maps: usize,
    // invalid JSON, or an index map (`sections`) this does not expand
    pub unusable: usize,
    // listed in a map but without its text, so nothing to write
    pub no_content: usize,
    // a path that reduced to nothing, or a write that failed
    pub skipped: usize,
    pub written: usize,
    pub node_modules: usize,
}

pub struct Plan { pub files: Vec<(String, String)>, pub stats: Stats }

fn is_map(a: &Asset) -> bool { a.name.as_deref().is_some_and(|n| n.ends_with(".map")) }

// (path, text) for every source that has text, plus how many had none. None when the
// map is not usable at all.
fn parse(bytes: &[u8]) -> Option<(Vec<(String, String)>, usize)> {
    let mut v: Value = serde_json::from_slice(bytes).ok()?;
    if v.get("sections").is_some() { return None; }
    let root = v.get("sourceRoot").and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_owned);
    let mut contents = match v.get_mut("sourcesContent") { Some(Value::Array(a)) => std::mem::take(a), _ => Vec::new() };
    let Some(Value::Array(sources)) = v.get("sources") else { return None };
    let (mut out, mut none) = (Vec::new(), 0);
    for (i, src) in sources.iter().enumerate() {
        match (src.as_str(), contents.get_mut(i).map(Value::take)) {
            (Some(src), Some(Value::String(text))) => {
                let full = match &root {
                    Some(r) if !src.contains("://") && !src.starts_with('/') => format!("{}/{src}", r.trim_end_matches('/')),
                    _ => src.to_string(),
                };
                out.push((full, text));
            }
            _ => none += 1,
        }
    }
    Some((out, none))
}

// Device names Windows opens instead of a file, whatever the extension.
fn windows_device(c: &str) -> bool {
    let stem = c.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit())
}

// `webpack:///./src/App.tsx?abc` -> `src/App.tsx`; `../../packages/x.ts` -> `packages/x.ts`.
// Never returns a path with a `..` component, an absolute path, a drive letter, a
// Windows device name, or a component Windows would silently rename.
pub fn safe_path(src: &str) -> Option<String> {
    let mut s = src.split(['?', '#']).next()?.replace('\\', "/");
    if let Some(i) = s.find("://") { s = s[i + 3..].to_string(); }
    if let Some(rest) = s.strip_prefix("webpack:") { s = rest.to_string(); }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() { s = s[2..].to_string(); }
    let mut parts: Vec<String> = Vec::new();
    for c in s.split('/') {
        // Windows drops trailing dots and spaces, so `.. ` would act as `..`.
        match c.trim_end_matches(['.', ' ']) {
            _ if c.trim_end_matches(' ') == ".." => { parts.pop(); }
            "" => {}
            t => {
                let clean: String = t.chars().map(|ch| if ch.is_control() || r#":*?"<>|"#.contains(ch) { '_' } else { ch }).collect();
                parts.push(if windows_device(&clean) { format!("_{clean}") } else { clean });
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

// Two paths a case-insensitive filesystem (macOS, Windows) would treat as one file
// get the same key, so the second becomes a `~N` sibling instead of overwriting.
pub fn key(rel: &str) -> String { rel.to_lowercase() }

fn suffixed(base: &str, n: usize) -> String {
    match base.rsplit_once('.') {
        Some((stem, ext)) if !ext.contains('/') && !stem.ends_with('/') && !stem.is_empty() => format!("{stem}~{n}.{ext}"),
        _ => format!("{base}~{n}"),
    }
}

fn hash(text: &str) -> u64 { let mut h = DefaultHasher::new(); text.hash(&mut h); h.finish() }

// Decide every file's output path without writing, so `inspect` reports exactly what
// extraction would write. `reserved` holds keys of paths already taken under sources/.
pub fn plan<'a>(assets: impl IntoIterator<Item = &'a Asset>, reserved: &HashSet<String>) -> Plan {
    let mut st = Stats::default();
    let mut files = Vec::new();
    // Bundles share sources (node_modules): the same text under one path is kept once,
    // different text gets a numbered sibling. Texts are compared by hash.
    let mut versions: HashMap<String, HashSet<u64>> = HashMap::new();
    let mut taken = reserved.clone();
    for a in assets.into_iter().filter(|a| is_map(a)) {
        st.maps += 1;
        let Some((entries, none)) = parse(&a.bytes) else { st.unusable += 1; continue };
        st.no_content += none;
        for (src, text) in entries {
            let Some(base) = safe_path(&src) else { st.skipped += 1; continue };
            let seen = versions.entry(key(&base)).or_default();
            if !seen.insert(hash(&text)) { continue; }
            let mut n = seen.len();
            let rel = loop {
                let rel = if n == 1 { base.clone() } else { suffixed(&base, n) };
                if taken.insert(key(&rel)) { break rel; }
                n += 1;
            };
            files.push((rel, text));
        }
    }
    Plan { files, stats: st }
}

pub fn write(out_dir: &Path, plan: Plan) -> Stats {
    let root = out_dir.join("sources");
    let mut st = plan.stats;
    for (rel, text) in plan.files {
        let path = root.join(&rel);
        let ok = path.parent().is_none_or(|p| std::fs::create_dir_all(p).is_ok()) && std::fs::write(&path, &text).is_ok();
        if !ok { st.skipped += 1; continue; }
        if rel.split('/').any(|c| c == "node_modules") { st.node_modules += 1; }
        st.written += 1;
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mutation check: drop the `..` pop, the scheme strip, the drive-letter strip, the
    // trailing-dot trim or the device-name prefix and the matching case below fails.
    #[test]
    fn paths_cannot_escape() {
        let p = |s: &str| safe_path(s);
        assert_eq!(p("../../../../packages/lib/x.ts").as_deref(), Some("packages/lib/x.ts"));
        assert_eq!(p("webpack:///./src/App.tsx?4a1f").as_deref(), Some("src/App.tsx"));
        assert_eq!(p("webpack://app/./src/a.js").as_deref(), Some("app/src/a.js"));
        assert_eq!(p("/etc/passwd").as_deref(), Some("etc/passwd"));
        assert_eq!(p("C:\\Users\\x\\src\\a.ts").as_deref(), Some("Users/x/src/a.ts"));
        assert_eq!(p("src/../../../x.ts").as_deref(), Some("x.ts"));
        assert_eq!(p("a/.. /.. /b.ts").as_deref(), Some("b.ts"), "`.. ` is `..` on Windows");
        assert_eq!(p("a/trail. /b").as_deref(), Some("a/trail/b"));
        assert_eq!(p("a/b:c*d.ts").as_deref(), Some("a/b_c_d.ts"));
        assert_eq!(p("x/CON").as_deref(), Some("x/_CON"));
        assert_eq!(p("nul.ts").as_deref(), Some("_nul.ts"));
        assert_eq!(p("COM1.js").as_deref(), Some("_COM1.js"));
        assert_eq!(p("console.ts").as_deref(), Some("console.ts"));
        assert_eq!(p("../.."), None);
        assert_eq!(p(""), None);
    }

    fn map_asset(json: &str) -> Asset {
        Asset { offset: 0, compressed_len: 0, bytes: json.as_bytes().to_vec(), name: Some("a.js.map".into()), path: Some("/a.js.map".into()) }
    }

    #[test]
    fn plans_conflicts_case_folding_and_unusable_maps() {
        let a = map_asset(r#"{"version":3,"sources":["../src/a.ts","../src/b.ts",null,"../../x.ts","src/App.tsx","src/app.tsx"],"sourcesContent":["A",null,"C","X","P","Q"]}"#);
        let b = map_asset(r#"{"version":3,"sourceRoot":"/r/","sources":["src/a.ts","../src/a.ts","../src/a.ts"],"sourcesContent":["R","A2","A"]}"#);
        let bad = map_asset(r#"{"version":3,"sources":["ok.ts",5],"sourcesContent":["k","v"]}"#);
        let index = map_asset(r#"{"version":3,"sections":[]}"#);
        let reserved: HashSet<String> = [key("x.ts")].into();
        let p = plan([&a, &b, &bad, &index], &reserved);
        let got: HashMap<&str, &str> = p.files.iter().map(|(r, t)| (r.as_str(), t.as_str())).collect();
        assert_eq!(got["src/a.ts"], "A");
        assert_eq!(got["src/a~2.ts"], "A2", "different text under one path is kept");
        assert_eq!(got["r/src/a.ts"], "R", "sourceRoot is applied");
        assert_eq!(got["x~2.ts"], "X", "a reserved path is not overwritten");
        assert_eq!(got["src/App.tsx"], "P");
        assert_eq!(got["src/app~2.tsx"], "Q", "case-only difference must not overwrite on macOS/Windows");
        assert_eq!(got["ok.ts"], "k", "one bad entry does not discard the map");
        assert_eq!(p.files.len(), 7, "the repeated identical `A` is written once");
        assert_eq!((p.stats.maps, p.stats.unusable, p.stats.no_content), (4, 1, 3));
    }

    #[test]
    fn many_conflicts_stay_fast() {
        let n = 20_000;
        let sources = vec!["\"a.ts\""; n].join(",");
        let contents: Vec<String> = (0..n).map(|i| format!("\"{i}\"")).collect();
        let m = map_asset(&format!(r#"{{"sources":[{sources}],"sourcesContent":[{}]}}"#, contents.join(",")));
        let t = std::time::Instant::now();
        assert_eq!(plan([&m], &HashSet::new()).files.len(), n);
        assert!(t.elapsed().as_secs() < 2, "conflict handling must not be quadratic");
    }

    #[test]
    fn writes_the_plan_inside_sources() {
        let dir = std::env::temp_dir().join(format!("tu-sources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = map_asset(r#"{"sources":["../../../../etc/x.ts","node_modules/y/i.js"],"sourcesContent":["X","Y"]}"#);
        let st = write(&dir, plan([&m], &HashSet::new()));
        assert_eq!(std::fs::read_to_string(dir.join("sources/etc/x.ts")).unwrap(), "X");
        assert_eq!((st.written, st.node_modules, st.skipped), (2, 1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
