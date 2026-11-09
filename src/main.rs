use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

mod check;
mod classify;
mod inspect;
mod output;
mod scan;
mod table;
mod target;

use classify::{classify_asset, Kind};

/// Extract the embedded web frontend out of a compiled Tauri app binary.
#[derive(Parser, Debug)]
#[command(name = "untauri", version, about, args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Path to a .app bundle or a raw executable
    #[arg(required = true)]
    path: Option<PathBuf>,
    /// Output directory
    #[arg(short, long, default_value = "unpacked")]
    out: PathBuf,
    /// Also write streams that could not be classified as web assets
    #[arg(long)]
    raw: bool,
    /// Print the recovered asset table to stderr
    #[arg(short, long)]
    verbose: bool,
    /// Write only assets.json, do not extract files
    #[arg(long)]
    manifest_only: bool,

    /// Rebuild the bundle's original directory layout instead of grouping by kind,
    /// so the output directory can be served as-is
    #[arg(long)]
    tree: bool,
    /// Exit non-zero if any file failed to extract
    #[arg(long)]
    strict: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Report what an app is built with and what its frontend can call, without extracting
    Inspect {
        /// Path to a .app bundle or a raw executable
        path: PathBuf,
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
        /// Write every crate version found as a Cargo.lock, for `cargo audit -f <FILE>`
        #[arg(long, value_name = "FILE")]
        lockfile: Option<PathBuf>,
    },
}

pub struct Recovered {
    pub assets: Vec<scan::Asset>,
    pub undecoded: Vec<String>,
    pub from_table: bool,
    pub compressed: bool,
}

// Tauri's own asset table gives every file's exact name and bytes. Only when there is
// none (an unusual build, or a format the reader does not know) fall back to scanning
// the binary for brotli streams, which is a guess and can pick up noise.
pub fn recover(loaded: &target::Loaded, log: bool) -> Recovered {
    if let Some(t) = table::read(&loaded.data) {
        if log {
            eprintln!("read Tauri's asset table: {} entries ({})", t.assets.len() + t.undecoded.len(),
                if t.compressed { "brotli" } else { "uncompressed" });
        }
        return Recovered { assets: t.assets, undecoded: t.undecoded, from_table: true, compressed: t.compressed };
    }
    if log { eprintln!("no Tauri asset table found; falling back to scanning for brotli streams (names and completeness are best-effort)"); }
    let (mut assets, mut undecoded) = (Vec::new(), Vec::new());
    for r in &loaded.ranges {
        let (found, failed) = scan::walk(&loaded.data, r.clone());
        assets.extend(found);
        undecoded.extend(failed);
    }
    Recovered { assets, undecoded, from_table: false, compressed: true }
}

fn load(path: &std::path::Path) -> Option<target::Loaded> {
    match target::load(path) {
        Ok(l) => Some(l),
        Err(e) => { eprintln!("error: not a readable binary: {} ({e})", path.display()); None }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Cmd::Inspect { path, json, lockfile }) = &cli.cmd {
        let Some(loaded) = load(path) else { return ExitCode::from(1) };
        return inspect::run(&loaded, *json, lockfile.as_deref());
    }
    let Some(path) = cli.path.as_deref() else { return ExitCode::from(2) }; // clap enforces it
    let Some(loaded) = load(path) else { return ExitCode::from(1) };

    let Recovered { assets, undecoded, from_table, .. } = recover(&loaded, true);
    let table_len = assets.len() + undecoded.len();

    let stream_count = assets.len();
    let mut items: Vec<(scan::Asset, Kind)> = assets.into_iter()
        .map(|a| { let k = classify_asset(a.name.as_deref(), &a.bytes); (a, k) })
        .filter(|(_, k)| cli.raw || *k != Kind::Bin)
        .collect();

    // No HTML, JS or CSS means no frontend was found, whatever else decoded. Saying
    // "complete" over a pile of decoded noise is the worst possible answer.
    if !items.iter().any(|(_, k)| matches!(k, Kind::Html | Kind::Js | Kind::Css)) {
        eprintln!("found {stream_count} streams but no HTML/JS/CSS: no web frontend recovered. This may not be a Tauri app, or it is a debug build that loads assets from disk.");
        return ExitCode::from(2);
    }

    items.extend(hash_aliases(&undecoded, &items));

    if cli.verbose {
        for (a, k) in &items { eprintln!("  {:>10} {:<5} off={} clen={}", a.bytes.len(), k.ext(), a.offset, a.compressed_len); }
    }

    let entries = match output::write(&cli.out, &items, cli.manifest_only, cli.tree) {
        Ok(entries) => entries,
        Err(e) => { eprintln!("error writing output: {e}"); return ExitCode::from(1); }
    };
    eprintln!("recovered {} assets -> {}", entries.len(), cli.out.display());

    if from_table {
        if undecoded.is_empty() {
            eprintln!("complete: all {table_len} entries in the app's asset table were extracted.");
        } else {
            eprintln!("warning: {} of {table_len} table entries did not decode:", undecoded.len());
            for u in &undecoded { eprintln!("  undecoded: {u}"); }
        }
    }
    let missing = check::missing(&items, &entries);
    if missing.is_empty() {
        eprintln!("references: every JS/CSS chunk and font the bundle names (mapDeps, ./chunk imports, CSS url()) was extracted.");
    } else if from_table {
        // The table is everything the app embeds, so these are not extraction gaps: the
        // app loads them from elsewhere (network, disk, a runtime-built path) or never does.
        eprintln!("note: {} name(s) referenced by the bundle are not in the app's asset table (loaded from elsewhere at runtime, or unused):", missing.len());
        for m in &missing { eprintln!("  not embedded: {m}"); }
    } else {
        eprintln!("warning: {} asset(s) named by the recovered bundle were NOT extracted:", missing.len());
        for m in &missing { eprintln!("  missing: {m}"); }
    }
    let incomplete = !undecoded.is_empty() || (!from_table && !missing.is_empty());
    if cli.strict && incomplete { ExitCode::from(3) } else { ExitCode::SUCCESS }
}

