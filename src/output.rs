use crate::classify::Kind;
use crate::scan::Asset;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    // Original hashed filename recovered from the app's asset-key strings, when the
    // stream was decoded off one. None when named by the fallback `<ext>/NNN.ext` scheme.
    pub original_name: Option<String>,
    pub kind: String,
    pub offset: usize,
    pub compressed_len: usize,
    pub decompressed_len: usize,
    // Set when this row's bytes are a copy of an earlier asset (a Vite content-hash twin
    // whose own stream did not decode). The two rows share `offset`, so without this the
    // manifest looks like two distinct extractions at one offset. Names the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias_of: Option<String>,
}

// A full HTML document, not a fragment: starts (after whitespace) with `<!doctype` or
// `<html`. Only such a keyless asset earns the `index.html` name.
fn is_document(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(64)]);
    let s = head.trim_start().to_ascii_lowercase();
    s.starts_with("<!doctype") || s.starts_with("<html")
}

// A key path reduced to a safe relative output path: leading slashes stripped, and
// rejected outright if any component could escape the output dir. `--tree` writes here.
fn safe_tree_path(path: &str) -> Option<String> {
    let rel = path.trim_start_matches('/');
    if rel.is_empty() || rel.contains('\\') { return None; }
    if rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") { return None; }
    Some(rel.to_string())
}

