use crate::classify::{kind_from_ext, Kind};
use crate::output::Entry;
use crate::scan::Asset;
use std::collections::BTreeSet;

// Completeness check. Both real extraction gaps on a real Tauri app (17 JS chunks, 39 fonts)
// were silent: assets.json reported success and the frontend only broke when a dynamic
// import 404'd at runtime. The recovered bundle names its own dependencies, so diff what
// it references against what was written and say so when something is missing.

// Asset filenames referenced by the recovered JS/HTML (`__vite__mapDeps` entries and
// import specifiers are plain `"assets/x.js"` / `"./x.js"` string literals) and CSS
// (`url(...)` targets).
pub fn referenced(items: &[(Asset, Kind)]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (asset, kind) in items {
        match kind {
            // Root-absolute paths are read out of HTML only. In HTML `href="/x.svg"` is
            // unambiguously a bundle asset; in minified JS a quoted `/name.ext` is far
            // more often a fragment being concatenated or an in-memory path, and every
            // such hit is a false `--strict` failure. Measured across seven shipped
            // apps, allowing it in JS produced six false positives and no true ones:
            // `"".concat(cfg.paths.vs,"/loader.js")`, `od(t,`node_modules`,e,`package.json`)`,
            // and `WriteImage(r,"/image.png")` (an emscripten FS path) among them.
            Kind::Html => scan_specifiers(&asset.bytes, &mut out, true),
            Kind::Js => scan_specifiers(&asset.bytes, &mut out, false),
            Kind::Css => scan_urls(&asset.bytes, &mut out),
            _ => {}
        }
    }
    out
}