// A record whose stream will not decode may be a Vite duplicate: two emitted names for
// identical content, stored once (`classDiagram-v2-Q7r8S9t0-M3n4O5p6.js` shares the
// `M3n4O5p6` content hash with `classDiagram-U1v2W3x4-M3n4O5p6.js`). Copy the twin's
// bytes out under the failing record's name.
fn hash_aliases(undecoded: &[String], items: &[(scan::Asset, Kind)]) -> Vec<(scan::Asset, Kind)> {
    let mut out = Vec::new();
    for name in undecoded {
        let Some(key) = hash_key(name) else { continue };
        let Some((src, kind)) = items.iter().find(|(a, _)| a.name.as_deref().and_then(hash_key) == Some(key.clone())) else { continue };
        eprintln!("note: {name} did not decode; emitting a copy of {} (same content hash)", src.name.as_deref().unwrap_or("?"));
        out.push((
            // Same directory as the twin, under the failing record's own name, so `--tree`
            // places the copy where the bundle expects to find it.
            scan::Asset {
                offset: src.offset,
                compressed_len: src.compressed_len,
                bytes: src.bytes.clone(),
                name: Some(name.clone()),
                path: src.path.as_deref().map(|p| match p.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/{name}"),
                    None => name.clone(),
                }),
            },
            *kind,
        ));
    }
    out
}

// `classDiagram-v2-Q7r8S9t0-M3n4O5p6.js` -> ("M3n4O5p6", "js"): Vite's content hash plus
// the extension, which together identify identical content. The hash must be Vite-shaped:
// exactly 8 base64-ish chars with BOTH an upper- and a lower-case letter. Without the
// mixed-case requirement, kebab-case names alias by their last word (`panel-window.js`
// and `dialog-window.js` both -> "window"), so a record that merely fails to decode would
// be handed the wrong file's bytes under its real name, and the completeness check, seeing
// the name written, would falsely report clean.
fn hash_key(name: &str) -> Option<(String, String)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let hash = stem.rsplit('-').next()?;
    let vite_shaped = hash.len() == 8
        && hash.chars().any(|c| c.is_ascii_uppercase())
        && hash.chars().any(|c| c.is_ascii_lowercase());
    vite_shaped.then(|| (hash.to_string(), ext.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> (scan::Asset, Kind) {
        (scan::Asset { offset: 7, compressed_len: 9, bytes: b"export default 1;".to_vec(), name: Some(name.into()), path: None }, Kind::Js)
    }

    // Mutation check: drop the hash comparison (match any asset) and the negative case
    // below starts producing an alias; drop the alias entirely and the first fails.
    #[test]
    fn aliases_an_undecodable_record_to_its_content_hash_twin() {
        let items = vec![asset("classDiagram-U1v2W3x4-M3n4O5p6.js")];
        let out = hash_aliases(&["classDiagram-v2-Q7r8S9t0-M3n4O5p6.js".to_string()], &items);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.name.as_deref(), Some("classDiagram-v2-Q7r8S9t0-M3n4O5p6.js"));
        assert_eq!(out[0].0.bytes, items[0].0.bytes);
    }

    #[test]
    fn does_not_alias_across_different_content_hashes() {
        let items = vec![asset("classDiagram-U1v2W3x4-ZZZZZZZZ.js")];
        assert!(hash_aliases(&["classDiagram-v2-Q7r8S9t0-M3n4O5p6.js".to_string()], &items).is_empty());
    }

    // Kebab-case names with no content hash must NOT alias by their last word. Mutation
    // check: drop the len==8 + mixed-case guard (`hash.len() >= 6`) and `window` aliases,
    // producing a copy of dialog-window's bytes under panel-window's name.
    #[test]
    fn kebab_case_names_without_a_hash_do_not_alias() {
        let items = vec![asset("dialog-window.js")];
        assert!(
            hash_aliases(&["panel-window.js".to_string()], &items).is_empty(),
            "a bare kebab word (`window`) is not a Vite content hash"
        );
        // ...but a real Vite pair still aliases (both share the 8-char mixed-case M3n4O5p6)
        let vite = vec![asset("classDiagram-U1v2W3x4-M3n4O5p6.js")];
        assert_eq!(hash_aliases(&["classDiagram-v2-Q7r8S9t0-M3n4O5p6.js".to_string()], &vite).len(), 1);
    }
}
