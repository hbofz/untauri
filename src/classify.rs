#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind { Html, Css, Js, Json, Font, Image, Other, Bin }

impl Kind {
    // Output sub-directory, and the `kind` value in assets.json. Doubles as the extension
    // in the `NNN.<ext>` fallback used for streams with no recovered name.
    pub fn ext(&self) -> &'static str {
        match self {
            Kind::Html => "html", Kind::Css => "css", Kind::Js => "js", Kind::Json => "json",
            Kind::Font => "font", Kind::Image => "img", Kind::Other => "other", Kind::Bin => "bin",
        }
    }
}

// The record's own extension, when we recovered its name. Authoritative: a `.woff2` is a
// font whatever its bytes sniff as, and content sniffing cannot tell a font from garbage
// at all — which is how 39 KaTeX fonts were classified `bin` and dropped.
pub fn kind_from_ext(name: &str) -> Option<Kind> {
    kind_of_ext(name.rsplit_once('.')?.1)
}

// The extension alone, so a candidate can be rejected off a byte slice without first
// allocating a name string for it. `kind_from_ext` is the only other caller, so the
// extension table stays a single source of truth.
pub fn kind_of_ext(ext: &str) -> Option<Kind> {
    let ext = ext.to_ascii_lowercase();
    Some(match ext.as_str() {
        "html" | "htm" => Kind::Html,
        "css" => Kind::Css,
        "js" | "mjs" | "cjs" => Kind::Js,
        "json" | "map" => Kind::Json,
        "woff" | "woff2" | "ttf" | "otf" | "eot" => Kind::Font,
        "png" | "jpg" | "jpeg" | "svg" | "webp" | "ico" | "gif" | "avif" => Kind::Image,
        _ => return None,
    })
}

// Classify a recovered asset: by its real extension when we have a name, else by sniffing
// its bytes. A named stream never falls to `Bin` (which `--raw` gates); only an
// unidentifiable keyless stream does.
pub fn classify_asset(name: Option<&str>, bytes: &[u8]) -> Kind {
    match name.and_then(kind_from_ext) {
        Some(k) => k,
        None => match classify(bytes) {
            Kind::Bin if name.is_some() => Kind::Other,
            k => k,
        },
    }
}

pub fn classify(bytes: &[u8]) -> Kind {
    let n = bytes.len().min(4096);
    // printable ratio gate: reject binary
    let printable = bytes[..n].iter().filter(|b| matches!(b, 9|10|13|32..=126)).count();
    if n == 0 || (printable as f32) / (n as f32) < 0.85 { return Kind::Bin; }

    let head = String::from_utf8_lossy(&bytes[..n]);
    let s = head.trim_start();
    let lower = s.to_ascii_lowercase();

    if lower.starts_with("<!doctype") || lower.starts_with("<html") { return Kind::Html; }

    // JSON before CSS: shiki grammars are JSON objects that contain css-ish words
    if (s.starts_with('{') || s.starts_with('[')) && serde_json::from_str::<serde_json::Value>(s.trim_end()).is_ok() {
        return Kind::Json;
    }

    let looks_js = s.starts_with("import")
        || s.starts_with("function")
        || s.starts_with("const ")
        || s.starts_with("var ")
        || s.starts_with("export ")
        || s.contains("__vite__")
        || s.contains("=>");
    if looks_js { return Kind::Js; }

    // CSS needs a strong signal, not just one stray `{` `:` `}`: an at-rule / `:root`,
    // or a real run of `property:value;` declarations. The old `contains("--")` /
    // single-brace fallback matched any data blob with those characters (a 236KB
    // non-CSS blob was mislabelled `css`), so require substance.
    let strong = s.contains("@layer")
        || s.contains("@media")
        || s.contains("@font-face")
        || s.contains("@supports")
        || s.contains("@keyframes")
        || s.contains("@charset")
        || s.contains(":root");
    if strong || declaration_count(s) >= 3 {
        return Kind::Css;
    }

    Kind::Bin
}

