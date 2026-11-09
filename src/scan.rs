use brotli::{BrotliDecompressStream, BrotliResult, BrotliState, HeapAlloc, HuffmanCode};

pub struct Asset {
    pub offset: usize,
    pub compressed_len: usize,
    pub bytes: Vec<u8>,
    // Original hashed filename recovered from the asset-key path anchor this stream
    // was decoded off (e.g. `main-A1b2C3d4.css`). None for streams found by forward-
    // chaining or the exhaustive scan, which have no key of their own.
    pub name: Option<String>,
    // The whole key path the name came from (`/assets/main-A1b2C3d4.css`), so `--tree`
    // can rebuild the original directory layout. None whenever `name` is.
    pub path: Option<String>,
}

// 4MB bounded probe window: a stream whose *compressed* length exceeds it cannot be
// decoded (brotli returns NeedsMoreInput). It was 1MB, which silently lost large vendor
// chunks. Decode stops at the stream's own end regardless of how much input it is
// handed, so a wider window costs nothing but the slice bound.
const WINDOW: usize = 4 << 20;
const MIN_OUT: usize = 8;        // ignore trivially tiny decodes
const MAX_OUT: usize = 64 * 1024 * 1024; // 64MB decompressed-output cap, guards against decompression bombs

// ponytail: the high-level brotli::Decompressor (io::Read) reads through trailing
// garbage past the stream's final block when asked to read_to_end, so `consumed`
// came back as the whole probe window instead of the true compressed length
// (confirmed by decodes_stream_and_reports_consumed_length_ignoring_trailing_garbage
// failing with left: 541, right: 41 against the window-sized input). Dropping to the
// low-level BrotliDecompressStream state machine and reading back `input_offset`
// (bytes actually consumed from the input slice) gives the true stream length.
// Test-only probe helper; `walk` calls `decode` directly (binary crate, no external callers).
#[cfg(test)]
fn try_decode_at(data: &[u8], off: usize) -> Option<(Vec<u8>, usize)> {
    let end = (off + WINDOW).min(data.len());
    match decode(data, off, end, MAX_OUT, false) {
        Decoded::Asset(bytes, consumed) if bytes.len() >= MIN_OUT => Some((bytes, consumed)),
        _ => None,
    }
}

#[cfg(test)]
fn try_decode_at_capped(data: &[u8], off: usize, max_out: usize) -> Option<(Vec<u8>, usize)> {
    let end = (off + WINDOW).min(data.len());
    match decode(data, off, end, max_out, false) {
        Decoded::Asset(bytes, consumed) if bytes.len() >= MIN_OUT => Some((bytes, consumed)),
        _ => None,
    }
}

// Decode exactly the brotli stream at data[off..off+len]: it must finish and consume
// every byte. The asset table gives the true length, so anything else is not a stream.
pub fn decode_exact(data: &[u8], off: usize, len: usize) -> Option<Vec<u8>> {
    match decode(data, off, off.checked_add(len)?, MAX_OUT, false) {
        Decoded::Asset(bytes, consumed) if consumed == len => Some(bytes),
        _ => None,
    }
}

enum Decoded {
    Asset(Vec<u8>, usize), // fully decoded: bytes + true compressed length
    Binary,                // valid brotli but decompresses to non-text (font/wasm/garbage)
    None,                  // not a brotli stream at this offset
}

// PEEK: how many output bytes to decode before deciding a stream is textual.
// A real web asset (html/css/js/json) is printable ASCII from byte 0; binary
// assets and coincidental brotli-valid runs are not. Judging on a small prefix
// lets `walk` reject the (very common) binary decodes without paying to inflate
// them fully — the __const section brotli-decodes at thousands of offsets.
const PEEK: usize = 4096;