pub fn write(out_dir: &Path, items: &[(Asset, Kind)], manifest_only: bool, tree: bool) -> std::io::Result<Vec<Entry>> {
    use std::collections::{HashMap, HashSet};
    std::fs::create_dir_all(out_dir)?;
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    // Under `--tree`, one asset's path can be another's parent directory (a bogus key
    // recovered from binary noise is enough: `assets/x.js` alongside `assets/x.js/y.css`).
    // Writing the file then fails with "Is a directory". Reserve every directory a tree
    // path needs up front and treat a hit as an ordinary name collision.
    let mut dirs: HashSet<String> = HashSet::new();
    if tree {
        for (asset, _) in items {
            let Some(p) = asset.path.as_deref().and_then(safe_tree_path) else { continue };
            let mut acc = String::new();
            for comp in p.split('/').take(p.split('/').count().saturating_sub(1)) {
                if !acc.is_empty() { acc.push('/'); }
                acc.push_str(comp);
                dirs.insert(acc.clone());
            }
        }
    }
    let mut entries = Vec::new();
    for (asset, kind) in items {
        let ext = kind.ext();
        // Only trust a recovered name that is a single, safe filename component: never
        // empty, `.`/`..`, or containing a path separator (a hostile/odd asset-key string
        // must not escape the kind dir or collide with the parent). Otherwise fall back.
        let safe_orig = asset.name.as_deref().filter(|o| {
            !o.is_empty() && *o != "." && *o != ".." && !o.contains('/') && !o.contains('\\')
        });
        // Prefer the recovered original name (kept under its kind dir so the tree is
        // stable); fall back to `<ext>/NNN.ext`. Disambiguate every name collision,
        // including a recovered name that happens to equal an NNN value.
        // `--tree` rebuilds the bundle's own directory layout, so the output dir can be
        // served directly: index.html's `./assets/main-x.js` resolves to a real file.
        // The default kind-dir layout does not, which is only a browsing convenience.
        let tree_name = if tree { asset.path.as_deref().and_then(safe_tree_path) } else { None };
        let name = match tree_name {
            Some(p) => {
                let mut candidate = p.clone();
                let mut n = 1;
                while used.contains(&candidate) || dirs.contains(&candidate) {
                    candidate = match p.rsplit_once('/') {
                        Some((dir, base)) => format!("{dir}/{n}-{base}"),
                        None => format!("{n}-{p}"),
                    };
                    n += 1;
                }
                candidate
            }
            None => match safe_orig {
            Some(orig) => {
                let mut candidate = format!("{ext}/{orig}");
                let mut n = 1;
                while used.contains(&candidate) {
                    candidate = format!("{ext}/{n}-{orig}");
                    n += 1;
                }
                candidate
            }
            // The root document has no `/assets/` record, so it arrives keyless. It is the
            // entry point: name it `index.html` so the output dir can be served as-is. Guard
            // on a real document head so a keyless HTML *fragment* at a lower offset can't
            // steal the name ahead of the true root.
            None if *kind == Kind::Html && !used.contains("html/index.html") && !used.contains("index.html") && is_document(&asset.bytes) => {
                if tree { "index.html".to_string() } else { "html/index.html".to_string() }
            }
            None => {
                let idx = counts.entry(ext).or_insert(0);
                let mut candidate = format!("{}/{:03}.{}", ext, *idx, ext);
                *idx += 1;
                while used.contains(&candidate) {
                    candidate = format!("{}/{:03}.{}", ext, *idx, ext);
                    *idx += 1;
                }
                candidate
            }
            },
        };
        used.insert(name.clone());
        // A shared offset means these bytes were copied from an earlier row (the hash-alias
        // fallback), never two real extractions — the merge dedups real assets by offset.
        // Name that earlier row so the manifest doesn't misreport provenance.
        let alias_of = entries.iter()
            .find(|e: &&Entry| e.offset == asset.offset)
            .map(|e| e.name.clone());
        if !manifest_only {
            let full = out_dir.join(&name);
            if let Some(parent) = full.parent() { std::fs::create_dir_all(parent)?; }
            std::fs::write(&full, &asset.bytes)?;
        }
        entries.push(Entry {
            name,
            // Report the name we actually trusted, not a rejected unsafe one.
            original_name: safe_orig.map(|s| s.to_string()),
            kind: ext.to_string(),
            offset: asset.offset,
            compressed_len: asset.compressed_len,
            decompressed_len: asset.bytes.len(),
            alias_of,
        });
    }
    let json = serde_json::to_string_pretty(&entries).unwrap();
    std::fs::write(out_dir.join("assets.json"), json)?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `--tree` rebuilds the bundle's own layout so the output dir can be served directly.
    // A key path that could escape the output dir must be refused and fall back, and a
    // path that is also another asset's parent directory must be disambiguated rather
    // than fail the write with "Is a directory".
    // Mutation check: return `Some(rel)` unconditionally from `safe_tree_path` and the
    // traversal assertion fails; drop the `dirs` reservation and the collision one does.
    #[test]
    fn tree_mode_rebuilds_key_paths_and_refuses_unsafe_ones() {
        let dir = std::env::temp_dir().join(format!("tu-test-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            (Asset { offset: 1, compressed_len: 5, bytes: b"body{a:1}".to_vec(), name: Some("m.css".into()), path: Some("/assets/m.css".into()) }, Kind::Css),
            (Asset { offset: 2, compressed_len: 5, bytes: b"<!doctype html><p>x".to_vec(), name: None, path: None }, Kind::Html),
            (Asset { offset: 3, compressed_len: 5, bytes: b"a{b:2}".to_vec(), name: Some("e.css".into()), path: Some("/../escape/e.css".into()) }, Kind::Css),
            (Asset { offset: 4, compressed_len: 5, bytes: b"c{d:3}".to_vec(), name: Some("m.css".into()), path: Some("/assets/m.css/nested.css".into()) }, Kind::Css),
        ];
        let entries = write(&dir, &items, false, true).unwrap();
        assert_eq!(entries[0].name, "assets/1-m.css", "collides with a needed directory");
        assert_eq!(entries[1].name, "index.html", "the keyless root document sits at the root");
        assert_eq!(entries[2].name, "css/e.css", "a traversing key path falls back to the kind dir, keeping the safe name");
        assert_eq!(entries[3].name, "assets/m.css/nested.css");
        assert!(dir.join("assets/m.css/nested.css").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Two rows sharing an offset means the second is a hash-alias copy of the first; it
    // must carry `alias_of` naming the source, and a distinct-offset row must not.
    // Mutation check: make `alias_of` always None and the second assertion fails.
    #[test]
    fn alias_row_records_its_source_and_distinct_rows_do_not() {
        let dir = std::env::temp_dir().join(format!("tu-test-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            (Asset { offset: 500, compressed_len: 9, bytes: b".a{color:red}".to_vec(), name: Some("real-M3n4O5p6.css".into()), path: None }, Kind::Css),
            (Asset { offset: 500, compressed_len: 9, bytes: b".a{color:red}".to_vec(), name: Some("twin-M3n4O5p6.css".into()), path: None }, Kind::Css),
        ];
        let entries = write(&dir, &items, true, false).unwrap();
        assert_eq!(entries[0].alias_of, None, "the first row at an offset is the real extraction");
        assert_eq!(entries[1].alias_of.as_deref(), Some("css/real-M3n4O5p6.css"), "the second names its source");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_files_and_manifest() {
        let dir = std::env::temp_dir().join(format!("tu-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            (Asset { offset: 10, compressed_len: 5, bytes: b"<!doctype html>".to_vec(), name: None, path: None }, Kind::Html),
            (Asset { offset: 20, compressed_len: 6, bytes: b":root{--x:1}".to_vec(), name: None, path: None }, Kind::Css),
        ];
        let entries = write(&dir, &items, false, false).unwrap();
        assert_eq!(entries.len(), 2);
        // the keyless root document is the entry point, so it is `index.html`, not `000.html`
        assert_eq!(entries[0].name, "html/index.html");
        assert_eq!(entries[0].decompressed_len, 15);
        assert!(dir.join("html/index.html").exists());
        assert!(dir.join("css/000.css").exists());
        assert!(dir.join("assets.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A recovered name is written under its kind dir with its real basename; a keyless
    // asset keeps the NNN scheme; a duplicate recovered name is disambiguated.
    #[test]
    fn recovered_names_are_written_and_collisions_disambiguated() {
        let dir = std::env::temp_dir().join(format!("tu-test-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            (Asset { offset: 1, compressed_len: 5, bytes: b".a{color:red}".to_vec(), name: Some("main-A1b2C3d4.css".into()), path: None }, Kind::Css),
            (Asset { offset: 2, compressed_len: 5, bytes: b".b{color:blue}".to_vec(), name: Some("main-A1b2C3d4.css".into()), path: None }, Kind::Css),
            (Asset { offset: 3, compressed_len: 5, bytes: b".c{color:green}".to_vec(), name: None, path: None }, Kind::Css),
        ];
        let entries = write(&dir, &items, false, false).unwrap();
        assert_eq!(entries[0].name, "css/main-A1b2C3d4.css");
        assert_eq!(entries[0].original_name.as_deref(), Some("main-A1b2C3d4.css"));
        assert_eq!(entries[1].name, "css/1-main-A1b2C3d4.css", "duplicate recovered name disambiguated");
        assert_eq!(entries[2].name, "css/000.css", "keyless asset keeps the NNN scheme");
        assert_eq!(entries[2].original_name, None);
        assert!(dir.join("css/main-A1b2C3d4.css").exists());
        assert!(dir.join("css/1-main-A1b2C3d4.css").exists());
        assert!(dir.join("css/000.css").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A recovered name that collides with the fallback NNN scheme must not silently
    // overwrite: the keyless asset skips the used name and both files survive.
    #[test]
    fn keyed_name_equal_to_nnn_does_not_clobber_keyless() {
        let dir = std::env::temp_dir().join(format!("tu-test-clobber-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            (Asset { offset: 1, compressed_len: 5, bytes: b".keyed{a:1}".to_vec(), name: Some("000.css".into()), path: None }, Kind::Css),
            (Asset { offset: 2, compressed_len: 5, bytes: b".keyless{b:2}".to_vec(), name: None, path: None }, Kind::Css),
        ];
        let entries = write(&dir, &items, false, false).unwrap();
        assert_eq!(entries[0].name, "css/000.css");
        assert_ne!(entries[1].name, entries[0].name, "keyless asset must not reuse the keyed name");
        assert!(dir.join(&entries[0].name).exists());
        assert!(dir.join(&entries[1].name).exists());
        // both sets of bytes survive on disk
        assert_eq!(std::fs::read(dir.join(&entries[0].name)).unwrap(), b".keyed{a:1}");
        assert_eq!(std::fs::read(dir.join(&entries[1].name)).unwrap(), b".keyless{b:2}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // An unsafe recovered basename (`.`, `..`, empty, or with a separator) is rejected and
    // the asset falls back to the NNN scheme instead of aborting the whole run.
    #[test]
    fn unsafe_basename_falls_back_and_does_not_abort() {
        let dir = std::env::temp_dir().join(format!("tu-test-unsafe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            (Asset { offset: 1, compressed_len: 5, bytes: b".a{x:1}".to_vec(), name: Some("..".into()), path: None }, Kind::Css),
            (Asset { offset: 2, compressed_len: 5, bytes: b".b{y:2}".to_vec(), name: Some("a/b.css".into()), path: None }, Kind::Css),
        ];
        let entries = write(&dir, &items, false, false).unwrap(); // must NOT error out
        assert_eq!(entries[0].name, "css/000.css");
        assert_eq!(entries[0].original_name, None, "unsafe name is not reported as recovered");
        assert_eq!(entries[1].name, "css/001.css");
        assert_eq!(entries[1].original_name, None);
        assert!(dir.join("css/000.css").exists());
        assert!(dir.join("css/001.css").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
