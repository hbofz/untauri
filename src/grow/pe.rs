// Windows (PE32+): a new `.untauri` section at the end of the file. The section table has
// room for it: the headers are padded to FileAlignment, and real Tauri executables leave
// space for five or six more entries. A table entry's pointer is a plain virtual address
// with a base relocation already on it (that is how it worked before), so repointing is
// writing the new address.
//
// An Authenticode signature sits at the very end of the file, where the section goes.
// It only covers the bytes it signed, so patching breaks it anyway: it is removed and
// the security directory cleared. Data after the last section that is not a signature
// (an installer's appended payload) is refused rather than buried under the new section.
//
// A later repack finds `.untauri` by name; it is the last section, so it is extended in
// place.

const NAME: &[u8; 8] = b".untauri";
// IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ
const READ_DATA: u32 = 0x4000_0040;
const SECURITY_DIR: usize = 4;

fn u16_at(d: &[u8], o: usize) -> u16 { u16::from_le_bytes(d[o..o + 2].try_into().unwrap()) }
fn u32_at(d: &[u8], o: usize) -> u32 { u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) }
fn u64_at(d: &[u8], o: usize) -> u64 { u64::from_le_bytes(d[o..o + 8].try_into().unwrap()) }
fn put_u32(d: &mut [u8], o: usize, v: u32) { d[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
fn put_u64(d: &mut [u8], o: usize, v: u64) { d[o..o + 8].copy_from_slice(&v.to_le_bytes()); }

struct Pe { opt: usize, nsec: usize, table: usize, file_align: u64, sect_align: u64, image_base: u64, headers: usize }

#[derive(Clone, Copy)]
struct Sec { name: [u8; 8], vsize: u64, va: u64, raw_size: u64, raw_ptr: u64 }

fn header(d: &[u8]) -> Option<Pe> {
    let pe = u32_at(d.get(..0x40)?, 0x3c) as usize;
    if d.get(pe..pe + 4)? != b"PE\0\0" { return None; }
    let opt = pe + 24;
    let optsz = u16_at(d.get(pe + 20..pe + 22)?, 0) as usize;
    if optsz < 112 + 16 * 8 || u16_at(d.get(opt..opt + 2)?, 0) != 0x20b { return None; }
    let nsec = u16_at(d, pe + 6) as usize;
    let table = opt + optsz;
    d.get(opt..table.checked_add(nsec * 40)?)?;
    Some(Pe { opt, nsec, table, image_base: u64_at(d, opt + 24), sect_align: u32_at(d, opt + 32) as u64,
              file_align: u32_at(d, opt + 36) as u64, headers: u32_at(d, opt + 60) as usize })
}

pub fn supported(d: &[u8]) -> bool { d.starts_with(b"MZ") && header(d).is_some() }

fn sections(d: &[u8], p: &Pe) -> Vec<Sec> {
    (0..p.nsec).map(|i| {
        let o = p.table + i * 40;
        Sec { name: d[o..o + 8].try_into().unwrap(), vsize: u32_at(d, o + 8) as u64, va: u32_at(d, o + 12) as u64,
              raw_size: u32_at(d, o + 16) as u64, raw_ptr: u32_at(d, o + 20) as u64 }
    }).collect()
}

fn round_up(v: u64, a: u64) -> u64 { v.next_multiple_of(a.max(1)) }

// The PE checksum: 16-bit one's-complement-style sum of the file (skipping the field
// itself) plus its length. Only drivers are checked by the loader, but a stale value
// is worse than a right one.
fn checksum(d: &[u8], field: usize) -> u32 {
    let mut sum: u64 = 0;
    for (i, w) in d.chunks(2).enumerate() {
        if i * 2 == field || i * 2 == field + 2 { continue; }
        sum += w[0] as u64 | (*w.get(1).unwrap_or(&0) as u64) << 8;
        sum = (sum & 0xffff) + (sum >> 16);
    }
    (sum as u32 & 0xffff).wrapping_add(d.len() as u32)
}

pub fn append(d: &mut Vec<u8>, blobs: &[Vec<u8>]) -> Result<Vec<u64>, String> {
    let p = header(d).ok_or("not a 64-bit Windows executable")?;
    let bad = || "the executable's headers are inconsistent".to_string();
    if !p.file_align.is_power_of_two() || !p.sect_align.is_power_of_two() || p.sect_align > 1 << 20 { return Err(bad()); }

    // Drop the signature: it would no longer verify, and it sits where the section goes.
    let dir = p.opt + 112 + SECURITY_DIR * 8;
    let (cert_at, cert_len) = (u32_at(d, dir) as usize, u32_at(d, dir + 4) as usize);
    if cert_len > 0 {
        let end = cert_at.checked_add(cert_len).ok_or_else(bad)?;
        // The table is padded to 8 bytes; anything else after it is unknown data.
        if cert_at > d.len() || end > d.len() || d.len() - end >= 8 { return Err("the signature is not at the end of the file, so repack can't make room for new data".into()); }
        d.truncate(cert_at);
        put_u64(d, dir, 0);
    }

    let mut secs = sections(d, &p);
    let raw_end = secs.iter().map(|s| s.raw_ptr + s.raw_size).max().unwrap_or(0);
    let (i, raw_ptr) = match secs.iter().position(|s| &s.name == NAME) {
        Some(i) => {
            let s = secs[i];
            let last = secs.iter().all(|t| t.va <= s.va) && s.raw_ptr + s.raw_size == d.len() as u64;
            if !last { return Err("the executable was changed after the last repack in a way repack can't follow; repack the original instead".into()); }
            d.truncate((s.raw_ptr + s.vsize) as usize);
            (i, s.raw_ptr)
        }
        None => {
            if d.len() as u64 != raw_end { return Err("the executable has data after its last section (an installer payload?), which repack won't bury".into()); }
            let entry = p.table + p.nsec * 40;
            let first_raw = secs.iter().filter(|s| s.raw_size > 0).map(|s| s.raw_ptr as usize).min().unwrap_or(p.headers);
            if entry + 40 > p.headers.min(first_raw) || d[entry..entry + 40].iter().any(|&b| b != 0) {
                return Err("the executable has no room for another section header".into());
            }
            let va = round_up(secs.iter().map(|s| s.va + s.vsize.max(s.raw_size)).max().ok_or_else(bad)?, p.sect_align);
            let raw_ptr = round_up(d.len() as u64, p.file_align);
            if va > u32::MAX as u64 || raw_ptr > u32::MAX as u64 { return Err(bad()); }
            d.resize(raw_ptr as usize, 0);
            secs.push(Sec { name: *NAME, vsize: 0, va, raw_size: 0, raw_ptr });
            d[entry..entry + 8].copy_from_slice(NAME);
            put_u32(d, entry + 12, va as u32);
            put_u32(d, entry + 20, raw_ptr as u32);
            put_u32(d, entry + 36, READ_DATA);
            // COFF header NumberOfSections, 18 bytes before the optional header.
            d[p.opt - 18..p.opt - 16].copy_from_slice(&((p.nsec + 1) as u16).to_le_bytes());
            (secs.len() - 1, raw_ptr)
        }
    };
    let mut addrs = Vec::new();
    for b in blobs {
        d.resize(d.len().next_multiple_of(8), 0);
        addrs.push(p.image_base + secs[i].va + (d.len() as u64 - raw_ptr));
        d.extend_from_slice(b);
    }
    let vsize = d.len() as u64 - raw_ptr;
    let raw_size = round_up(vsize, p.file_align);
    if vsize > u32::MAX as u64 || secs[i].va + raw_size > u32::MAX as u64 { return Err("the grown files are too large for a Windows executable".into()); }
    d.resize((raw_ptr + raw_size) as usize, 0);
    let entry = p.table + i * 40;
    put_u32(d, entry + 8, vsize as u32);
    put_u32(d, entry + 16, raw_size as u32);
    put_u32(d, p.opt + 56, round_up(secs[i].va + vsize, p.sect_align) as u32);
    Ok(addrs)
}

// After every change (in-place patches, a new section, repointed entries): recompute the
// checksum, if the file carries one.
pub fn fix_checksum(d: &mut [u8]) {
    let Some(p) = header(d) else { return };
    if u32_at(d, p.opt + 64) != 0 {
        let sum = checksum(d, p.opt + 64);
        put_u32(d, p.opt + 64, sum);
    }
}

// A pointer in a PE image is the absolute virtual address, fixed up by its base
// relocation when the image loads somewhere else.
pub fn point(d: &mut [u8], slot: usize, addr: u64, len: usize) -> Result<(), String> {
    d.get(slot + 16..slot + 32).ok_or("table entry is out of bounds")?;
    put_u64(d, slot + 16, addr);
    put_u64(d, slot + 24, len as u64);
    Ok(())
}

#[cfg(test)]
mod tests {
    // Pinned to an independent implementation that reproduces a real linker's stored
    // checksum (Rust's own Windows std DLL). Mutation check: drop the carry fold or the
    // skipped field and the value changes.
    #[test]
    fn checksum_matches_the_reference_algorithm() {
        let d: Vec<u8> = (0..1001u32).map(|i| ((i * 7 + 3) % 251) as u8).collect();
        assert_eq!(super::checksum(&d, 64), 0x896b);
    }
}