// Bound the input to [off, end) so a stream cannot read past its section into
// unrelated data (kills the ~776KB Rust-debug-text over-read at the section tail).
// When `peek_gate` is set, abort with Binary as soon as the first PEEK output
// bytes prove non-textual, instead of inflating the whole (unwanted) blob.
fn decode(data: &[u8], off: usize, end: usize, max_out: usize, peek_gate: bool) -> Decoded {
    let end = end.min(data.len());
    if off >= end { return Decoded::None; }
    let input = &data[off..end];

    // new_strict (large_window=false): standard brotli only, the profile bundlers
    // and Tauri emit. Rejecting the large-window extension kills most coincidental
    // "valid" decodes in binary const data AND avoids allocating a multi-MB ring
    // buffer for every such false start, which made a per-offset scan unusable.
    let mut state = BrotliState::new_strict(
        HeapAlloc::<u8>::new(0),
        HeapAlloc::<u32>::new(0),
        HeapAlloc::<HuffmanCode>::new(HuffmanCode::default()),
    );

    let mut available_in = input.len();
    let mut input_offset = 0usize;
    let mut out = vec![0u8; 4096.min(max_out)];
    let mut output_offset = 0usize;
    let mut total_out = 0usize;

    loop {
        let mut available_out = out.len() - output_offset;
        let result = BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            input,
            &mut available_out,
            &mut output_offset,
            &mut out,
            &mut total_out,
            &mut state,
        );
        match result {
            BrotliResult::ResultSuccess => break,
            BrotliResult::NeedsMoreOutput => {
                if peek_gate && output_offset >= PEEK && !looks_textual(&out[..output_offset]) {
                    return Decoded::Binary;
                }
                // decompression-bomb guard: never grow past max_out
                if out.len() >= max_out { return Decoded::None; }
                let grown = (out.len() * 2).min(max_out);
                out.resize(grown, 0);
            }
            BrotliResult::NeedsMoreInput | BrotliResult::ResultFailure => return Decoded::None,
        }
    }

    out.truncate(output_offset);
    if peek_gate && !looks_textual(&out) { return Decoded::Binary; }
    Decoded::Asset(out, input_offset)
}

// A real web asset decompresses to printable text; binary assets and coincidental
// brotli-valid garbage do not. Only textual streams are recorded and have their
// interior skipped — a binary decode's `consumed` is unreliable (brotli over-reads
// past the true stream end) and would jump the walk clean over a real text asset
// that begins inside that bogus span.
fn looks_textual(bytes: &[u8]) -> bool {
    let n = bytes.len().min(PEEK);
    if n == 0 { return false; }
    let printable = bytes[..n].iter().filter(|b| matches!(b, 9 | 10 | 13 | 32..=126)).count();
    (printable as f32) / (n as f32) >= 0.85
}

// Every asset is stored as `<key path><brotli stream>`, so each `/`-led key path in the
// section enumerates a record. Each is decoded independently, which is the whole point:
// a record found this way cannot be hidden by a neighbouring stream reporting a bad
// length. Returns (assets, names whose stream did not decode).
//
// This used to enumerate the literal `/assets/` only, which left every root-level record
// (`/index.html`, `/vite.svg`, `/main.js`) with no independent path and reachable solely
// through the chain/scan walks — where one over-consuming decode loses the lot. Measured
// on a Vite Tauri build: a single stream at offset 4825011 decoded as 547169 consumed
// bytes, `walk_scan` stepped over the whole span, and index.html, tauri.svg and vite.svg
// (all inside it) were silently dropped while the `/assets/` files survived only because
// this pass found them on their own. Same bytes, extracted as a standalone slice:
// 8 of 8. Hence: give every key path the independent pass, not just `/assets/`.
fn walk_literal(data: &[u8], range: &std::ops::Range<usize>) -> (Vec<Asset>, Vec<String>) {
    let mut assets: Vec<Asset> = Vec::new();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut failed = Vec::new();
    for cands in literal_records(data, range) {
        let mut done = false;
        for (base, path, data_off) in &cands {
            // Two key runs can name the same stream (`/deep/x.js` is also found from its
            // inner `/x.js`); the first spelling wins and the record is not re-decoded.
            if seen.contains(data_off) { done = true; break; }
            if let Some((bytes, consumed)) = decode_record(data, *data_off, range) {
                seen.insert(*data_off);
                assets.push(Asset {
                    offset: *data_off,
                    compressed_len: consumed,
                    bytes,
                    name: Some(base.clone()),
                    path: Some(path.clone()),
                });
                done = true;
                break;
            }
        }
        if !done {
            if let Some((base, _, _)) = cands.first() { failed.push(base.clone()); }
        }
    }
    (assets, failed)
}

