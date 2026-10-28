use core::cmp::min;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::xx::buf::Buf;
use crate::xx::ffi;
use crate::xx::log;
use crate::xx::mem::*;

// From the dumper-7 CppSDK dump (CppSDK/SDK/Engine_classes.hpp).
// UWorld::OwningGameInstance
const OFF_WORLD_GI: usize = 0x1A8;
// UGameInstance::LocalPlayers
const OFF_GI_PLAYERS: usize = 0x38;
// UPlayer::PlayerController
const OFF_PLAYER_PC: usize = 0x30;
// APlayerController::PlayerCameraManager
const OFF_PC_MANAGER: usize = 0x340;
// TArray<UPlayer*>: Data @ +0x00, Num @ +0x08
const OFF_TARRAY_NUM: usize = 0x08;
// Offsets::ProcessEventIdx
const PROCESSEVENT_IDX: usize = 0x44;

// ---- object / array / FName ----
const OFF_MAX_ELEMENTS: usize = 0x10;
const OFF_NUM_ELEMENTS: usize = 0x14;
const OFF_MAX_CHUNKS: usize = 0x18;
const OFF_NUM_CHUNKS: usize = 0x1C;
const ELEMENTS_PER_CHUNK: u32 = 0x10000;
const FUOBJECT_ITEM: usize = 0x18;
const UA_MIN_ELEMENTS: u32 = 0x800;
const UA_MIN_MAX_ELEMENTS: u32 = 0x1_0000;
const UA_MIN_CHUNKS: u32 = 1;
const UA_MAX_CHUNKS: u32 = 0x14;
const UA_MIN_MAX_CHUNKS: u32 = 6;
const UA_MAX_MAX_CHUNKS: u32 = 0x5FF;
const UA_CHUNK_ALIGN: u32 = 0x10;
const UA_MIN_PER_CHUNK: u32 = 0x8000;
const UA_MAX_PER_CHUNK: u32 = 0x8_0000;
const FNAME_BLOCKS_MAX: usize = 8192;
const FNAME_BLOCK_BYTES_MAX: usize = 0x2_0000;

// object header block
const OBJ_BLOCK: usize = 0x28;
// UE object header: VTable(0x00) ObjectFlags(0x08) InternalIndex(0x0C) Class(0x10) Name(0x18)
const OFF_FLAGS: usize = 0x08;
const OFF_CLASS: usize = 0x10;
const OFF_NAME: usize = 0x18;
// RF_ClassDefaultObject(0x10) | RF_ArchetypeObject(0x20): never live instances
const RF_CDO_OR_ARCHETYPE: u32 = 0x10 | 0x20;
const RF_DESTROYED: u32 = 0x8000 | 0x10000;

const POOL_BLOCKS: usize = 0x10;
const BLOCK_SHIFT: u32 = 16;
const BLOCK_MASK: u32 = 0xFFFF;
const ENTRY_HEADER: usize = 2;
const MAX_NAME: usize = 64;

const CAM_CLASS: &[u8] = b"PlayerCameraComponent";
const FUNC_CLASS: &[u8] = b"Function";
const DISABLE_NAME: &[u8] = b"DisableCameraDither";

// class tags
const TAG_OTHER: u32 = 0;
const TAG_CAM: u32 = 1;
const TAG_FUNC: u32 = 2;
// object-name tags (only resolved for Function objects)
const TAG_DISABLE: u32 = 1;
// direct-mapped caches (2048 slots)
const CACHE_MASK: usize = 0x7FF;

const SCAN_CHUNK: usize = 0x10000;
// sanity cap: something is very wrong if the array claims more than this
const MAX_ELEMENTS: u32 = 8_000_000;
const MAX_CHUNKS: u32 = 0x1000;

static WORLD_SLOT: AtomicUsize = AtomicUsize::new(0);
static GOBJECTS: AtomicUsize = AtomicUsize::new(0);
static GNAMES: AtomicUsize = AtomicUsize::new(0);
static RESOLVE_TRIES: AtomicU64 = AtomicU64::new(0);
static NEXT_SCAN: AtomicU64 = AtomicU64::new(0);
// class-ptr -> validity (slot = class | 1 when the class vtable is in-module)
static CLASS_OK: [AtomicU64; 2048] = [const { AtomicU64::new(0) }; 2048];
// class-ptr -> class tag, parallel to CLASS_OK
static CLASS_TAG: [AtomicU64; 2048] = [const { AtomicU64::new(0) }; 2048];
// FName id -> object-name tag (slot = valid | tag<<1 | key<<32)
static NAME_TAG: [AtomicU64; 2048] = [const { AtomicU64::new(0) }; 2048];

