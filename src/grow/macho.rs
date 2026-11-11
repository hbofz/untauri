// macOS (64-bit Mach-O): a new read-only `__UNTAURI` segment right before __LINKEDIT,
// which moves up (in the file and in memory) to make room. __LINKEDIT has to stay last:
// codesign requires it, and the code signature inside it must end the file. Everything
// in __LINKEDIT (symbols, fixups, exports, the signature) is found through a file offset
// in a load command, so moving it means updating those offsets.
//
// The segment's load command goes in the padding the linker leaves between the load
// commands and the first section (a hundred-odd bytes in real Tauri apps; it takes 72).
// It has no section: nothing needs one, and adding one would renumber the sections the
// symbol table refers to.
//
// Binaries built for macOS 12+ use chained fixups, which keep one entry per segment, and
// dyld checks that count. The new segment gets an empty entry there.
//
// A table entry's pointer is a plain address (older binaries, rebased by opcodes that
// name its location) or a chained-fixup rebase whose low 36 bits are the target, as an
// address or as an offset from the image base. Only those 36 bits change.
//
// The code signature no longer matches after any patch; repack already says to re-sign.
//
// A later repack finds `__UNTAURI` by name right before __LINKEDIT, extends it in place
// and moves __LINKEDIT up again.

const NAME: &[u8; 16] = b"__UNTAURI\0\0\0\0\0\0\0";
const LINKEDIT: &[u8; 16] = b"__LINKEDIT\0\0\0\0\0\0";
const TEXT: &[u8; 16] = b"__TEXT\0\0\0\0\0\0\0\0\0\0";
const MH_MAGIC_64: u32 = 0xfeed_facf;
const ARM64: u32 = 0x0100_000c;
const X86_64: u32 = 0x0100_0007;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
const LC_NOTE: u32 = 0x31;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;
const LC_CODE_SIGNATURE: u32 = 0x1d;
// Every load command that is a linkedit_data_command (dataoff at +8, datasize at +12):
// code signature, split info, function starts, data in code, code-sign DRs, linker
// optimization hints, exports trie, chained fixups, atom info, function variants (+fixups).
const LINKEDIT_DATA: [u32; 11] = [LC_CODE_SIGNATURE, 0x1e, 0x26, 0x29, 0x2b, 0x2e, 0x8000_0033, LC_DYLD_CHAINED_FIXUPS, 0x36, 0x37, 0x38];
const SEG_CMD: usize = 72;
const LOW36: u64 = (1 << 36) - 1;