// Decode the stream sitting on a key. Two departures from the keyless walks, both earned
// by the key being strong evidence that a real record starts here:
//
// * WINDOW is a slice bound, not a format limit. A stream longer than it makes brotli
//   report NeedsMoreInput and the record is lost outright (a 6MB asset reproduces this).
//   Retry unbounded to the end of the range before giving up.
// * MIN_OUT is a false-positive filter for blind scanning. A keyed record does not need
//   it, and applying it drops legitimately tiny assets (a 1-byte file reproduces this).
fn decode_record(data: &[u8], off: usize, range: &std::ops::Range<usize>) -> Option<(Vec<u8>, usize)> {
    let windowed = (off + WINDOW).min(range.end);
    let ends = if windowed == range.end { &[windowed][..] } else { &[windowed, range.end][..] };
    for end in ends {
        if let Decoded::Asset(bytes, consumed) = decode(data, off, *end, MAX_OUT, false) {
            if consumed > 0 && !bytes.is_empty() { return Some((bytes, consumed)); }
        }
    }
    None
}

fn name_char(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
}

// Key paths hold directory separators, and filenames are not always ASCII: a non-ASCII
// byte ends the run under a plain `name_char`, so the anchor lands mid-name and the
// stream is never probed (`café-ñ.js` reproduces this). Accepting high bytes can instead
// over-run into the stream, which is exactly what the per-dot candidate list below
// already exists to absorb.
fn key_char(b: u8) -> bool { name_char(b) || b == b'/' || b >= 0x80 }

// Longest key run considered. A path this long is binary data, not a key.
const MAX_KEY: usize = 260;

// For each key path, the candidate (basename, path, stream offset) triples, best first.
// The record has no length field, so the name ends where the brotli stream begins and the
// stream's first byte may itself be a name char. At each `.` in the run, try every
// following end position and keep the ones whose extension `classify::kind_of_ext`
// recognises — one source of truth, so any kind the classifier writes is also a kind the
// scan enumerates. Prefer a candidate whose extension is followed by a non-key byte
// (clean), then the shortest such name; the decode picks the winner. (`x.js.map` beats
// `x.js` because `.js` ends on a `.`, a key byte, so it is not clean, not because it is
// longer.)
//
// A run can hold more than one key: adjacent keys pack with no separator between them
// (`/assets/classDiagram-v2-HSJHXN6E-BNMu6tuA.js/assets/sequenceDiagram-FGHM5R23-DsoRjOOc.js`
// appears verbatim in a release binary). Split the run wherever a dot's preferred name is
// followed by `/` — that is a new key path starting, not a directory inside this one — and
// emit a record per segment. Splitting on every dot instead would break `x.js.map`, whose
// `.js` is also a preferred-but-unclean name; there the next byte is `.`, not `/`.
fn literal_records(data: &[u8], range: &std::ops::Range<usize>) -> Vec<Vec<(String, String, usize)>> {
    let seg = &data[range.clone()];
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < seg.len() {
        if seg[i] != b'/' { i += 1; continue; }
        let start = i;
        let mut e = start + 1;
        while e < seg.len() && e - start < MAX_KEY && key_char(seg[e]) { e += 1; }

        let mut seg_start = start;
        let mut group: Vec<(String, String, usize, bool)> = Vec::new();
        for k in seg_start + 1..e {
            if k < seg_start || seg[k] != b'.' { continue; }
            let mut dot: Vec<(String, String, usize, bool)> = Vec::new();
            for end in k + 2..=e {
                if seg[end - 1] == b'.' { continue; }
                // Reject on the extension bytes before allocating anything: in binary
                // data almost every candidate dies here.
                let Ok(ext) = std::str::from_utf8(&seg[k + 1..end]) else { continue };
                if crate::classify::kind_of_ext(ext).is_none() { continue; }
                let path = String::from_utf8_lossy(&seg[seg_start..end]).into_owned();
                let base = path.rsplit('/').next().unwrap_or(&path).to_string();
                if base.is_empty() { continue; }
                let clean = end >= seg.len() || !key_char(seg[end]);
                dot.push((base, path, range.start + end, clean));
            }
            if dot.is_empty() { continue; }
            dot.sort_by_key(|(b, _, _, clean)| (!*clean, b.len()));
            // Preferred end of this dot's own name, in `seg` coordinates.
            let pref_end = dot[0].2 - range.start;
            group.extend(dot);
            if pref_end < e && seg[pref_end] == b'/' {
                group.sort_by_key(|(b, _, _, clean)| (!*clean, b.len()));
                out.push(std::mem::take(&mut group).into_iter().map(|(b, p, o, _)| (b, p, o)).collect());
                seg_start = pref_end;
            }
        }
        if !group.is_empty() {
            group.sort_by_key(|(b, _, _, clean)| (!*clean, b.len()));
            out.push(group.into_iter().map(|(b, p, o, _)| (b, p, o)).collect());
        }
        // Skip past the run: its interior `/`s are the same key, not new records.
        i = e.max(start + 1);
    }
    out
}