static CUR_MANAGER: AtomicUsize = AtomicUsize::new(0);
static FUNC: AtomicUsize = AtomicUsize::new(0);
static CAM_EVER_LOGGED: AtomicUsize = AtomicUsize::new(0);
static CAM_CALLS: AtomicU64 = AtomicU64::new(0);
static CAMS_LAST: AtomicU64 = AtomicU64::new(0);
// raw count from the last full scan, for the log only
static CAMS_SEEN: AtomicU64 = AtomicU64::new(0);
static KNOWN_N: AtomicU64 = AtomicU64::new(0);
// last full-scan duration (ms) and full-scan count; idle ticks skip the walk
static SCAN_MS: AtomicU64 = AtomicU64::new(0);
static SCANS: AtomicU64 = AtomicU64::new(0);
static KNOWN: [AtomicUsize; 128] = [const { AtomicUsize::new(0) }; 128];
// GObjects total seen by the last full scan (u32::MAX = none yet)
static LAST_TOTAL: AtomicU64 = AtomicU64::new(u32::MAX as u64);
// consecutive ticks with no usable Disable UFunction
static FUNC_STALE: AtomicU64 = AtomicU64::new(0);

struct Described {
    class_tag: u32,
    name_id: u32,
}

fn cache_idx(key: usize) -> usize {
    key & CACHE_MASK
}

fn count(objects: usize) -> u32 {
    if objects == 0 {
        return 0;
    }
    match read_u32(objects + OFF_NUM_ELEMENTS) {
        Some(n) if n <= MAX_ELEMENTS => n,
        _ => 0,
    }
}

fn chunks_ptr(objects: usize) -> usize {
    if objects == 0 {
        return 0;
    }
    match read_u32(objects + OFF_NUM_CHUNKS) {
        Some(n) if n > 0 && n <= MAX_CHUNKS => {}
        _ => return 0,
    }
    read_u64(objects).unwrap_or(0) as usize
}

fn gobjects() -> usize {
    GOBJECTS.load(Ordering::Relaxed)
}

fn gnames() -> usize {
    GNAMES.load(Ordering::Relaxed)
}

fn is_destroyed(obj: usize) -> bool {
    matches!(read_u32(obj + OFF_FLAGS), Some(f) if f & RF_DESTROYED != 0)
}

fn valid_object(module: &Module, obj: usize) -> bool {
    obj != 0 && obj % 8 == 0 && read_u64(obj).is_some_and(|v| module.contains(v as usize)) && !is_destroyed(obj)
}

fn is_cdo_or_archetype(obj: usize) -> bool {
    matches!(read_u32(obj + 0x08), Some(f) if f & RF_CDO_OR_ARCHETYPE != 0)
}

fn array(addr: usize, limit: u32) -> Option<(usize, u32)> {
    let data = read_u64(addr)? as usize;
    let num = read_u32(addr + OFF_TARRAY_NUM)?;
    if data == 0 || num == 0 || num > limit {
        return None;
    }
    Some((data, num))
}

fn chunk_at(chunks: usize, chunk_idx: u32) -> usize {
    if chunks == 0 {
        return 0;
    }
    read_u64(chunks + chunk_idx as usize * 8).unwrap_or(0) as usize
}

fn item_at(chunk: usize, index: u32) -> usize {
    if chunk == 0 {
        return 0;
    }
    read_u64(chunk + (index % ELEMENTS_PER_CHUNK) as usize * FUOBJECT_ITEM).unwrap_or(0) as usize
}

fn matches(src: &[u8], tgt: &[u8]) -> bool {
    if src.len() == tgt.len() {
        return src == tgt;
    }
    if src.len() > tgt.len() && src.len() - tgt.len() <= 2 {
        return &src[..tgt.len()] == tgt;
    }
    false
}