// Count `property:value;` declarations (incl. `--custom:...;`). A real stylesheet has
// many; HTML/data blobs with incidental `{` `:` `}` do not.
fn declaration_count(s: &str) -> usize {
    let b = s.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < b.len() {
        let start = i;
        while i < b.len() && (b[i].is_ascii_lowercase() || b[i] == b'-') {
            i += 1;
        }
        // property name >= 2 chars, then ':', then a non-empty value ending in ';'
        if i - start >= 2 && i < b.len() && b[i] == b':' {
            let mut j = i + 1;
            while j < b.len() && !matches!(b[j], b';' | b'{' | b'}') {
                j += 1;
            }
            // a declaration ends at `;` or, for the last one in a block, `}`
            if j < b.len() && matches!(b[j], b';' | b'}') && j > i + 1 {
                count += 1;
                i = j + 1;
                continue;
            }
        }
        i = i.max(start + 1);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn detects_html() { assert_eq!(classify(b"<!doctype html><html><head>"), Kind::Html); }
    #[test] fn detects_css() { assert_eq!(classify(b":root{--theme-background:#0f0f0f;color:red}"), Kind::Css); }
    #[test] fn detects_tailwind_layer() { assert_eq!(classify(b"@layer theme{:root{--x:1}}"), Kind::Css); }
    #[test] fn detects_js() { assert_eq!(classify(b"const __vite__mapDeps=(i,m)=>{import(\"./x.js\")}"), Kind::Js); }
    #[test] fn detects_json() { assert_eq!(classify(b"{\"name\":\"x\",\"v\":[1,2,3]}"), Kind::Json); }
    #[test] fn binary_is_bin() { assert_eq!(classify(&[0u8,159,146,150,0,1,2]), Kind::Bin); }
    #[test] fn shiki_json_grammar_is_json_not_css() { assert_eq!(classify(b"{\"displayName\":\"CSS\",\"patterns\":[]}"), Kind::Json); }

    // A non-CSS data blob with incidental `{` `:` `}` and `--` must NOT be called CSS.
    // The old fallback (contains("--") || {`:`}) labelled this `css`; mutation check:
    // reverting looks_css to that fallback flips this back to Css and fails the test.
    #[test]
    fn data_blob_with_stray_braces_is_not_css() {
        let blob = b"administration transportation International settings <meta name=\"x\"> \
                     favicon.ico here is {some} text a:b and a <!-- comment --> more words";
        assert_eq!(classify(blob), Kind::Bin);
    }

    // Plain minified CSS with no at-rule still classifies via declaration density.
    #[test]
    fn minified_css_without_at_rules_is_css() {
        assert_eq!(classify(b".a{color:red;margin:0;padding:0}.b{display:flex}"), Kind::Css);
    }

    // A font's bytes sniff as binary and used to be dropped as `Bin`; the record's own
    // extension is authoritative. Mutation check: drop the `kind_from_ext` lookup from
    // `classify_asset` and the woff2 assertion fails (it comes back `Other`).
    #[test]
    fn extension_beats_content_sniffing_for_named_records() {
        let woff2 = [0x77, 0x4f, 0x46, 0x32, 0x00, 0x01, 0x00, 0x00, 0xff, 0xfe, 0x00, 0x03];
        assert_eq!(classify(&woff2), Kind::Bin, "content sniffing alone cannot see a font");
        assert_eq!(classify_asset(Some("KaTeX_Main-Bold-I9j0K1l2.woff2"), &woff2), Kind::Font);
        assert_eq!(Kind::Font.ext(), "font", "fonts are written under font/");
        assert_eq!(classify_asset(Some("logo-A1B2C3D4.svg"), b"<svg viewBox=\"0 0 1 1\"/>"), Kind::Image);
        assert_eq!(Kind::Image.ext(), "img", "images are written under img/");
        // a named record with an unknown extension is still extracted, not gated behind --raw
        assert_eq!(classify_asset(Some("data-A1B2C3D4.wasm"), &woff2), Kind::Other);
        // a keyless unidentifiable stream stays Bin
        assert_eq!(classify_asset(None, &woff2), Kind::Bin);
    }

    // One lone declaration is not enough to call something CSS.
    #[test]
    fn single_declaration_is_not_css() {
        assert_eq!(classify(b"label text height:tall in a sentence, nothing structural here"), Kind::Bin);
    }
}
