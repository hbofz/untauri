// Write edited files back into a Tauri binary.
//
// Each asset is patched in place (see `table::patch`): the new bytes, brotli-compressed
// when the app was built that way, go over the old ones and the entry's length shrinks
// to match. Nothing moves, so no pointer, relocation or section header changes. The
// catch: an edited file must still fit in its original span.
// ponytail: in-place only; growing an asset needs a new section per format (ELF, Mach-O, PE).

use crate::output::{content_hash, Entry};
use crate::scan::Asset;
use crate::{grow, table, target};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

// The smallest stream any quality level gives. Tauri itself uses quality 9, and higher
// is not always smaller: on small files 9 can beat 11 by a dozen bytes, which is the
// whole slot. Only edited files get here, so trying every level is cheap.
fn brotli(bytes: &[u8]) -> Vec<u8> {
    (0..=11).map(|q| {
        let mut out = Vec::new();
        { let mut w = brotli::CompressorWriter::new(&mut out, 4096, q, 22); w.write_all(bytes).unwrap(); }
        out
    }).min_by_key(Vec::len).unwrap()
}

// `out` None is a dry run: everything up to and including the read-back, then stop.
pub fn run(path: &Path, dir: &Path, out: Option<&Path>) -> ExitCode {
    let bin = target::resolve_binary(path);
    let mut data = match std::fs::read(&bin) {
        Ok(d) => d,
        Err(e) => { say!("error: cannot read {}: {e}", bin.display()); return ExitCode::from(1); }
    };
    let manifest: Vec<Entry> = match std::fs::read_to_string(dir.join("assets.json")).map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
        Ok(m) => m,
        Err(e) => { say!("error: cannot read {}: {e}", dir.join("assets.json").display()); return ExitCode::from(1); }
    };
    if !manifest.is_empty() && manifest.iter().all(|e| e.key.is_none() || e.hash.is_none()) {
        say!("error: {} was written by an older untauri, which did not record what repack needs. Extract again with this version, then edit that output.", dir.join("assets.json").display());
        return ExitCode::from(1);
    }
    // Alias rows count too: in table mode they are keys whose identical bytes the linker
    // merged, and their edits matter. Keys the table cannot take back are reported below.
    let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
    // What extraction recorded for each key. Editing a file does not change it, so a
    // mismatch means the directory came from a different build of the app.
    let mut recorded: HashMap<&str, &str> = HashMap::new();
    for e in &manifest {
        let Some(key) = e.key.as_deref() else { continue };
        if let Some(h) = e.hash.as_deref() { recorded.insert(key, h); }
        match std::fs::read(dir.join(&e.name)) {
            Ok(b) => { files.insert(key, b); }
            Err(err) => say!("warning: skipping {} ({err})", e.name),
        }
    }

    let mut changed: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();
    let mut mismatched: Vec<String> = Vec::new();
    let mut before: Vec<table::Table> = Vec::new();
    let slices = target::slices(&data);
    // Files that no longer fit in place: (entry slots, stored bytes). They move to new
    // space at the end of the binary once every in-place patch is done.
    let can_grow = slices.len() == 1 && grow::supported(&data);
    let mut grown: Vec<(Vec<usize>, Vec<u8>)> = Vec::new();
    let mut moved: Vec<String> = Vec::new();
    for (i, r) in slices.iter().enumerate() {
        let slice = &mut data[r.clone()];
        let Some(t) = table::read(slice) else {
            say!("error: no Tauri asset table in {}; repack needs one", bin.display());
            return ExitCode::from(1);
        };
        for a in &t.assets {
            let key = a.path.as_deref().unwrap_or_default();
            if recorded.get(key).is_some_and(|&rec| rec != content_hash(&a.bytes)) && !mismatched.iter().any(|m| m == key) {
                mismatched.push(key.to_string());
            }
        }
        // A universal binary repeats the same table per slice; say each thing once.
        for key in files.keys().filter(|_| i == 0) {
            if t.undecoded.iter().any(|u| u == key) {
                say!("warning: {key} is stored in a form that does not decode, so it cannot be repacked; skipped");
            } else if !t.slots.contains_key(*key) {
                say!("warning: {key} is not in the app's asset table; skipped");
            }
        }
        // Patching a span changes every key stored in it, so they must all end up equal.
        let mut spans: BTreeMap<usize, Vec<&Asset>> = BTreeMap::new();
        for a in &t.assets { spans.entry(a.offset).or_default().push(a); }
        for group in spans.values() {
            let key = |a: &Asset| a.path.clone().unwrap_or_default();
            let want: Vec<&Vec<u8>> = group.iter().map(|a| files.get(key(a).as_str()).unwrap_or(&a.bytes)).collect();
            if want.iter().all(|w| **w == group[0].bytes) { continue; }
            if want.iter().any(|w| *w != want[0]) {
                let names: Vec<String> = group.iter().map(|a| key(a)).collect();
                errors.push(format!("  {} are one stored copy in the binary (identical files the linker merged). Give them identical content, or leave them all unchanged.", names.join(", ")));
                continue;
            }
            // A zero length would end the table for our own reader, so the patch could not
            // be verified. Brotli of an empty file is one byte, so only raw builds hit this.
            if !t.compressed && want[0].is_empty() {
                errors.push(format!("  {}: a file can't be emptied in an uncompressed build. Leave one byte in it, like a newline.", key(group[0])));
                continue;
            }
            let stored = if t.compressed { brotli(want[0]) } else { want[0].clone() };
            let slots: Vec<usize> = group.iter().map(|a| t.slots[&key(a)]).collect();
            match table::patch(slice, group[0], &slots, &stored) {
                Ok(()) => for a in group { changed.insert(key(a), want[0].clone()); },
                Err(_) if can_grow => {
                    for a in group { changed.insert(key(a), want[0].clone()); moved.push(key(a)); }
                    grown.push((slots, stored));
                }
                Err(over) => errors.push(format!("  {}: {over} bytes over its {}-byte slot", key(group[0]), group[0].compressed_len)),
            }
        }
        before.push(t);
    }
    if !mismatched.is_empty() {
        mismatched.sort();
        say!("error: {} was extracted from a different build of this app ({} file(s) differ from what it holds, e.g. {}). Nothing written.",
            dir.display(), mismatched.len(), mismatched[0]);
        say!("Extract this binary again and redo the edits in that output.");
        return ExitCode::from(1);
    }
    if !errors.is_empty() {
        errors.sort();
        errors.dedup();
        say!("error: {} problem(s), nothing written:", errors.len());
        for m in &errors { say!("{m}"); }
        if errors.iter().any(|m| m.contains("over its")) {
            say!("Each file has to fit in the space its original took. Trim the edit (minify it, drop comments) and repack again. (Growing files is supported for Linux and Android binaries so far.)");
        }
        return ExitCode::from(1);
    }
    if changed.is_empty() {
        say!("no extracted file differs from the app's own; nothing written.");
        return ExitCode::SUCCESS;
    }

    if !grown.is_empty() {
        let blobs: Vec<Vec<u8>> = grown.iter().map(|g| g.1.clone()).collect();
        let res = grow::append(&mut data, &blobs).and_then(|vaddrs| {
            for ((slots, stored), va) in grown.iter().zip(vaddrs) {
                for &s in slots { grow::point(&mut data, s, va, stored.len())?; }
            }
            Ok(())
        });
        if let Err(e) = res { say!("error: {e}. Nothing written."); return ExitCode::from(1); }
    }
    // Growing made the file longer, so re-measure it before the read-back.
    let slices = if grown.is_empty() { slices } else { target::slices(&data) };
    let listing = |k: &str| say!("  {k}{}", if moved.iter().any(|m| m == k) { " (grown: moved to new space at the end of the binary)" } else { "" });

    // Read the result back the same way extraction does: every edited file must come
    // out as edited and every other file exactly as it was, so a bad patch never ships.
    for (r, t) in slices.iter().zip(&before) {
        let after = table::read(&data[r.clone()]);
        let got: HashMap<&str, &Vec<u8>> = after.iter().flat_map(|a| &a.assets)
            .filter_map(|a| Some((a.path.as_deref()?, &a.bytes))).collect();
        let bad = t.assets.iter().map(|a| (a.path.as_deref().unwrap_or_default(), &a.bytes))
            .find(|(k, orig)| got.get(k).copied() != Some(changed.get(*k).unwrap_or(orig)));
        if let Some((k, _)) = bad {
            say!("error: {k} did not read back correctly from the patched binary; nothing written. Please report this.");
            return ExitCode::from(1);
        }
    }

    let Some(out) = out else {
        say!("dry run: {} asset(s) would be patched, and the result checks out. Nothing written.", changed.len());
        for k in changed.keys() { listing(k); }
        return ExitCode::SUCCESS;
    };
    if let Err(e) = std::fs::write(out, &data) { say!("error: cannot write {}: {e}", out.display()); return ExitCode::from(1); }
    if let Ok(m) = std::fs::metadata(&bin) { let _ = std::fs::set_permissions(out, m.permissions()); }
    say!("patched {} asset(s) -> {}", changed.len(), out.display());
    for k in changed.keys() { listing(k); }
    match object::FileKind::parse(&*data) {
        Ok(object::FileKind::MachO64 | object::FileKind::MachOFat32 | object::FileKind::MachOFat64) => say!(
            "note: the code signature no longer matches. Put the binary back in its .app and re-sign:\n  codesign --force --deep --preserve-metadata=entitlements -s - <App>.app"),
        Ok(object::FileKind::Pe64 | object::FileKind::Pe32) => say!(
            "note: if the app was code-signed, that signature no longer matches; Windows may warn before running it."),
        _ => {}
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mutation check: pin quality 11 and this tiny input comes out larger than the
    // quality-9 stream Tauri wrote, so a one-byte edit to a small file would not fit.
    #[test]
    fn brotli_keeps_the_smallest_level_and_round_trips() {
        let input = b"import{_ as o}from\"./entry.js\";const t={};export{t as default};\n";
        let q = |q| { let mut o = Vec::new(); { let mut w = brotli::CompressorWriter::new(&mut o, 4096, q, 22); w.write_all(input).unwrap(); } o.len() };
        let best = brotli(input);
        assert!(best.len() <= q(9) && best.len() <= q(11));
        assert_eq!(crate::scan::decode_exact(&best, 0, best.len()).as_deref(), Some(&input[..]));
    }
}
