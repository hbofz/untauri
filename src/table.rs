// Read Tauri's own asset table instead of guessing.
//
// tauri-codegen embeds the frontend as `phf::Map<&'static str, &'static [u8]>` (see
// tauri-utils `EmbeddedAssets`). The map's `entries` field is a contiguous array of
// `(&str, &[u8])`, i.e. four u64s per asset: name_ptr, name_len, data_ptr, data_len.
// The key is the asset path (`/assets/main-A1b2C3d4.js`), the value the stored bytes:
// a brotli stream, or the raw file when the app was built without Tauri's
// `compression` feature. Reading the array gives every asset's exact name, offset and
// length, so there is nothing to guess and no binary noise to mistake for an asset.
//
// Pointers in a position-independent binary are not plain addresses in the file:
// - ELF: the slot holds 0 (RELA) or the link-time address (RELR); the address lives in
//   an R_*_RELATIVE relocation addend in the first case, in the slot in the second.
// - Mach-O: the slot holds a chained-fixup rebase whose low 36 bits are the target,
//   either as a vmaddr (DYLD_CHAINED_PTR_64) or as an offset from the image base
//   (DYLD_CHAINED_PTR_64_OFFSET). Pre-fixup binaries hold the plain vmaddr.
// - PE: a plain virtual address (base relocations only adjust it at load time).

use crate::scan::{decode_exact, Asset};
use object::{Object, ObjectSection, ObjectSegment};
use std::collections::HashMap;

const ENTRY: usize = 32;
const MAX_NAME: usize = 4096;

pub struct Table {
    pub assets: Vec<Asset>,
    // Keys whose stream claimed to be brotli but would not decode.
    pub undecoded: Vec<String>,
    pub compressed: bool,
    // Asset key -> file offset of its entry in the `entries` array, for `patch`.
    pub slots: HashMap<String, usize>,
}

struct Image<'a> {
    data: &'a [u8],
    // (vmaddr, file offset, file size)
    segs: Vec<(u64, u64, u64)>,
    // file offset of a pointer slot -> target vmaddr (ELF RELA)
    rel: HashMap<usize, u64>,
    // Mach-O image base, for chained-fixup targets stored as base-relative offsets.
    base: Option<u64>,
}

impl Image<'_> {
    fn off(&self, va: u64) -> Option<usize> {
        self.segs.iter()
            .find(|(a, _, sz)| va >= *a && va - a < *sz)
            .map(|(a, fo, _)| (va - a + fo) as usize)
    }
    fn u64_at(&self, o: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.data.get(o..o + 8)?.try_into().ok()?))
    }
    // File offset a pointer slot points at.
    fn deref(&self, slot: usize) -> Option<usize> {
        if let Some(&va) = self.rel.get(&slot) { return self.off(va); }
        let v = self.u64_at(slot)?;
        if v == 0 { return None; }
        if let Some(o) = self.off(v) { return Some(o); }
        // Mach-O chained-fixup rebase (bind bit 63 clear): low 36 bits are the target.
        if v >> 63 == 0 {
            let t = v & ((1 << 36) - 1);
            return self.off(t).or_else(|| self.off(t + self.base?));
        }
        None
    }
    fn entry(&self, o: usize) -> Option<(String, usize, usize)> {
        let name_len = self.u64_at(o + 8)? as usize;
        let data_len = self.u64_at(o + 24)? as usize;
        if name_len == 0 || name_len > MAX_NAME || data_len == 0 { return None; }
        let no = self.deref(o)?;
        let dof = self.deref(o + 16)?;
        let name = self.data.get(no..no.checked_add(name_len)?)?;
        self.data.get(dof..dof.checked_add(data_len)?)?;
        // Tauri keys are root-absolute paths. Any printable UTF-8 is allowed (a file
        // named `my logo.png` or `café.svg` is a real asset), control bytes are not.
        let name = std::str::from_utf8(name).ok()?;
        if !name.starts_with('/') || name.chars().any(|c| c.is_control()) { return None; }
        Some((name.to_string(), dof, data_len))
    }
}