fn u32_at(d: &[u8], o: usize) -> u32 { u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) }
fn u64_at(d: &[u8], o: usize) -> u64 { u64::from_le_bytes(d[o..o + 8].try_into().unwrap()) }
fn put_u32(d: &mut [u8], o: usize, v: u32) { d[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
fn add_u32(d: &mut [u8], o: usize, v: u32) { let x = u32_at(d, o) + v; put_u32(d, o, x); }
fn put_u64(d: &mut [u8], o: usize, v: u64) { d[o..o + 8].copy_from_slice(&v.to_le_bytes()); }

pub fn supported(d: &[u8]) -> bool {
    // MH_EXECUTE, arm64 (not arm64e, whose pointers are signed) or x86-64.
    d.len() > 32 && u32_at(d, 0) == MH_MAGIC_64 && u32_at(d, 12) == 2
        && (u32_at(d, 4) == X86_64 || (u32_at(d, 4) == ARM64 && u32_at(d, 8) & 0xff != 2))
}

// (offset, cmd) of every load command.
fn commands(d: &[u8]) -> Result<Vec<(usize, u32)>, String> {
    let bad = || "the binary's load commands are inconsistent".to_string();
    let (n, size) = (u32_at(d, 16) as usize, u32_at(d, 20) as usize);
    let end = 32usize.checked_add(size).filter(|&e| e <= d.len()).ok_or_else(bad)?;
    let mut out = Vec::with_capacity(n);
    let mut o = 32;
    for _ in 0..n {
        if o + 8 > end { return Err(bad()); }
        let len = u32_at(d, o + 4) as usize;
        if len < 8 || !len.is_multiple_of(8) || o + len > end { return Err(bad()); }
        let cmd = u32_at(d, o);
        // Big enough for every field read or written later (a segment: with its sections).
        let need = match cmd {
            LC_SYMTAB => 24,
            LC_DYSYMTAB => 80,
            LC_DYLD_INFO | LC_DYLD_INFO_ONLY => 48,
            LC_NOTE => 40,
            LC_SEGMENT_64 if len >= SEG_CMD => (u32_at(d, o + 64) as usize).checked_mul(80).ok_or_else(bad)? + SEG_CMD,
            LC_SEGMENT_64 => SEG_CMD,
            c if LINKEDIT_DATA.contains(&c) => 16,
            _ => 8,
        };
        if len < need { return Err(bad()); }
        out.push((o, cmd));
        o += len;
    }
    Ok(out)
}

#[derive(Clone, Copy)]
struct Seg { lc: usize, name: [u8; 16], vmaddr: u64, vmsize: u64, fileoff: u64, filesize: u64 }

fn segments(d: &[u8], cmds: &[(usize, u32)]) -> Vec<Seg> {
    cmds.iter().filter(|c| c.1 == LC_SEGMENT_64).map(|&(o, _)| Seg {
        lc: o, name: d[o + 8..o + 24].try_into().unwrap(), vmaddr: u64_at(d, o + 24), vmsize: u64_at(d, o + 32),
        fileoff: u64_at(d, o + 40), filesize: u64_at(d, o + 48),
    }).collect()
}

// Give the segment at `index` an empty entry in the chained fixups' per-segment table
// (`blob` is the LC_DYLD_CHAINED_FIXUPS payload inside `le`). 16 bytes go in right after
// the table, which keeps everything after them aligned as it was; the offsets that point
// past them move up by 16. Returns where in `le` they went.
fn add_chained_entry(le: &mut Vec<u8>, blob: usize, size: usize, index: usize, nsegs: usize) -> Result<usize, String> {
    let bad = || "the binary's chained fixups are inconsistent".to_string();
    if size < 28 || blob.checked_add(size).is_none_or(|e| e > le.len()) { return Err(bad()); }
    let (starts, imports, symbols) = (u32_at(le, blob + 4) as usize, u32_at(le, blob + 8) as usize, u32_at(le, blob + 12) as usize);
    let s = blob + starts;
    if starts + 4 > size || imports > size || symbols > size || u32_at(le, s) as usize != nsegs { return Err(bad()); }
    let table = s + 4;
    let table_end = table + 4 * nsegs;
    if table_end > blob + size { return Err(bad()); }
    let offsets: Vec<u32> = (0..nsegs).map(|i| u32_at(le, table + 4 * i)).collect();
    if offsets.iter().any(|&v| v != 0 && ((v as usize) < 4 + 4 * nsegs || starts + v as usize > size)) { return Err(bad()); }
    le.splice(table_end..table_end, [0u8; 16]);
    let mut new: Vec<u32> = offsets.iter().map(|&v| if v == 0 { 0 } else { v + 16 }).collect();
    new.insert(index, 0);
    for (i, v) in new.iter().enumerate() { put_u32(le, table + 4 * i, *v); }
    put_u32(le, s, new.len() as u32);
    for (field, v) in [(8, imports), (12, symbols)] {
        if blob + v >= table_end { put_u32(le, blob + field, (v + 16) as u32); }
    }
    Ok(table_end)
}

// Append `blobs` to the binary, mapped read-only. Returns each blob's virtual address.
pub fn append(d: &mut Vec<u8>, blobs: &[Vec<u8>]) -> Result<Vec<u64>, String> {
    let bad = || "the binary's load commands are inconsistent".to_string();
    let changed = || "the binary was changed after the last repack in a way repack can't follow; repack the original binary instead".to_string();
    let cmds = commands(d)?;
    let segs = segments(d, &cmds);
    let page: u64 = if u32_at(d, 4) == ARM64 { 0x4000 } else { 0x1000 };

    // __LINKEDIT must be the last segment in every sense, and end the file.
    let li = segs.iter().position(|s| &s.name == LINKEDIT).ok_or("the binary has no __LINKEDIT segment")?;
    let link = segs[li];
    if !link.fileoff.is_multiple_of(page) || !link.vmaddr.is_multiple_of(page) {
        return Err(format!("the binary's __LINKEDIT segment is not aligned to {} KB pages, which repack doesn't handle", page / 1024));
    }
    if li + 1 != segs.len() || link.fileoff == 0
        || segs[..li].iter().any(|s| s.vmaddr.saturating_add(s.vmsize) > link.vmaddr || s.fileoff.saturating_add(s.filesize) > link.fileoff) {
        return Err(bad());
    }
    if link.fileoff.checked_add(link.filesize) != Some(d.len() as u64) {
        return Err("the binary has data after its __LINKEDIT segment, which repack won't bury".into());
    }

    let ours = segs.iter().position(|s| &s.name == NAME);
    let (start, vmaddr, old_len) = match ours {
        Some(i) => {
            let s = segs[i];
            if i + 1 != li || s.fileoff + s.filesize != link.fileoff || s.vmaddr + s.vmsize != link.vmaddr { return Err(changed()); }
            (s.fileoff, s.vmaddr, s.filesize)
        }
        None => (link.fileoff, link.vmaddr, 0),
    };
    let mut seg = d[start as usize..(start + old_len) as usize].to_vec();
    let mut addrs = Vec::new();
    for b in blobs {
        seg.resize(seg.len().next_multiple_of(8), 0);
        addrs.push(vmaddr + seg.len() as u64);
        seg.extend_from_slice(b);
    }
    seg.resize(seg.len().next_multiple_of(page as usize), 0);
    let delta = seg.len() as u64 - old_len;
    let mut le = d[link.fileoff as usize..].to_vec();

    // The load commands end before the first section's data (`commands` checked each
    // segment holds its sections).
    let header_end = 32 + u32_at(d, 20) as usize;
    let first = segs.iter().flat_map(|s| (0..u32_at(d, s.lc + 64) as usize).map(move |k| s.lc + SEG_CMD + k * 80))
        .map(|o| u32_at(d, o + 48) as usize).filter(|&off| off > 0).min().unwrap_or(usize::MAX).min(link.fileoff as usize);
    if header_end > first { return Err(bad()); }

    // A new segment: room for its command, and its chained-fixups entry.
    let mut grew_at = None;
    if ours.is_none() {
        // An unsigned binary (ld's default on x86-64) also needs room for the code
        // signature command codesign adds when the user re-signs. codesign doesn't check,
        // and would write over the start of the code.
        let room = SEG_CMD + if cmds.iter().any(|c| c.1 == LC_CODE_SIGNATURE) { 0 } else { 16 };
        if header_end + room > first || d[header_end..header_end + room].iter().any(|&b| b != 0) {
            return Err("the binary has no room for another load command".into());
        }
        if let Some(&(o, _)) = cmds.iter().find(|c| c.1 == LC_DYLD_CHAINED_FIXUPS) {
            let at = (u32_at(d, o + 8) as u64).checked_sub(link.fileoff).ok_or_else(bad)? as usize;
            grew_at = Some(link.fileoff + add_chained_entry(&mut le, at, u32_at(d, o + 12) as usize, li, segs.len())? as u64);
        }
        let at = link.lc;
        d.copy_within(at..header_end, at + SEG_CMD);
        let mut c = [0u8; SEG_CMD];
        put_u32(&mut c, 0, LC_SEGMENT_64);
        put_u32(&mut c, 4, SEG_CMD as u32);
        c[8..24].copy_from_slice(NAME);
        put_u64(&mut c, 24, vmaddr);
        put_u64(&mut c, 40, start);
        put_u32(&mut c, 56, 1); // maxprot: read
        put_u32(&mut c, 60, 1); // initprot: read
        d[at..at + SEG_CMD].copy_from_slice(&c);
        add_u32(d, 16, 1);
        add_u32(d, 20, SEG_CMD as u32);
    }

    // Where a __LINKEDIT file offset ends up.
    let grown = |o: u64| grew_at.is_some_and(|g| o >= g) as u64 * 16;
    let shift = |o: u64| if o >= link.fileoff { o + delta + grown(o) } else { o };
    let too_big = || "the grown files are too large for a macOS binary".to_string();
    let fix32 = |d: &mut Vec<u8>, at: usize| -> Result<(), String> {
        let v = shift(u32_at(d, at) as u64);
        put_u32(d, at, u32::try_from(v).map_err(|_| too_big())?);
        Ok(())
    };
    for (o, cmd) in commands(d)? {
        match cmd {
            LC_SYMTAB => for f in [8, 16] { fix32(d, o + f)? },
            LC_DYSYMTAB => for f in [32, 40, 48, 56, 64, 72] { fix32(d, o + f)? },
            LC_DYLD_INFO | LC_DYLD_INFO_ONLY => for f in [8, 16, 24, 32, 40] { fix32(d, o + f)? },
            LC_NOTE => { let v = shift(u64_at(d, o + 24)); put_u64(d, o + 24, v); }
            c if LINKEDIT_DATA.contains(&c) => {
                fix32(d, o + 8)?;
                if c == LC_DYLD_CHAINED_FIXUPS && grew_at.is_some() { add_u32(d, o + 12, 16); }
            }
            LC_SEGMENT_64 if d[o + 8..o + 24] == *NAME => {
                put_u64(d, o + 32, seg.len() as u64);
                put_u64(d, o + 48, seg.len() as u64);
            }
            LC_SEGMENT_64 if d[o + 8..o + 24] == *LINKEDIT => {
                put_u64(d, o + 24, link.vmaddr + delta);
                put_u64(d, o + 40, link.fileoff + delta);
                put_u64(d, o + 48, le.len() as u64);
                put_u64(d, o + 32, (le.len() as u64).next_multiple_of(page));
            }
            _ => {}
        }
    }
    if link.vmaddr + delta + le.len() as u64 > LOW36 { return Err(too_big()); }
    d.truncate(start as usize);
    d.extend_from_slice(&seg);
    d.extend_from_slice(&le);
    Ok(addrs)
}

// Point the table entry at `slot` (its data pointer at +16, length at +24) to `addr`.
pub fn point(d: &mut [u8], slot: usize, addr: u64, len: usize) -> Result<(), String> {
    d.get(slot + 16..slot + 32).ok_or("table entry is out of bounds")?;
    let segs = segments(d, &commands(d)?);
    let base = segs.iter().find(|s| &s.name == TEXT).map(|s| s.vmaddr).ok_or("the binary has no __TEXT segment")?;
    let mapped = |va: u64| segs.iter().any(|s| va >= s.vmaddr && va - s.vmaddr < s.filesize);
    let old = u64_at(d, slot + 16);
    let t = old & LOW36;
    // Bit 63 set is a bind (a symbol from another image), not a pointer into this one.
    let target = if old >> 63 != 0 { None }
        else if mapped(t) { Some(addr) }
        else if mapped(t + base) { addr.checked_sub(base) }
        else { None };
    let target = target.filter(|&t| t <= LOW36).ok_or("the table entry's pointer is in a form repack can't update")?;
    put_u64(d, slot + 16, (old & !LOW36) | target);
    put_u64(d, slot + 24, len as u64);
    Ok(())
}
