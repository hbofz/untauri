// ELF (Linux, Android): the program header table has no spare entry, so the PT_NOTE entry
// becomes the new PT_LOAD. Notes are build metadata (build id, ABI tag) that the loader
// does not need; the note sections themselves stay in the file. The entry moves to just
// after the last PT_LOAD, because loaders expect PT_LOADs sorted by address.
//
// The segment also gets a `.untauri` section header. Without one, `strip` and `objcopy`
// (which every Linux packaging tool runs) rebuild the file from its sections and drop
// the data. The section header table and its name table are rewritten after the data.
//
// A later repack finds the segment by name and copies it to the end of the file at the
// same address, so every pointer into it stays valid, then appends after the copy. It
// never cuts anything off: after the first repack, tools like objcopy, `strip -g` or a
// debuglink step move symbol and debug sections after ours, and they must survive.

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_NOTE: u32 = 4;
const PT_GNU_PROPERTY: u32 = 0x6474_e553;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
// Android's packed relocations: their addends can't be edited in place, and with
// --apply-dynamic-relocs our read-back would not notice the stale one.
const DT_ANDROID_REL: u64 = 0x6000_000f;
const DT_ANDROID_RELA: u64 = 0x6000_0011;
const SECTION: &[u8] = b".untauri";
// Largest page size a real build uses (2 MiB huge pages); anything bigger is corrupt.
const MAX_ALIGN: u64 = 2 << 20;

#[derive(Clone, Copy)]
struct Phdr { kind: u32, flags: u32, offset: u64, vaddr: u64, paddr: u64, filesz: u64, memsz: u64, align: u64 }

fn u16_at(d: &[u8], o: usize) -> u16 { u16::from_le_bytes(d[o..o + 2].try_into().unwrap()) }
fn u32_at(d: &[u8], o: usize) -> u32 { u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) }
fn u64_at(d: &[u8], o: usize) -> u64 { u64::from_le_bytes(d[o..o + 8].try_into().unwrap()) }
fn put_u64(d: &mut [u8], o: usize, v: u64) { d[o..o + 8].copy_from_slice(&v.to_le_bytes()); }
fn round_up(v: u64, a: u64) -> Option<u64> { v.checked_next_multiple_of(a) }

// The R_*_RELATIVE type for this binary's architecture.
fn relative_type(data: &[u8]) -> Option<u32> {
    match u16_at(data, 0x12) { 62 => Some(8), 183 => Some(1027), _ => None } // x86-64, aarch64
}

pub fn supported(data: &[u8]) -> bool {
    data.len() > 64 && data.starts_with(b"\x7fELF") && data[4] == 2 && data[5] == 1
        && u16_at(data, 0x36) == 56 && u16_at(data, 0x3a) == 64 && relative_type(data).is_some()
}

// (offset of the table in the file, entries)
fn phdrs(data: &[u8]) -> Result<(usize, Vec<Phdr>), String> {
    let (off, n) = (u64_at(data, 0x20) as usize, u16_at(data, 0x38) as usize);
    n.checked_mul(56).and_then(|s| s.checked_add(off)).filter(|&e| e <= data.len()).ok_or("program header table is out of bounds")?;
    Ok((off, (0..n).map(|i| {
        let o = off + i * 56;
        Phdr { kind: u32_at(data, o), flags: u32_at(data, o + 4), offset: u64_at(data, o + 8), vaddr: u64_at(data, o + 16),
               paddr: u64_at(data, o + 24), filesz: u64_at(data, o + 32), memsz: u64_at(data, o + 40), align: u64_at(data, o + 48) }
    }).collect()))
}

fn write_phdrs(data: &mut [u8], off: usize, ph: &[Phdr]) {
    for (i, p) in ph.iter().enumerate() {
        let o = off + i * 56;
        data[o..o + 4].copy_from_slice(&p.kind.to_le_bytes());
        data[o + 4..o + 8].copy_from_slice(&p.flags.to_le_bytes());
        for (f, v) in [(8, p.offset), (16, p.vaddr), (24, p.paddr), (32, p.filesz), (40, p.memsz), (48, p.align)] { put_u64(data, o + f, v); }
    }
}

fn file_to_vaddr(ph: &[Phdr], off: u64) -> Option<u64> {
    ph.iter().find(|p| p.kind == PT_LOAD && off >= p.offset && off - p.offset < p.filesz).map(|p| off - p.offset + p.vaddr)
}
fn vaddr_to_file(ph: &[Phdr], va: u64) -> Option<usize> {
    ph.iter().find(|p| p.kind == PT_LOAD && va >= p.vaddr && va - p.vaddr < p.filesz).map(|p| (va - p.vaddr + p.offset) as usize)
}