fn image(data: &[u8]) -> Option<Image<'_>> {
    let obj = object::File::parse(data).ok()?;
    let segs: Vec<(u64, u64, u64)> = obj.segments()
        .filter_map(|s| { let (fo, fs) = s.file_range(); (fs > 0).then(|| (s.address(), fo, fs)) })
        .collect();
    let mut img = Image { data, segs, rel: HashMap::new(), base: None };
    if let object::File::MachO64(_) | object::File::MachO32(_) = obj {
        img.base = obj.segments().find(|s| s.name().ok().flatten() == Some("__TEXT")).map(|s| s.address());
    }
    if let Some(relocs) = obj.dynamic_relocations() {
        for (va, r) in relocs {
            // R_X86_64_RELATIVE = 8, R_AARCH64_RELATIVE = 1027
            let relative = r.kind() == object::RelocationKind::Relative
                || matches!(r.flags(), object::RelocationFlags::Elf { r_type: 8 | 1027 });
            if relative {
                if let Some(o) = img.off(va) { img.rel.insert(o, r.addend() as u64); }
            }
        }
    }
    Some(img)
}

// Every 8-aligned slot that could start an entry: the relocated slots of an ELF, plus a
// sweep of the data sections (covers RELR, Mach-O and PE, whose pointers sit in place).
fn candidate_slots(data: &[u8], img: &Image) -> Vec<usize> {
    let mut slots: Vec<usize> = img.rel.keys().copied().collect();
    if let Ok(obj) = object::File::parse(data) {
        for s in obj.sections() {
            let name = s.name().unwrap_or("");
            let seg = s.segment_name().ok().flatten().unwrap_or("");
            let wanted = name.contains("rel.ro") || name == ".rodata" || name == ".data" || name == ".rdata"
                || seg.starts_with("__DATA");
            if !wanted { continue; }
            if let Some((fo, sz)) = s.file_range() {
                let start = (fo as usize + 7) & !7;
                slots.extend((start..(fo + sz) as usize).step_by(8));
            }
        }
    }
    slots.sort_unstable();
    slots.dedup();
    slots
}

// Runs of back-to-back entries: each is one `entries` array.
fn runs(img: &Image, slots: &[usize]) -> Vec<Vec<(String, usize, usize, usize)>> {
    let mut covered = std::collections::HashSet::new();
    let mut out = Vec::new();
    for &s in slots {
        if covered.contains(&s) { continue; }
        let mut run = Vec::new();
        let mut o = s;
        while let Some((k, d, l)) = img.entry(o) { covered.insert(o); run.push((k, d, l, o)); o += ENTRY; }
        if !run.is_empty() { out.push(run); }
    }
    out
}

fn web_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.ends_with(".html") || k.ends_with(".js") || k.ends_with(".mjs") || k.ends_with(".css")
}

pub fn read(data: &[u8]) -> Option<Table> { from_image(data, &image(data)?) }

