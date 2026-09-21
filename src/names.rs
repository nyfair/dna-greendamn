use crate::buf::Buf;
use crate::mem;
use crate::mem::Module;

// UE object header: VTable(0x00) ObjectFlags(0x08) InternalIndex(0x0C) Class(0x10) Name(0x18)
pub const OFF_CLASS: usize = 0x10;
pub const OFF_NAME: usize = 0x18;
const OFF_OUTER: usize = 0x20;
// RF_ClassDefaultObject(0x10) | RF_ArchetypeObject(0x20): never live instances
pub const SKIP_FLAGS: u32 = 0x30;
const POOL_BLOCKS: usize = 0x10;
const BLOCK_SHIFT: u32 = 16;
const BLOCK_MASK: u32 = 0xFFFF;
const ENTRY_HEADER: usize = 2;
const MAX_NAME: usize = 64;

pub fn matches(src: &[u8], tgt: &[u8]) -> bool {
    if src.len() == tgt.len() {
        return src == tgt;
    }
    if src.len() > tgt.len() && src.len() - tgt.len() <= 2 {
        return &src[..tgt.len()] == tgt;
    }
    false
}

pub fn read_fname(pool: usize, id: u32, out: &mut [u8]) -> usize {
    let block = match mem::read_u64(pool + POOL_BLOCKS + ((id >> BLOCK_SHIFT) as usize) * 8) {
        Some(v) if v != 0 => v as usize,
        _ => return 0,
    };
    let entry = block + (id & BLOCK_MASK) as usize * 2;
    let h = match mem::read_u16(entry) {
        Some(v) => v,
        None => return 0,
    };
    let mut len = (h >> 1) as usize;
    let wide = (h & 1) != 0;
    if len == 0 {
        return 0;
    }
    if len > MAX_NAME {
        len = MAX_NAME;
    }
    let limit = core::cmp::min(out.len(), len);
    let mut buf = [0u8; MAX_NAME * 2];
    let want = core::cmp::min(if wide { limit * 2 } else { limit }, buf.len());
    let got = unsafe { mem::read_raw(entry + ENTRY_HEADER, buf.as_mut_ptr(), want) };
    let mut n = 0usize;
    if wide {
        let mut i = 0usize;
        while i + 1 < got && n < limit {
            if buf[i + 1] != 0 {
                break;
            }
            let c = buf[i];
            if !(0x20..0x7f).contains(&c) {
                break;
            }
            out[n] = c;
            n += 1;
            i += 2;
        }
    } else {
        while n < limit && n < got {
            let c = buf[n];
            if !(0x20..0x7f).contains(&c) {
                break;
            }
            out[n] = c;
            n += 1;
        }
    }
    n
}

pub fn class_name(module: &Module, pool: usize, obj: usize, out: &mut [u8]) -> usize {
    let class = class_of(obj);
    if class == 0 {
        return 0;
    }
    // a class lives on the heap, but its vtable has to sit inside the game image
    match mem::read_u64(class) {
        Some(v) if module.contains(v as usize) => {}
        _ => return 0,
    }
    let id = match mem::read_u32(class + OFF_NAME) {
        Some(v) => v,
        None => return 0,
    };
    read_fname(pool, id, out)
}

pub fn class_of(obj: usize) -> usize {
    mem::read_u64(obj + OFF_CLASS).unwrap_or(0) as usize
}

#[allow(dead_code)]
pub fn outer_of(obj: usize) -> usize {
    mem::read_u64(obj + OFF_OUTER).unwrap_or(0) as usize
}

/// appends the class name of `obj` (`?` when it cannot be resolved)
pub fn push_class_name(b: &mut Buf, module: &Module, pool: usize, obj: usize) {
    let mut buf = [0u8; MAX_NAME];
    let len = class_name(module, pool, obj, &mut buf);
    if len == 0 {
        b.push_byte(b'?');
    } else {
        b.push_bytes(&buf[..len]);
    }
}