fn dynamic(data: &[u8], ph: &[Phdr]) -> Vec<(u64, u64)> {
    let Some(p) = ph.iter().find(|p| p.kind == PT_DYNAMIC) else { return vec![] };
    let end = p.offset.saturating_add(p.filesz).min(data.len() as u64) as usize;
    (p.offset as usize..end).step_by(16).filter(|o| o + 16 <= end)
        .map(|o| (u64_at(data, o), u64_at(data, o + 8))).take_while(|&(t, _)| t != 0).collect()
}

// The section header table and its name table, read out of the file.
struct Sections { entries: Vec<[u8; 64]>, names: Vec<u8>, strndx: usize }

impl Sections {
    fn read(data: &[u8]) -> Result<Sections, String> {
        let (off, n, strndx) = (u64_at(data, 0x28) as usize, u16_at(data, 0x3c) as usize, u16_at(data, 0x3e) as usize);
        // One below SHN_LORESERVE, so the section we may add still fits without extended numbering.
        if n == 0 || n >= 0xfeff || strndx >= n { return Err("the binary's section headers are missing or use extended numbering".into()); }
        let table = data.get(off..off.checked_add(n * 64).ok_or("section headers out of bounds")?).ok_or("section headers out of bounds")?;
        let entries: Vec<[u8; 64]> = table.chunks(64).map(|c| c.try_into().unwrap()).collect();
        let (so, ss) = (u64_at(&entries[strndx], 24) as usize, u64_at(&entries[strndx], 32) as usize);
        let names = data.get(so..so.checked_add(ss).ok_or("section names out of bounds")?).ok_or("section names out of bounds")?.to_vec();
        Ok(Sections { entries, names, strndx })
    }
    fn find(&self, name: &[u8]) -> Option<usize> {
        self.entries.iter().position(|e| {
            let at = u32_at(e, 0) as usize;
            self.names.get(at..).and_then(|s| s.split(|&b| b == 0).next()) == Some(name)
        })
    }
    // Write the name table and the section header table at the end of the file.
    fn append_to(&mut self, data: &mut Vec<u8>) {
        let at = data.len() as u64;
        put_u64(&mut self.entries[self.strndx], 24, at);
        put_u64(&mut self.entries[self.strndx], 32, self.names.len() as u64);
        data.extend_from_slice(&self.names);
        data.resize(data.len().next_multiple_of(8), 0);
        let shoff = data.len() as u64;
        for e in &self.entries { data.extend_from_slice(e); }
        put_u64(data, 0x28, shoff);
        data[0x3c..0x3e].copy_from_slice(&(self.entries.len() as u16).to_le_bytes());
    }
}