fn read_fname(pool: usize, id: u32, out: &mut [u8]) -> usize {
    let block = match read_u64(pool + POOL_BLOCKS + ((id >> BLOCK_SHIFT) as usize) * 8) {
        Some(v) if v != 0 => v as usize,
        _ => return 0,
    };
    let entry = block + (id & BLOCK_MASK) as usize * 2;
    let h = match read_u16(entry) {
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
    let limit = min(out.len(), len);
    let mut buf = [0u8; MAX_NAME * 2];
    let want = min(if wide { limit * 2 } else { limit }, buf.len());
    let got = unsafe { read_raw(entry + ENTRY_HEADER, buf.as_mut_ptr(), want) };
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

fn rd_u32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn rd_u64(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

fn scan<F>(module: &Module, window: usize, mut test: F) -> Option<usize>
where
    F: FnMut(usize, &[u8], usize) -> bool,
{
    let mut sections = [Section { start: 0, len: 0 }; MAX_SECTIONS];
    let n = module.writable_sections(&mut sections);
    let mut buf = [0u8; SCAN_CHUNK];
    let overlap = window - 8;
    let mut s = 0usize;
    while s < n {
        let start = sections[s].start;
        let len = sections[s].len;
        let mut off = 0usize;
        while off < len {
            let left = len - off;
            let want = if left < SCAN_CHUNK { left } else { SCAN_CHUNK };
            let got = unsafe { read_raw(start + off, buf.as_mut_ptr(), want) };
            let usable = if got < window { 0 } else { got & !7 };
            let mut i = 0usize;
            while i + window <= usable {
                if test(start + off + i, &buf, i) {
                    return Some(start + off + i);
                }
                i += 8;
            }
            let step = if got == want && got > window {
                got - overlap
            } else if usable > window {
                usable - overlap
            } else {
                want
            };
            off += step;
        }
        s += 1;
    }
    None
}

fn heapish(module: &Module, p: u64) -> bool {
    p != 0 && p % 8 == 0 && p >= 0x1_0000 && !module.contains(p as usize)
}

/// Walks World -> GameInstance -> LocalPlayers -> PlayerController -> PlayerCameraManager.
/// None when any link is missing, not an object, or a CDO/archetype.
fn resolve_world(module: &Module, world: usize) -> Option<(usize, usize)> {
    if world == 0 || !valid_object(module, world) || is_cdo_or_archetype(world) {
        return None;
    }
    let gi = read_u64(world + OFF_WORLD_GI).unwrap_or(0) as usize;
    if gi == 0 || !valid_object(module, gi) {
        return None;
    }
    let (data, _num) = array(gi + OFF_GI_PLAYERS, 32)?;
    let lp = read_u64(data).unwrap_or(0) as usize;
    if lp == 0 || !valid_object(module, lp) {
        return None;
    }
    let pc = read_u64(lp + OFF_PLAYER_PC).unwrap_or(0) as usize;
    if pc == 0 || !valid_object(module, pc) {
        return None;
    }
    let mgr = read_u64(pc + OFF_PC_MANAGER).unwrap_or(0) as usize;
    if mgr == 0 || !valid_object(module, mgr) {
        return None;
    }
    Some((pc, mgr))
}

/// Resolve the current camera manager through the controller chain. None while loading / between worlds.
fn resolve_manager(module: &Module) -> Option<usize> {
    let slot = WORLD_SLOT.load(Ordering::Acquire);
    if slot == 0 || !module.contains(slot) {
        return None;
    }
    let world = read_u64(slot)? as usize;
    resolve_world(module, world).map(|w| w.1)
}

fn discover_gworld(module: &Module) -> Option<usize> {
    scan(module, 8, |_at, b, i| {
        let world = rd_u64(b, i) as usize;
        world != 0
            && (world & 7) == 0
            && !module.contains(world)
            && resolve_world(module, world).is_some()
    })
}

fn check_objects(b: &[u8], i: usize) -> bool {
    let max = rd_u32(b, i + OFF_MAX_ELEMENTS);
    let num = rd_u32(b, i + OFF_NUM_ELEMENTS);
    let max_chunks = rd_u32(b, i + OFF_MAX_CHUNKS);
    let num_chunks = rd_u32(b, i + OFF_NUM_CHUNKS);
    if num <= UA_MIN_ELEMENTS || max <= UA_MIN_MAX_ELEMENTS || num > max {
        return false;
    }
    if num_chunks < UA_MIN_CHUNKS || num_chunks > UA_MAX_CHUNKS {
        return false;
    }
    if max_chunks < UA_MIN_MAX_CHUNKS || max_chunks > UA_MAX_MAX_CHUNKS || num_chunks > max_chunks {
        return false;
    }
    if max % UA_CHUNK_ALIGN != 0 {
        return false;
    }
    let per_chunk = max / max_chunks;
    if per_chunk % UA_CHUNK_ALIGN != 0
        || per_chunk < UA_MIN_PER_CHUNK
        || per_chunk > UA_MAX_PER_CHUNK
    {
        return false;
    }
    if max / per_chunk != max_chunks {
        return false;
    }
    (num / per_chunk) + 1 == num_chunks
}

fn objects_contain(a: usize, world: usize) -> bool {
    let num = read_u32(a + OFF_NUM_ELEMENTS).unwrap_or(0);
    let max = read_u32(a + OFF_MAX_ELEMENTS).unwrap_or(0);
    let max_chunks = read_u32(a + OFF_MAX_CHUNKS).unwrap_or(0);
    let num_chunks = read_u32(a + OFF_NUM_CHUNKS).unwrap_or(0);
    if max_chunks == 0 {
        return false;
    }
    let per_chunk = max / max_chunks;
    if per_chunk == 0 {
        return false;
    }
    let chunks = read_u64(a).unwrap_or(0) as usize;
    if chunks == 0 {
        return false;
    }
    let mut c = 0u32;
    while c < num_chunks {
        if read_u64(chunks + c as usize * 8).unwrap_or(0) == 0 {
            return false;
        }
        c += 1;
    }
    let mut i = 0u32;
    while i < num {
        let chunk = read_u64(chunks + (i / per_chunk) as usize * 8).unwrap_or(0) as usize;
        if chunk == 0 {
            return false;
        }
        let obj = read_u64(chunk + (i % per_chunk) as usize * FUOBJECT_ITEM).unwrap_or(0) as usize;
        if obj == world {
            return true;
        }
        i += 1;
    }
    false
}

fn discover_objects(module: &Module, world: usize) -> Option<usize> {
    scan(module, 0x20, |at, b, i| {
        check_objects(b, i) && objects_contain(at, world)
    })
}

fn names_world(pool: usize, world: usize) -> bool {
    let class = read_u64(world + OFF_CLASS).unwrap_or(0) as usize;
    if class == 0 {
        return false;
    }
    let id = read_u32(class + OFF_NAME).unwrap_or(0);
    let mut buf = [0u8; 8];
    let n = read_fname(pool, id, &mut buf);
    // Same two-character suffix tolerance the rest of the file uses, so a uniquified class name cannot disable the whole plugin.
    n >= 5 && n <= 7 && &buf[..5] == b"World"
}

fn discover_names(module: &Module, world: usize) -> Option<usize> {
    scan(module, 0x30, |at, b, i| {
        if rd_u32(b, i) != 0 {
            return false;
        }
        let cur_block = rd_u32(b, i + 0x08);
        let cur_cursor = rd_u32(b, i + 0x0C);
        if cur_block as usize >= FNAME_BLOCKS_MAX {
            return false;
        }
        if cur_cursor < 0x10 || cur_cursor as usize >= FNAME_BLOCK_BYTES_MAX {
            return false;
        }
        let mut k = 0usize;
        while k < 4 {
            if !heapish(module, rd_u64(b, i + POOL_BLOCKS + k * 8)) {
                return false;
            }
            k += 1;
        }
        names_world(at, world)
    })
}

/// Finds the GWorld slot, GObjects and GNames, retrying a few ticks apart.
fn ensure_globals(module: &Module) -> bool {
    if WORLD_SLOT.load(Ordering::Relaxed) != 0 && gobjects() != 0 && gnames() != 0 {
        return true;
    }
    let tries = RESOLVE_TRIES.fetch_add(1, Ordering::Relaxed);
    if tries < NEXT_SCAN.load(Ordering::Relaxed) {
        return false;
    }
    // wait 4 ticks for next gworld scan
    NEXT_SCAN.store(tries + 4, Ordering::Relaxed);

    if WORLD_SLOT.load(Ordering::Relaxed) == 0 {
        match discover_gworld(module) {
            Some(slot) => {
                WORLD_SLOT.store(slot, Ordering::Relaxed);
                let world = read_u64(slot).unwrap_or(0) as usize;
                log_gworld(slot, world, module);
            }
            None => {
                if tries == 0 {
                    log::line("GWorld not found yet; the scan will retry");
                }
                return false;
            }
        }
    }

    let world = read_u64(WORLD_SLOT.load(Ordering::Relaxed)).unwrap_or(0) as usize;
    if world == 0 {
        return false;
    }
    if gobjects() == 0 {
        match discover_objects(module, world) {
            Some(a) => {
                GOBJECTS.store(a, Ordering::Relaxed);
                log_global("GObjects", a, module);
            }
            None => {
                if tries == 0 {
                    log::line("GObjects not found yet; the scan will retry");
                }
            }
        }
    }
    if gnames() == 0 {
        match discover_names(module, world) {
            Some(a) => {
                GNAMES.store(a, Ordering::Relaxed);
                log_global("GNames", a, module);
            }
            None => {
                if tries == 0 {
                    log::line("GNames not found yet; the scan will retry");
                }
            }
        }
    }
    gobjects() != 0 && gnames() != 0
}

fn log_global(label: &str, addr: usize, module: &Module) {
    let mut b = Buf::new();
    b.push_str(label);
    b.push_str(" rva 0x");
    b.push_hex(addr.wrapping_sub(module.base) as u64, 0);
    log::log_buf(&b);
}

fn log_gworld(slot: usize, world: usize, module: &Module) {
    let mut b = Buf::new();
    b.push_str("GWorld slot 0x");
    b.push_hex(slot as u64, 0);
    b.push_str(" (RVA 0x");
    b.push_hex(slot.wrapping_sub(module.base) as u64, 0);
    b.push_str("), world 0x");
    b.push_hex(world as u64, 0);
    log::log_buf(&b);
}

fn name_tag(pool: usize, name_id: u32) -> u32 {
    let idx = cache_idx(name_id as usize);
    let v = NAME_TAG[idx].load(Ordering::Relaxed);
    if v & 1 != 0 && ((v >> 32) as u32) == name_id {
        return ((v >> 1) & 0x3) as u32;
    }
    let mut buf = [0u8; MAX_NAME];
    let len = read_fname(pool, name_id, &mut buf);
    let tag = if len > 0 && matches(&buf[..len], DISABLE_NAME) {
        TAG_DISABLE
    } else {
        TAG_OTHER
    };
    NAME_TAG[idx].store(1 | ((tag as u64) << 1) | ((name_id as u64) << 32), Ordering::Relaxed);
    tag
}

fn describe(module: &Module, pool: usize, obj: usize) -> Option<Described> {
    let mut blk = [0u8; OBJ_BLOCK];
    if unsafe { read_raw(obj, blk.as_mut_ptr(), blk.len()) } != blk.len() {
        return None;
    }
    let vt = u64::from_le_bytes(blk[0..8].try_into().ok()?) as usize;
    if vt == 0 || !module.contains(vt) {
        return None;
    }
    let flags = u32::from_le_bytes(blk[8..12].try_into().ok()?);
    if flags & RF_CDO_OR_ARCHETYPE != 0 {
        return None;
    }
    let class = u64::from_le_bytes(blk[0x10..0x18].try_into().ok()?) as usize;
    if class == 0 {
        return None;
    }
    let name_id = u32::from_le_bytes(blk[0x18..0x1C].try_into().ok()?);

    let idx = cache_idx(class >> 3);
    let want_a = (class as u64) | 1;
    let class_tag = if CLASS_OK[idx].load(Ordering::Relaxed) == want_a {
        (CLASS_TAG[idx].load(Ordering::Relaxed) & 0x3) as u32
    } else {
        match read_u64(class) {
            Some(v) if v != 0 && module.contains(v as usize) => {}
            _ => return None,
        }
        let class_id = match read_u32(class + OFF_NAME) {
            Some(v) => v,
            None => return None,
        };
        let mut buf = [0u8; 64];
        let len = read_fname(pool, class_id, &mut buf);
        if len == 0 {
            return None;
        }
        let tag = if matches(&buf[..len], CAM_CLASS) {
            TAG_CAM
        } else if matches(&buf[..len], FUNC_CLASS) {
            TAG_FUNC
        } else {
            TAG_OTHER
        };
        CLASS_OK[idx].store(want_a, Ordering::Relaxed);
        CLASS_TAG[idx].store(tag as u64, Ordering::Relaxed);
        tag
    };

    Some(Described {
        class_tag,
        name_id,
    })
}

fn class_name(module: &Module, pool: usize, obj: usize, out: &mut [u8]) -> usize {
    let class = class_of(obj);
    if class == 0 {
        return 0;
    }
    // a class lives on the heap, but its vtable has to sit inside the game image
    match read_u64(class) {
        Some(v) if module.contains(v as usize) => {}
        _ => return 0,
    }
    let id = match read_u32(class + OFF_NAME) {
        Some(v) => v,
        None => return 0,
    };
    read_fname(pool, id, out)
}

fn class_of(obj: usize) -> usize {
    read_u64(obj + OFF_CLASS).unwrap_or(0) as usize
}

fn push_class_name(b: &mut Buf, module: &Module, pool: usize, obj: usize) {
    let mut buf = [0u8; MAX_NAME];
    let len = class_name(module, pool, obj, &mut buf);
    if len == 0 {
        b.push_byte(b'?');
    } else {
        b.push_bytes(&buf[..len]);
    }
}

fn is_disable_func(module: &Module, pool: usize, func: usize) -> bool {
    match describe(module, pool, func) {
        Some(d) if d.class_tag == TAG_FUNC => name_tag(pool, d.name_id) == TAG_DISABLE,
        _ => false,
    }
}

unsafe fn call_disable(module: &Module, cam: usize, func: usize) -> bool {
    let vt = match read_u64(cam) {
        Some(v) if v != 0 => v as usize,
        _ => return false,
    };
    if !module.contains(vt) {
        return false;
    }
    let pe = match read_u64(vt + PROCESSEVENT_IDX * 8) {
        Some(v) if v != 0 => v as usize,
        _ => return false,
    };
    if !module.contains(pe) {
        return false;
    }
    type PeFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void);
    let f: PeFn = core::mem::transmute(pe as *const c_void);
    f(
        cam as *mut c_void,
        func as *mut c_void,
        core::ptr::null_mut(),
    );
    true
}

fn remember_cam(cam: usize) -> bool {
    let n = KNOWN_N.load(Ordering::Relaxed) as usize;
    let mut i = 0usize;
    while i < n && i < KNOWN.len() {
        if KNOWN[i].load(Ordering::Relaxed) == cam {
            return false;
        }
        i += 1;
    }
    if n < KNOWN.len() {
        KNOWN[n].store(cam, Ordering::Relaxed);
        KNOWN_N.store((n + 1) as u64, Ordering::Relaxed);
        return true;
    }
    // ring overwrite; entries are revalidated every tick anyway
    static CURSOR: AtomicUsize = AtomicUsize::new(0);
    let c = CURSOR.fetch_add(1, Ordering::Relaxed) % KNOWN.len();
    KNOWN[c].store(cam, Ordering::Relaxed);
    true
}

fn log_camera(module: &Module, pool: usize, cam: usize) {
    if CAM_EVER_LOGGED.swap(1, Ordering::Relaxed) == 0 {
        let mut b = Buf::new();
        b.push_str("camera 0x");
        b.push_hex(cam as u64, 0);
        b.push_str(" '");
        push_class_name(&mut b, module, pool, cam);
        b.push_byte(b'\'');
        log::log_buf(&b);
    }
}

pub fn tick(module: &Module) {
    if !ensure_globals(module) {
        return;
    }
    let Some(mgr) = resolve_manager(module) else {
        return;
    };
    if is_cdo_or_archetype(mgr) {
        return;
    }
    let pool = gnames();

    // revalidate the cached Disable UFunction
    let mut func = FUNC.load(Ordering::Relaxed);
    if func != 0 && !is_disable_func(module, pool, func) {
        FUNC.store(0, Ordering::Relaxed);
        func = 0;
    }
    if func == 0 {
        FUNC_STALE.fetch_add(1, Ordering::Relaxed);
    } else {
        FUNC_STALE.store(0, Ordering::Relaxed);
    }

    // revalidate known cameras: drop the dead
    let alive: u64;
    {
        let mut n = KNOWN_N.load(Ordering::Relaxed) as usize;
        if n > KNOWN.len() {
            n = KNOWN.len();
        }
        let mut w = 0usize;
        let mut k = 0usize;
        while k < n {
            let p = KNOWN[k].load(Ordering::Relaxed);
            // looks_alive rejects reused objects whose class/name bytes still read back as a camera but whose vtable was overwritten.
            if p != 0 && valid_object(module, p) && describe(module, pool, p).is_some_and(|d| d.class_tag == TAG_CAM) {
                KNOWN[w].store(p, Ordering::Relaxed);
                w += 1;
            }
            k += 1;
        }
        while w < n {
            KNOWN[w].store(0, Ordering::Relaxed);
            w += 1;
        }
        KNOWN_N.store(w as u64, Ordering::Relaxed);
        alive = w as u64;
    }

    if func != 0 {
        let n = KNOWN_N.load(Ordering::Relaxed) as usize;
        let mut k = 0usize;
        while k < n && k < KNOWN.len() {
            let p = KNOWN[k].load(Ordering::Relaxed);
            if p != 0 && unsafe { call_disable(module, p, func) } {
                CAM_CALLS.fetch_add(1, Ordering::Relaxed);
            }
            k += 1;
        }
    }

    let new_mgr = CUR_MANAGER.swap(mgr, Ordering::Relaxed) != mgr;
    if new_mgr {
        let mut b = Buf::new();
        b.push_str("scene 0x");
        b.push_hex(mgr as u64, 0);
        log::log_buf(&b);
    }
    let need = new_mgr
        || alive != CAMS_LAST.load(Ordering::Relaxed)
        || (func == 0 && FUNC_STALE.load(Ordering::Relaxed) >= 4);
    if need {
        full_scan(module, pool, func);
    }
}

fn full_scan(module: &Module, pool: usize, mut func: usize) {
    let t0 = unsafe { ffi::GetTickCount64() };
    let chunks = chunks_ptr(gobjects());
    let total = count(gobjects());
    if chunks == 0 || total == 0 {
        return;
    }
    KNOWN_N.store(0, Ordering::Relaxed);

    let mut chunk_idx = u32::MAX;
    let mut chunk = 0usize;
    let mut cams = 0u64;
    let mut i = 0u32;
    while i < total {
        let cidx = i >> 16;
        if cidx != chunk_idx {
            chunk = chunk_at(chunks, cidx);
            chunk_idx = cidx;
            if chunk == 0 {
                i = (cidx + 1) << 16;
                continue;
            }
        }
        let obj = item_at(chunk, i);
        i += 1;
        if obj == 0 {
            continue;
        }
        let d = match describe(module, pool, obj) {
            Some(v) => v,
            None => continue,
        };
        if d.class_tag == TAG_CAM {
            cams += 1;
            if remember_cam(obj) {
                log_camera(module, pool, obj);
            }
            if func != 0 && unsafe { call_disable(module, obj, func) } {
                CAM_CALLS.fetch_add(1, Ordering::Relaxed);
            }
        } else if d.class_tag == TAG_FUNC {
            if func == 0 && name_tag(pool, d.name_id) == TAG_DISABLE {
                FUNC.store(obj, Ordering::Relaxed);
                func = obj;
                log::line("Camera UFunction acquired");
            }
        }
    }

    LAST_TOTAL.store(total as u64, Ordering::Relaxed);
    FUNC_STALE.store(if func == 0 { u32::MAX as u64 } else { 0 }, Ordering::Relaxed);
    CAMS_SEEN.store(cams, Ordering::Relaxed);
    CAMS_LAST.store(KNOWN_N.load(Ordering::Relaxed), Ordering::Relaxed);
    SCANS.fetch_add(1, Ordering::Relaxed);
    SCAN_MS.store(unsafe { ffi::GetTickCount64() }.wrapping_sub(t0), Ordering::Relaxed);
}

pub fn walk_ms() -> u64 {
    SCAN_MS.load(Ordering::Relaxed)
}

pub fn scans() -> u64 {
    SCANS.load(Ordering::Relaxed)
}

pub fn cam_calls() -> u64 {
    CAM_CALLS.load(Ordering::Relaxed)
}

pub fn cams_last() -> u64 {
    CAMS_SEEN.load(Ordering::Relaxed)
}

pub fn known() -> u64 {
    KNOWN_N.load(Ordering::Relaxed)
}

pub fn func_ok() -> bool {
    FUNC.load(Ordering::Relaxed) != 0
}
