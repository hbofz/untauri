// Room for files that outgrow their original span: append them in new loadable space at
// the end of the binary and point their table entries there. Each format adds that space
// its own way; `append` returns the address each blob is mapped at, and `point` repoints
// one table entry (data pointer at +16, length at +24) to it.
// ponytail: ELF and PE; Mach-O (code signing, chained fixups, fat files) is next.

mod elf;
mod pe;

pub fn supported(data: &[u8]) -> bool { elf::supported(data) || pe::supported(data) }

pub fn append(data: &mut Vec<u8>, blobs: &[Vec<u8>]) -> Result<Vec<u64>, String> {
    if elf::supported(data) { elf::append(data, blobs) }
    else if pe::supported(data) { pe::append(data, blobs) }
    else { Err(UNSUPPORTED.into()) }
}

pub fn point(data: &mut [u8], slot: usize, addr: u64, len: usize) -> Result<(), String> {
    if elf::supported(data) { elf::point(data, slot, addr, len) }
    else if pe::supported(data) { pe::point(data, slot, addr, len) }
    else { Err(UNSUPPORTED.into()) }
}

// The last write before the result is checked: format bookkeeping that covers the whole
// file (the PE checksum).
pub fn finish(data: &mut [u8]) {
    if pe::supported(data) { pe::fix_checksum(data); }
}

pub const UNSUPPORTED: &str = "growing a file past its original size is supported for 64-bit Linux, Android and Windows binaries so far";