// Append `blobs` to the binary, mapped read-only. Returns each blob's virtual address.
pub fn append(data: &mut Vec<u8>, blobs: &[Vec<u8>]) -> Result<Vec<u64>, String> {
    let (off, mut ph) = phdrs(data)?;
    if dynamic(data, &ph).iter().any(|&(t, _)| t == DT_ANDROID_REL || t == DT_ANDROID_RELA) {
        return Err("this binary uses Android packed relocations, which repack can't update to grow a file".into());
    }
    let mut sh = Sections::read(data)?;
    let corrupt = || "the binary's program headers are inconsistent".to_string();

    let seg = match sh.find(SECTION) {
        // An earlier repack's segment: copy it to the end of the file at the same address
        // and keep appending there. Whatever sits after the old copy stays untouched.
        Some(s) => {
            let e = &sh.entries[s];
            let (addr, start, size) = (u64_at(e, 16), u64_at(e, 24), u64_at(e, 32));
            let changed = || "the binary was changed after the last repack in a way repack can't follow; repack the original binary instead".to_string();
            let i = ph.iter().position(|p| p.kind == PT_LOAD && p.vaddr == addr && p.offset == start && p.filesz == size && p.memsz == size)
                .ok_or_else(changed)?;
            // Growing it in memory must not run into any other segment.
            addr.checked_add(size).ok_or_else(corrupt)?;
            if ph.iter().enumerate().any(|(j, p)| j != i && p.kind == PT_LOAD && p.vaddr.saturating_add(p.memsz) > addr) { return Err(changed()); }
            let old = data.get(start as usize..(start as usize).checked_add(size as usize).ok_or_else(corrupt)?)
                .ok_or_else(changed)?.to_vec();
            // The page size, as on the first repack: our own p_align could have been edited.
            let align = ph.iter().filter(|p| p.kind == PT_LOAD).map(|p| p.align).max().unwrap_or(0).max(0x1000);
            if !align.is_power_of_two() || align > MAX_ALIGN { return Err(corrupt()); }
            let at = round_up(data.len() as u64, align).ok_or_else(corrupt)? + addr % align;
            data.resize(at as usize, 0);
            data.extend_from_slice(&old);
            ph[i].offset = at;
            put_u64(&mut sh.entries[s], 24, at);
            i
        }
        None => {
            let loads = ph.iter().filter(|p| p.kind == PT_LOAD);
            let top = loads.clone().map(|p| p.vaddr.checked_add(p.memsz)).collect::<Option<Vec<_>>>().ok_or_else(corrupt)?
                .into_iter().max().ok_or("no loadable segments")?;
            let align = loads.map(|p| p.align).max().unwrap_or(0).max(0x1000);
            if !align.is_power_of_two() || align > MAX_ALIGN { return Err(corrupt()); }
            let props: Vec<(u64, u64)> = ph.iter().filter(|p| p.kind == PT_GNU_PROPERTY).map(|p| (p.offset, p.filesz)).collect();
            let note = (0..ph.len()).rev().find(|&i| ph[i].kind == PT_NOTE && !props.contains(&(ph[i].offset, ph[i].filesz)))
                .ok_or("the binary has no spare program header (PT_NOTE) to map new data with")?;
            let (start, vaddr) = (round_up(data.len() as u64, align).ok_or_else(corrupt)?, round_up(top, align).ok_or_else(corrupt)?);
            ph.remove(note);
            let at = ph.iter().rposition(|q| q.kind == PT_LOAD).map_or(ph.len(), |i| i + 1);
            ph.insert(at, Phdr { kind: PT_LOAD, flags: 4, offset: start, vaddr, paddr: vaddr, filesz: 0, memsz: 0, align });
            data.resize(start as usize, 0);
            // A PROGBITS, SHF_ALLOC section over the segment, named in a grown name table.
            let mut e = [0u8; 64];
            e[0..4].copy_from_slice(&(sh.names.len() as u32).to_le_bytes());
            e[4..8].copy_from_slice(&1u32.to_le_bytes());
            put_u64(&mut e, 8, 2);
            put_u64(&mut e, 16, vaddr);
            put_u64(&mut e, 24, start);
            put_u64(&mut e, 48, 8);
            sh.names.extend_from_slice(SECTION);
            sh.names.push(0);
            sh.entries.push(e);
            at
        }
    };
    let mut vaddrs = Vec::new();
    for b in blobs {
        data.resize(data.len().next_multiple_of(8), 0);
        vaddrs.push(ph[seg].vaddr + (data.len() as u64 - ph[seg].offset));
        data.extend_from_slice(b);
    }
    let size = data.len() as u64 - ph[seg].offset;
    ph[seg].filesz = size;
    ph[seg].memsz = size;
    let s = sh.find(SECTION).expect("added above");
    put_u64(&mut sh.entries[s], 32, size);
    write_phdrs(data, off, &ph);
    sh.append_to(data);
    Ok(vaddrs)
}

// Point the table entry at `slot` (its data pointer at +16, length at +24) to `vaddr`.
// In a position-independent binary the pointer is set at load time from a RELATIVE
// relocation's addend, so that is updated too; the slot itself is written for RELR and
// non-PIE binaries, which read the address from it.
pub fn point(data: &mut [u8], slot: usize, vaddr: u64, len: usize) -> Result<(), String> {
    let (_, ph) = phdrs(data)?;
    let relative = relative_type(data).ok_or("unsupported architecture")?;
    let ptr = slot + 16;
    let ptr_va = file_to_vaddr(&ph, ptr as u64).ok_or("table entry is not in a loaded segment")?;
    let tags = dynamic(data, &ph);
    let tag = |t| tags.iter().find(|&&(k, _)| k == t).map(|&(_, v)| v);
    if let (Some(start), Some(size)) = (tag(DT_RELA).and_then(|va| vaddr_to_file(&ph, va)), tag(DT_RELASZ)) {
        let end = start.saturating_add(size as usize).min(data.len());
        for e in (start..end).step_by(24).filter(|&e| e + 24 <= end) {
            if u64_at(data, e) != ptr_va { continue; }
            if u64_at(data, e + 8) as u32 != relative { return Err("the table entry's pointer has an unexpected relocation type".into()); }
            put_u64(data, e + 16, vaddr);
        }
    }
    put_u64(data, ptr, vaddr);
    put_u64(data, slot + 24, len as u64);
    Ok(())
}