fn from_image(data: &[u8], img: &Image) -> Option<Table> {
    let runs = runs(img, &candidate_slots(data, img));
    // Other `(&str, &[u8])`-shaped arrays exist (CSP hash maps, lists of ssl paths).
    // The asset table is the run whose values are brotli streams; failing that (an app
    // built without compression), the largest run that holds web files.
    // Decode each run once; the run with the most streams that decode is the table.
    let decoded: Vec<Vec<Option<Vec<u8>>>> = runs.iter()
        .map(|r| r.iter().map(|(_, o, l, _)| decode_exact(data, *o, *l)).collect())
        .collect();
    let best = (0..runs.len())
        .map(|i| (decoded[i].iter().filter(|d| d.is_some()).count(), runs[i].len(), i))
        .filter(|(n, _, _)| *n > 0)
        .max();
    let (compressed, i) = match best {
        Some((_, _, i)) => (true, i),
        None => (false, (0..runs.len()).filter(|&i| runs[i].iter().any(|(k, ..)| web_key(k))).max_by_key(|&i| runs[i].len())?),
    };

    let mut assets = Vec::new();
    let mut undecoded = Vec::new();
    let slots = runs[i].iter().map(|(k, _, _, s)| (k.clone(), *s)).collect();
    for ((key, off, len, _), dec) in runs[i].iter().zip(&decoded[i]) {
        let bytes = match (compressed, dec) {
            (true, Some(b)) => b.clone(),
            (true, None) => { undecoded.push(key.clone()); continue; }
            (false, _) => data[*off..*off + len].to_vec(),
        };
        let name = key.rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_string);
        assets.push(Asset { offset: *off, compressed_len: *len, bytes, name, path: Some(key.clone()) });
    }
    Some(Table { assets, undecoded, compressed, slots })
}