pub fn walk(data: &[u8], range: std::ops::Range<usize>) -> (Vec<Asset>, Vec<String>) {
    let (mut assets, failed) = walk_literal(data, &range);
    // The chain walk still earns its keep: it finds the keyless root `index.html`, which
    // has no `/assets/` record of its own. Merge it in, dropping anything it reports at an
    // offset the literal scan already owns or inside a stream the literal scan recovered
    // (that is the desynced walk re-reading another asset's interior).
    let mut spans: Vec<(usize, usize)> = assets.iter().map(|a| (a.offset, a.offset + a.compressed_len)).collect();
    spans.sort_unstable();
    let maxend = prefix_max_ends(&spans);
    for a in walk_chain(data, &range) {
        if !covered_by(&spans, &maxend, a.offset) { assets.push(a); }
    }
    assets.sort_by_key(|a| a.offset);
    (assets, failed)
}

// Prefix maximum of span ends: `maxend[i]` is the largest end among spans `0..=i`.
fn prefix_max_ends(spans: &[(usize, usize)]) -> Vec<usize> {
    let mut running = 0usize;
    spans.iter().map(|(_, e)| { running = running.max(*e); running }).collect()
}

// Is `offset` inside any literal-scan span? Spans can overlap (a record can over-consume;
// the desync run documented above is 396KB), so an offset may sit inside an *earlier,
// longer* span than its immediate predecessor. Test against the prefix-max of ends, not
// just `spans[i-1].1`. `spans` sorted by start; `maxend` from `prefix_max_ends`.
fn covered_by(spans: &[(usize, usize)], maxend: &[usize], offset: usize) -> bool {
    match spans.binary_search_by(|(s, _)| s.cmp(&offset)) {
        Ok(_) => true,
        Err(0) => false,
        Err(i) => offset < maxend[i - 1],
    }
}

fn walk_chain(data: &[u8], range: &std::ops::Range<usize>) -> Vec<Asset> {
    let range = range.clone();
    // A blind per-offset brotli scan of a real Tauri __const section is not viable:
    // it decode-attempts at ~25M offsets, and every offset landing inside a binary
    // asset (fonts/wasm) or padding pays a full ring-buffer allocation. Tauri embeds
    // each asset as `<key path string><brotli stream>`, so the compressed stream
    // begins exactly at the end of an ASCII key path. Seed decode attempts there:
    // a few hundred candidates instead of tens of millions. (Verified: main.css,
    // katex.css and the JS bundle all begin one byte past their `/assets/...` key.)
    //
    // Small or anchorless ranges (unit tests, non-Tauri inputs) fall back to the
    // exhaustive per-offset scan below.
    if range.end - range.start >= (1 << 20) {
        let assets = walk_anchored(data, &range);
        if !assets.is_empty() {
            return assets;
        }
        // no anchors decoded: fall through to the exhaustive scan rather than
        // silently returning nothing.
    }
    walk_scan(data, &range)
}