// Referenced names that were never written.
pub fn missing(items: &[(Asset, Kind)], entries: &[Entry]) -> Vec<String> {
    let written: BTreeSet<&str> = entries.iter().map(|e| basename(&e.name)).collect();
    referenced(items).into_iter().filter(|r| !written.contains(r.as_str())).collect()
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn name_char(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
}

// `"assets/x-HASH.js"` / `"./x-HASH.js"` / `"/favicon.ico"`. The name must terminate on a
// quote or paren: a run that stops mid-identifier is some other string, not a specifier.
//
// `root_absolute` adds the bare `/` marker, which the first two miss entirely: a shipped
// `index.html` carrying `href="/vite.svg"` reported clean while vite.svg was never
// extracted. Only HTML enables it (see `referenced`). Even there it is guarded on an
// opening quote so a remote or protocol-relative URL is not read as a local reference.
fn scan_specifiers(bytes: &[u8], out: &mut BTreeSet<String>, root_absolute: bool) {
    let markers: &[&[u8]] = if root_absolute {
        &[b"assets/", b"./", b"/"]
    } else {
        &[b"assets/", b"./"]
    };
    for marker in markers.iter().copied() {
        let mut i = 0;
        while i + marker.len() < bytes.len() {
            let Some(rel) = bytes[i..].windows(marker.len()).position(|w| w == marker) else { break };
            let start = i + rel;
            let s = start + marker.len();
            // A root-absolute reference must be a whole quoted string. Two rejects, both
            // measured on a real bundle: an unquoted slash is any old path fragment, and
            // a *concatenated* one is a URL being built, not a local asset — the analytics
            // call `"/array/"+t.config.token+"/config.js"` was reported missing until the
            // `+` on either side was ruled out.
            let mut quoted = false;
            if marker == b"/" {
                let open = match start.checked_sub(1) { Some(o) => o, None => { i = s; continue; } };
                if !matches!(bytes[open], b'"' | b'\'' | b'`') { i = s; continue; }
                if open > 0 && bytes[open - 1] == b'+' { i = s; continue; }
                quoted = true;
            }
            let mut e = s;
            while e < bytes.len() && name_char(bytes[e]) { e += 1; }
            let terminated = e < bytes.len() && matches!(bytes[e], b'"' | b'\'' | b'`' | b')');
            let concatenated = quoted && bytes.get(e + 1) == Some(&b'+');
            if terminated && !concatenated && e > s {
                if let Ok(name) = std::str::from_utf8(&bytes[s..e]) {
                    if kind_from_ext(name).is_some() { out.insert(name.to_string()); }
                }
            }
            i = s;
        }
    }
}

fn scan_urls(bytes: &[u8], out: &mut BTreeSet<String>) {
    let mut i = 0;
    while i + 4 < bytes.len() {
        let Some(rel) = bytes[i..].windows(4).position(|w| w == b"url(") else { break };
        let s = i + rel + 4;
        let Some(len) = bytes[s..].iter().position(|&b| b == b')') else { break };
        if let Ok(raw) = std::str::from_utf8(&bytes[s..s + len]) {
            let raw = raw.trim().trim_matches(['"', '\'']);
            // skip data: URIs, absolute and protocol-relative (`//cdn...`) URLs; strip
            // ?query / #fragment (`#iefix` etc.). A remote asset was never in the binary,
            // so flagging it would be a false "missing" that fails --strict on a clean run.
            if !raw.starts_with("data:") && !raw.contains("://") && !raw.starts_with("//") {
                let name = basename(raw.split(['?', '#']).next().unwrap_or(raw));
                if kind_from_ext(name).is_some() { out.insert(name.to_string()); }
            }
        }
        i = s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(bytes: &[u8], kind: Kind) -> (Asset, Kind) {
        (Asset { offset: 0, compressed_len: 1, bytes: bytes.to_vec(), name: None, path: None }, kind)
    }
    fn entry(name: &str) -> Entry {
        Entry { name: name.into(), key: None, original_name: None, kind: "js".into(), offset: 0, compressed_len: 1, decompressed_len: 1, hash: None, alias_of: None }
    }

    // Root-absolute references were invisible to the scan, so a bundle whose index.html
    // carries `href="/vite.svg"` reported clean with vite.svg never extracted. The two
    // rejects keep that from becoming noise: a bare path inside a URL, and a fragment
    // being concatenated into one. Mutation check: drop the `+` guard and config.js
    // (a real analytics endpoint from a shipped binary) is reported missing.
    #[test]
    fn sees_a_root_absolute_reference_but_not_a_built_url() {
        let mut out = BTreeSet::new();
        scan_specifiers(br#"<link href="/vite.svg"><script src="/app.js"></script>"#, &mut out, true);
        scan_specifiers(br#"<img src="https://cdn.example.com/remote.js"><a href="//cdn.io/p.js">"#, &mut out, true);
        assert!(out.contains("vite.svg"), "got {out:?}");
        assert!(out.contains("app.js"), "got {out:?}");
        assert!(!out.contains("remote.js"), "a remote URL is not a local asset");
        assert!(!out.contains("p.js"), "a protocol-relative URL is not a local asset");

        // The same spellings inside JS must stay invisible: every one of these is a real
        // false positive measured on a shipped app, and each would fail `--strict`.
        let mut js = BTreeSet::new();
        scan_specifiers(br#"x.concat(t.config.paths.vs,"/loader.js")"#, &mut js, false);
        scan_specifiers(br#"od(t,`node_modules`,e,`package.json`)"#, &mut js, false);
        scan_specifiers(br#"e.WriteImage(r,"/image.png")"#, &mut js, false);
        scan_specifiers(br#"endpointFor("assets","/array/"+t.token+"/config.js")"#, &mut js, false);
        assert!(js.is_empty(), "root-absolute paths in JS must not be reported: {js:?}");
    }

    // The safety net: a chunk the bundle imports but the extraction never wrote is
    // reported. Mutation check: make `missing` return an empty Vec and this fails.
    #[test]
    fn flags_a_referenced_chunk_that_was_not_written() {
        let js = br#"const __vite__mapDeps=(i,m)=>{};__vite__mapDeps(["assets/PanelWindow-E5f6G7h8.js","assets/index-AAAA1111.js"])"#;
        let items = vec![item(js, Kind::Js)];
        let entries = vec![entry("js/index-AAAA1111.js")];
        assert_eq!(missing(&items, &entries), vec!["PanelWindow-E5f6G7h8.js".to_string()]);
    }

    // A bundled font the stylesheet needs counts as referenced. A font served from a CDN
    // does not: it was never in the binary, so flagging it would be a false "missing".
    // Mutation check: drop the `://` / `data:` guard and the length assertion fails.
    #[test]
    fn css_url_bundled_fonts_are_referenced_and_remote_ones_are_not() {
        let css = br#"@font-face{src:url(./KaTeX_Main-Bold-I9j0K1l2.woff2#iefix) format("woff2"),url("https://fonts.gstatic.com/s/inter-v13.woff2") format("woff2"),url(//cdn.example.com/proto-rel.woff2),url(data:font/woff;base64,AAAA)}"#;
        let refs = referenced(&[item(css, Kind::Css)]);
        assert!(refs.contains("KaTeX_Main-Bold-I9j0K1l2.woff2"), "bundled font must be referenced (and #iefix stripped): {refs:?}");
        assert_eq!(refs.len(), 1, "remote (incl. protocol-relative) and data: urls must not be referenced: {refs:?}");
    }

    // Everything referenced was written: no warning. Mutation check: compare full paths
    // instead of basenames and this reports a spurious miss.
    #[test]
    fn reports_nothing_when_every_reference_was_written() {
        let js = br#"import("./graph-Xy78Zw90.js")"#;
        let items = vec![item(js, Kind::Js)];
        let entries = vec![entry("js/graph-Xy78Zw90.js")];
        assert!(missing(&items, &entries).is_empty());
    }
}