// Replace an asset's stored bytes in place: write `stored` over its old bytes, zero the
// rest of the old span, and set data_len in every entry in `slots` (the linker stores
// identical files once, so several keys can share one span). Only the length changes in
// an entry; both pointers stay valid, so no relocation or fixup needs touching. Lengths are
// plain integers in every format. Err(bytes over) when `stored` is larger than the span.
pub fn patch(data: &mut [u8], asset: &Asset, slots: &[usize], stored: &[u8]) -> Result<(), usize> {
    if stored.len() > asset.compressed_len { return Err(stored.len() - asset.compressed_len); }
    let span = &mut data[asset.offset..asset.offset + asset.compressed_len];
    span[..stored.len()].copy_from_slice(stored);
    span[stored.len()..].fill(0);
    for s in slots { data[s + 24..s + 32].copy_from_slice(&(stored.len() as u64).to_le_bytes()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn br(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        { let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22); w.write_all(input).unwrap(); }
        out
    }

    // A non-PIE ELF whose .rodata holds the keys and streams and whose .data.rel.ro holds
    // the entries array with plain addresses, the layout `read` must decode.
    fn elf(files: &[(&str, Vec<u8>)], noise: &[u8]) -> Vec<u8> {
        use object::write::{Object as W, StreamingBuffer};
        use object::{Architecture, BinaryFormat, Endianness, SectionKind};
        // Lay out .rodata first to learn each key/stream offset, then point entries at it.
        const RO_VA: u64 = 0x10000;
        let mut ro = noise.to_vec();
        let mut entries = Vec::new();
        for (k, v) in files {
            let ko = ro.len() as u64; ro.extend_from_slice(k.as_bytes());
            let vo = ro.len() as u64; ro.extend_from_slice(v);
            for x in [RO_VA + ko, k.len() as u64, RO_VA + vo, v.len() as u64] { entries.extend_from_slice(&x.to_le_bytes()); }
        }
        let mut w = W::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let r = w.add_section(vec![], b".rodata".to_vec(), SectionKind::ReadOnlyData);
        w.append_section_data(r, &ro, 1);
        let d = w.add_section(vec![], b".data.rel.ro".to_vec(), SectionKind::ReadOnlyDataWithRel);
        w.append_section_data(d, &entries, 8);
        let mut out = Vec::new();
        w.emit(&mut StreamingBuffer::new(&mut out)).unwrap();
        out
    }

    // A relocatable object has no segments, so map .rodata at the address `elf` used.
    fn read_obj(data: &[u8]) -> Option<Table> {
        let obj = object::File::parse(data).ok()?;
        let mut img = image(data)?;
        let ro = obj.section_by_name(".rodata")?.file_range()?;
        img.segs = vec![(0x10000, ro.0, ro.1)];
        from_image(data, &img)
    }

    // Mutation check: break the 32-byte stride or the pointer resolution and the keys
    // come back short or wrong; drop the exact-length decode and the 9MB stream is lost.
    #[test]
    fn reads_every_entry_with_its_real_key() {
        let files = vec![
            ("/index.html", br(b"<!doctype html><script src=/assets/a-AAAA1111.js></script>")),
            ("/assets/a-AAAA1111.js", br(b"console.log('a')")),
            ("/assets/big video.webm", br(&vec![7u8; 9 << 20])), // over the old 4MB window
        ];
        let t = read_obj(&elf(&files, b"\x00noise\x00")).expect("table");
        let keys: Vec<_> = t.assets.iter().map(|a| a.path.clone().unwrap()).collect();
        assert_eq!(keys, vec!["/index.html", "/assets/a-AAAA1111.js", "/assets/big video.webm"]);
        assert_eq!(t.assets[1].bytes, b"console.log('a')");
        assert_eq!(t.assets[2].bytes.len(), 9 << 20);
        assert_eq!(t.assets[2].name.as_deref(), Some("big video.webm"));
    }

    // Mutation check: skip the data_len write and the shorter stream fails to decode
    // exactly; drop the size guard and the oversized patch tramples the next asset.
    #[test]
    fn patch_rewrites_one_asset_in_place() {
        let files = vec![
            ("/index.html", br(b"<!doctype html><title>old title, quite long</title>")),
            ("/assets/a.js", br(b"console.log('untouched')")),
        ];
        let mut data = elf(&files, b"");
        let t = read_obj(&data).unwrap();
        let new = br(b"<!doctype html><title>new</title>");
        patch(&mut data, &t.assets[0], &[t.slots["/index.html"]], &new).unwrap();
        let t2 = read_obj(&data).unwrap();
        assert_eq!(t2.assets[0].bytes, b"<!doctype html><title>new</title>");
        assert_eq!(t2.assets[1].bytes, b"console.log('untouched')");
        let big = vec![b'x'; t.assets[0].compressed_len + 3];
        assert_eq!(patch(&mut data, &t2.assets[0], &[t2.slots["/index.html"]], &big), Err(big.len() - t2.assets[0].compressed_len));
    }

    // One entry at offset 0 naming `key`, pointing its data at 4 bytes past the key.
    fn one_entry(key: &[u8], data_len: u64) -> Vec<u8> {
        let mut d = vec![0u8; 32];
        let (ko, vo) = (32u64, 32 + key.len() as u64);
        for (i, x) in [ko, key.len() as u64, vo, data_len].iter().enumerate() { d[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes()); }
        d.extend_from_slice(key);
        d.extend_from_slice(b"DATA");
        d
    }

    // Mutation check: drop the `starts_with('/')`, control-char or in-bounds data test
    // and the matching bad entry below is accepted.
    #[test]
    fn rejects_entries_that_are_not_asset_keys() {
        let check = |key: &[u8], len: u64| {
            let d = one_entry(key, len);
            let img = Image { data: &d, segs: vec![(0, 0, d.len() as u64)], rel: HashMap::new(), base: None };
            img.entry(0).map(|e| e.0)
        };
        assert_eq!(check(b"/assets/my logo.png", 4).as_deref(), Some("/assets/my logo.png"));
        assert_eq!(check(b"assets/x.js", 4), None, "keys are root-absolute");
        assert_eq!(check(b"/assets/\x01x.js", 4), None, "no control bytes");
        assert_eq!(check(b"/assets/x.js", 5), None, "data must lie inside the file");
    }

    #[test]
    fn chained_fixup_targets_resolve() {
        // target as a base-relative offset (DYLD_CHAINED_PTR_64_OFFSET) with `next` bits set
        let mut data = vec![0u8; 64];
        let slot: u64 = (5 << 51) | 0x20;
        data[0..8].copy_from_slice(&slot.to_le_bytes());
        let img = Image { data: &data, segs: vec![(0x1_0000_0000, 0, 64)], rel: HashMap::new(), base: Some(0x1_0000_0000) };
        assert_eq!(img.deref(0), Some(0x20));
    }
}