// Seed decodes at the byte just past each asset-key path string in `range`.
fn walk_anchored(data: &[u8], range: &std::ops::Range<usize>) -> Vec<Asset> {
    // Each anchor is (stream offset, recovered basename): the key path string IS the
    // original hashed filename, e.g. `/assets/main-A1b2C3d4.css` -> `main-A1b2C3d4.css`.
    let mut anchors: Vec<(usize, String)> = Vec::new();
    let seg = &data[range.clone()];
    let is_key = |b: u8| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'/' | b'-');
    let mut i = 0;
    while i < seg.len() {
        if seg[i] == b'/' {
            let mut j = i + 1;
            while j < seg.len() && is_key(seg[j]) {
                j += 1;
            }
            if j - i >= 3 {
                // basename = part of the key path after its last '/'. Always ASCII
                // (is_key), so from_utf8 cannot fail; empty when the key ends in '/'.
                let key = &seg[i..j];
                let base = key.rsplit(|&b| b == b'/').next().unwrap_or(key);
                let name = String::from_utf8_lossy(base).into_owned();
                anchors.push((range.start + j, name)); // stream starts one past the key
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }

    anchors.sort_by_key(|a| a.0);
    anchors.dedup_by_key(|a| a.0);

    // Decode at each anchor, then chain forward: streams are packed contiguously, and
    // some (index.html) sit right after another stream's end with no key of their own.
    // Once landed on a real start the `consumed` length is accurate, so stepping by it
    // stays in sync through the run until a byte that is not a stream (the next key
    // string) breaks the chain.
    let mut assets: Vec<Asset> = Vec::new();
    let mut covered_end = range.start;
    for (anchor, key_name) in &anchors {
        if *anchor < covered_end {
            continue; // anchor lies inside a stream already recovered
        }
        let mut off = *anchor;
        let mut first = true; // only the stream sitting on the key carries its name
        while off < range.end {
            let end = (off + WINDOW).min(range.end);
            match decode(data, off, end, MAX_OUT, false) {
                Decoded::Asset(bytes, consumed) if consumed > 0 && bytes.len() >= MIN_OUT => {
                    covered_end = off + consumed;
                    let name = if first && !key_name.is_empty() { Some(key_name.clone()) } else { None };
                    assets.push(Asset { offset: off, compressed_len: consumed, bytes, name, path: None });
                    off += consumed;
                    first = false;
                }
                // Below MIN_OUT is still a real record boundary with a trustworthy length.
                // Breaking here ended the chain and lost every asset packed behind it; step
                // over instead and keep walking. (`walk_literal` records the tiny asset.)
                Decoded::Asset(_, consumed) if consumed > 0 => {
                    covered_end = off + consumed;
                    off += consumed;
                    first = false;
                }
                _ => break,
            }
        }
    }
    assets.sort_by_key(|a| a.offset);
    assets
}

// Exhaustive fallback: decode at every offset, keep textual streams and skip their
// interior; +1 past everything else. A binary decode's `consumed` is not trusted to
// advance (brotli over-reads past the true end and the bogus span swallows a real
// text asset starting inside it).
fn walk_scan(data: &[u8], range: &std::ops::Range<usize>) -> Vec<Asset> {
    let mut assets = Vec::new();
    let mut off = range.start;
    while off < range.end {
        match decode(data, off, range.end, MAX_OUT, true) {
            Decoded::Asset(bytes, consumed) if consumed > 0 && bytes.len() >= MIN_OUT => {
                assets.push(Asset { offset: off, compressed_len: consumed, bytes, name: None, path: None });
                off += consumed;
            }
            _ => off += 1,
        }
    }
    assets
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn br_compress(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
            w.write_all(input).unwrap();
        }
        out
    }

    fn record(key: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut data = Vec::from(key);
        data.extend_from_slice(&br_compress(payload));
        data
    }

    // The regression this whole pass exists for: a root-level record has no `/assets/`
    // prefix, and enumerating only that prefix left it reachable solely through the
    // chain/scan walks, where one over-consuming neighbour loses it silently.
    // Mutation check: restore the `/assets/` literal and this fails.
    #[test]
    fn enumerates_a_root_level_key_not_just_assets_paths() {
        let payload = b"<!doctype html><title>x</title>".repeat(3);
        let data = record(b"/index.html", &payload);
        let (assets, _) = walk_literal(&data, &(0..data.len()));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].name.as_deref(), Some("index.html"));
        assert_eq!(assets[0].path.as_deref(), Some("/index.html"));
        assert_eq!(assets[0].bytes, payload);
    }

    // Two keys pack with no separator when the first one's stream is absent. The second
    // must come out under its own path, not the concatenation of both.
    // Mutation check: drop the `seg[pref_end] == b'/'` split and the path becomes
    // `/assets/x.js/assets/y.js`.
    #[test]
    fn splits_a_run_holding_two_packed_keys() {
        let payload = b"export const y = 1;".repeat(3);
        let data = record(b"/assets/x.js/assets/y.js", &payload);
        let (assets, failed) = walk_literal(&data, &(0..data.len()));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].path.as_deref(), Some("/assets/y.js"));
        assert_eq!(assets[0].name.as_deref(), Some("y.js"));
        assert!(failed.contains(&"x.js".to_string()), "the key with no stream is reported");
    }

    // The split must trigger on `/` only: `.js.map`'s inner `.js` is also a preferred
    // unclean name, but it is one key, not two.
    #[test]
    fn does_not_split_a_map_name_at_its_inner_dot() {
        let payload = b"{\"version\":3,\"sources\":[]}".repeat(3);
        let data = record(b"/assets/x.js.map", &payload);
        let (assets, _) = walk_literal(&data, &(0..data.len()));
        assert_eq!(assets[0].name.as_deref(), Some("x.js.map"));
    }

    // A non-ASCII byte used to end the key run, so the anchor landed mid-name and the
    // stream was never probed. Mutation check: drop `b >= 0x80` from `key_char`.
    #[test]
    fn recovers_a_record_whose_name_is_not_ascii() {
        let payload = b"export const MARKER = 1;".repeat(3);
        let data = record("/assets/café-ñ.js".as_bytes(), &payload);
        let (assets, _) = walk_literal(&data, &(0..data.len()));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].name.as_deref(), Some("café-ñ.js"));
    }

    // MIN_OUT is a false-positive filter for blind scanning; a keyed record does not need
    // it and applying it drops legitimately tiny assets.
    // Mutation check: reinstate `bytes.len() >= MIN_OUT` in `decode_record`.
    #[test]
    fn recovers_a_keyed_record_smaller_than_the_min_out_floor() {
        let data = record(b"/data/tiny.json", b"1");
        let (assets, _) = walk_literal(&data, &(0..data.len()));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].bytes, b"1");
    }

    // WINDOW bounds the probe slice, not the format. A stream longer than it makes brotli
    // report NeedsMoreInput, and only the unbounded retry can finish it.
    // Mutation check: drop `range.end` from `decode_record`'s end list.
    #[test]
    fn recovers_a_keyed_record_longer_than_the_probe_window() {
        // Incompressible, so the compressed stream really does exceed WINDOW.
        let mut payload = vec![0u8; WINDOW + (1 << 19)];
        let mut x: u32 = 0x1234_5678;
        for b in payload.iter_mut() {
            x ^= x << 13; x ^= x >> 17; x ^= x << 5;
            *b = x as u8;
        }
        let mut data = Vec::from(&b"/data/big.json"[..]);
        {
            let mut w = brotli::CompressorWriter::new(&mut data, 4096, 1, 22);
            w.write_all(&payload).unwrap();
        }
        assert!(data.len() - 14 > WINDOW, "payload must not compress below the window");
        let (assets, _) = walk_literal(&data, &(0..data.len()));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].bytes.len(), payload.len());
    }

    #[test]
    fn decodes_stream_and_reports_consumed_length_ignoring_trailing_garbage() {
        let payload = b":root{--theme-background:#0f0f0f}".repeat(20);
        let stream = br_compress(&payload);
        let clen = stream.len();
        // embed: [pad][stream][trailing garbage]
        let mut data = vec![0xAB; 100];
        let start = data.len();
        data.extend_from_slice(&stream);
        data.extend_from_slice(&[0xCD; 500]); // trailing garbage
        let (out, consumed) = try_decode_at(&data, start).expect("should decode at stream start");
        assert_eq!(out, payload);
        assert_eq!(consumed, clen, "consumed must equal the true compressed length, not the window");
    }

    #[test]
    fn caps_decompressed_output_to_guard_against_decompression_bombs() {
        let payload = b"0123456789".repeat(10); // 100 bytes decompressed
        let stream = br_compress(&payload);
        // a cap smaller than the decompressed size must reject the stream, not allocate past it
        assert!(
            try_decode_at_capped(&stream, 0, 16).is_none(),
            "a stream whose decompressed size exceeds the cap must be rejected"
        );
        // the same stream under a generous cap must still decode fine
        let (out, _) = try_decode_at_capped(&stream, 0, MAX_OUT).expect("should decode under a generous cap");
        assert_eq!(out, payload);
    }

    #[test]
    fn returns_none_at_a_non_stream_offset() {
        let data = vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        assert!(try_decode_at(&data, 0).is_none());
    }

    // Large, multi-output-block stream: the tiny single-block fixtures above never
    // exercised the buffer-grow loop (a decoder that mishandled the second output
    // block, or returned the window length as `consumed`, passed them all). 200KB+ of
    // varied text compresses to many blocks. Mutation check: hard-code `consumed` to
    // the window length, or truncate `out` after the first block, and this fails.
    #[test]
    fn decodes_large_multiblock_stream_with_true_consumed_length() {
        let mut payload = Vec::new();
        for i in 0..8000 {
            payload.extend_from_slice(
                format!("line {i}: the quick brown fox jumps; k{i}=value-{};\n", i * 7).as_bytes(),
            );
        }
        assert!(payload.len() > 200_000);
        let stream = br_compress(&payload);
        let clen = stream.len();
        assert!(clen < payload.len(), "text must actually compress");

        let mut data = vec![0u8; 64];
        let start = data.len();
        data.extend_from_slice(&stream);
        data.extend_from_slice(&[0xEE; 300]); // trailing garbage past the stream
        let (out, consumed) = try_decode_at(&data, start).expect("large stream should decode");
        assert_eq!(out, payload, "decoded bytes must match the full multi-block payload");
        assert_eq!(consumed, clen, "consumed must be the true compressed length, not the window");
    }

    // On a large range, walk seeds at asset-key anchors and forward-chains through
    // contiguous streams. The keyed `.css` is found via its anchor; the `.html` that
    // follows it with no key of its own is found only by chaining. Mutation check:
    // drop the chaining loop and the html assertion fails.
    #[test]
    fn walk_anchors_key_and_chains_contiguous_stream() {
        let css = br_compress(b".a{color:red;margin:0;padding:0}");
        let html = br_compress(b"<!doctype html><html></html>");
        let mut data = vec![0u8; 1 << 20]; // 1MB, forces the anchored path
        let base = 500_000;
        let key = b"/assets/app-ABCDEF12.css";
        data[base..base + key.len()].copy_from_slice(key);
        let s1 = base + key.len();
        data[s1..s1 + css.len()].copy_from_slice(&css);
        let s2 = s1 + css.len();
        data[s2..s2 + html.len()].copy_from_slice(&html);

        let (assets, _) = walk(&data, 0..data.len());
        assert!(
            assets.iter().any(|a| a.offset == s1 && a.bytes.starts_with(b".a{")),
            "keyed css must be found at its anchor"
        );
        assert!(
            assets.iter().any(|a| a.offset == s2 && a.bytes.starts_with(b"<!doctype")),
            "contiguous unkeyed html must be found by forward-chaining"
        );
    }

    // The stream sitting on a key recovers that key's basename; the chained stream that
    // follows it with no key of its own recovers None. Mutation check: drop the `first`
    // gate (name every chained asset) and the html-is-None assertion fails; discard the
    // key basename and the css-name assertion fails.
    #[test]
    fn walk_recovers_key_basename_and_leaves_chained_and_keyless_none() {
        let css = br_compress(b".a{color:red;margin:0;padding:0}");
        let html = br_compress(b"<!doctype html><html></html>");
        let mut data = vec![0u8; 1 << 20]; // 1MB, forces the anchored path
        let base = 500_000;
        let key = b"/assets/main-A1b2C3d4.css";
        data[base..base + key.len()].copy_from_slice(key);
        let s1 = base + key.len();
        data[s1..s1 + css.len()].copy_from_slice(&css);
        let s2 = s1 + css.len();
        data[s2..s2 + html.len()].copy_from_slice(&html);

        let (assets, _) = walk(&data, 0..data.len());
        let keyed = assets.iter().find(|a| a.offset == s1).expect("keyed css found");
        assert_eq!(keyed.name.as_deref(), Some("main-A1b2C3d4.css"), "basename recovered off the key");
        let chained = assets.iter().find(|a| a.offset == s2).expect("chained html found");
        assert_eq!(chained.name, None, "the keyless chained stream carries no recovered name");
    }

    // A keyless input (exhaustive scan path) yields assets with no recovered name; output
    // then falls back to the NNN scheme.
    #[test]
    fn walk_scan_yields_no_recovered_names() {
        let a = br_compress(b"AAAAAAAAAAAAAAAAAAAAAAAA");
        let (assets, _) = walk(&a, 0..a.len());
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].name, None);
    }

    // The `/assets/` literal scan recovers a record the chain walk cannot reach. Here the
    // stream's first byte is `b`, a filename char, so the walk's key run swallows it and
    // its decode starts one byte late — one of the two desync modes measured on a real Tauri app
    // 1.1.2. Mutation check: drop `walk_literal` from `walk` and this fails; the
    // walk_chain precondition assert keeps it from passing vacuously.
    #[test]
    fn literal_scan_recovers_a_record_the_chain_walk_misses() {
        let payload = b"const x=1;export default x;".repeat(20);
        let mut polluted = Vec::new();
        {
            // q=5/lgwin=16 frames a stream starting 0x62 ('b'); q=5/lgwin=22 does not.
            let mut w = brotli::CompressorWriter::new(&mut polluted, 4096, 5, 16);
            w.write_all(&payload).unwrap();
        }
        assert!(name_char(polluted[0]), "fixture precondition: stream must start with a filename char");
        let plain = br_compress(b".a{color:red;margin:0;padding:0}");

        let mut data = vec![0u8; 1 << 20]; // 1MB, forces the anchored path
        let ok_key = b"/assets/app-ABCDEF12.css";
        let ok_off = 300_000 + ok_key.len();
        data[300_000..ok_off].copy_from_slice(ok_key);
        data[ok_off..ok_off + plain.len()].copy_from_slice(&plain);
        let bad_key = b"/assets/panel-E5f6G7h8.js";
        let bad_off = 500_000 + bad_key.len();
        data[500_000..bad_off].copy_from_slice(bad_key);
        data[bad_off..bad_off + polluted.len()].copy_from_slice(&polluted);

        let chained = walk_chain(&data, &(0..data.len()));
        assert!(chained.iter().any(|a| a.offset == ok_off), "precondition: the chain walk finds the clean record");
        assert!(!chained.iter().any(|a| a.offset == bad_off), "precondition: the chain walk misses the polluted record");

        let (assets, failed) = walk(&data, 0..data.len());
        let found = assets.iter().find(|a| a.offset == bad_off).expect("literal scan must find the missed record");
        assert_eq!(found.name.as_deref(), Some("panel-E5f6G7h8.js"), "name comes from the record, not the anchor run");
        assert_eq!(found.bytes, payload);
        assert!(failed.is_empty(), "both records decoded: {failed:?}");
        assert_eq!(assets.iter().filter(|a| a.offset == ok_off).count(), 1, "the merge must not duplicate the shared record");
    }

    // An offset inside an earlier, longer span (not the immediately-preceding one) is
    // covered. Mutation check: replace `maxend[i - 1]` with `spans[i - 1].1` and the
    // 21850000 assertion fails (it sits inside span 0 but after span 1's start).
    #[test]
    fn interior_coverage_spans_an_earlier_longer_span() {
        let spans = [(21737038usize, 22132938usize), (21800000, 21800500)];
        let maxend = prefix_max_ends(&spans);
        assert_eq!(maxend, vec![22132938, 22132938]);
        assert!(covered_by(&spans, &maxend, 21850000), "inside the first, longer span");
        assert!(covered_by(&spans, &maxend, 21800200), "inside the short second span");
        assert!(!covered_by(&spans, &maxend, 22132938), "one past the far end is not covered");
        assert!(!covered_by(&spans, &maxend, 21000000), "before every span is not covered");
    }

    // A stream whose *compressed* length exceeds the old 1MB window must still decode.
    // Mutation check: set WINDOW back to `1 << 20` and this fails (the >1MB stream returns
    // NeedsMoreInput inside the too-small input slice). ~1.5MB of high-entropy bytes barely
    // compresses, so its compressed length clears 1MB.
    #[test]
    fn recovers_a_stream_compressed_larger_than_one_megabyte() {
        let mut payload = Vec::with_capacity(1_600_000);
        let (mut a, mut b): (u32, u32) = (0x9E3779B9, 0x1234_5678); // xorshift-ish, incompressible
        while payload.len() < 1_600_000 {
            a ^= a << 13; a ^= a >> 17; a ^= a << 5;
            b = b.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            payload.extend_from_slice(&(a ^ b).to_le_bytes());
        }
        let stream = br_compress(&payload);
        assert!(stream.len() > (1 << 20), "compressed must exceed 1MB, got {}", stream.len());
        assert!(stream.len() < (4 << 20), "and stay under the 4MB window, got {}", stream.len());

        let mut data = vec![0u8; 32];
        let start = data.len();
        data.extend_from_slice(&stream);
        data.extend_from_slice(&[0xEE; 256]); // trailing garbage past the stream
        let (out, consumed) = try_decode_at(&data, start).expect(">1MB stream must decode");
        assert_eq!(out, payload);
        assert_eq!(consumed, stream.len(), "true compressed length, not the window");
    }

    #[test]
    fn walk_finds_two_contiguous_streams() {
        let a = br_compress(b"AAAAAAAAAAAAAAAAAAAAAAAA");
        let b = br_compress(b"BBBBBBBBBBBBBBBBBBBBBBBB");
        let mut data = Vec::new();
        data.extend_from_slice(&a);
        data.extend_from_slice(&b);
        let (assets, _) = walk(&data, 0..data.len());
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].bytes, b"AAAAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(assets[1].bytes, b"BBBBBBBBBBBBBBBBBBBBBBBB");
        assert_eq!(assets[1].offset, a.len(), "second stream must start right after the first");
    }
}

