use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::buf::Buf;
use crate::ffi;
use crate::log;
use crate::mem::*;

// Offsets::GNames
const GNAMES_RVA: usize = 0x0747CDC0;
// Offsets::GObjects
const GOBJECTS_RVA: usize = 0x07127C80;
// UObject::ProcessEvent vtable slot, Offsets::ProcessEventIdx
const PROCESSEVENT_IDX: usize = 0x44;
// TUObjectArray @ module + GOBJECTS_RVA.
// OFF_OBJECTS (0x0), OFF_NUM_ELEMENTS (0x14), OFF_NUM_CHUNKS (0x1C),
// ElementsPerChunk (0x10000), FUObjectItem size (0x18)
const OFF_OBJECTS: usize = 0x00;
const OFF_NUM_ELEMENTS: usize = 0x14;
const OFF_NUM_CHUNKS: usize = 0x1C;
const ELEMENTS_PER_CHUNK: u32 = 0x10000;
const FUOBJECT_ITEM: usize = 0x18;
// sanity cap: something is very wrong if the array claims more than this
const MAX_ELEMENTS: u32 = 8_000_000;
const CAM_CLASS: &[u8] = b"PlayerCameraComponent";
const FUNC_CLASS: &[u8] = b"Function";
const DISABLE_NAME: &[u8] = b"DisableCameraDither";
// object header block
const OBJ_BLOCK: usize = 0x28;
// class tags
const TAG_OTHER: u32 = 0;
const TAG_CAM: u32 = 1;
const TAG_FUNC: u32 = 2;
// object-name tags (only resolved for Function objects)
const TAG_DISABLE: u32 = 1;
// direct-mapped caches (2048 slots)
const CACHE_MASK: usize = 0x7FF;
// UE object header: VTable(0x00) ObjectFlags(0x08) InternalIndex(0x0C) Class(0x10) Name(0x18)
const OFF_CLASS: usize = 0x10;
const OFF_NAME: usize = 0x18;
// RF_ClassDefaultObject(0x10) | RF_ArchetypeObject(0x20): never live instances
const SKIP_FLAGS: u32 = 0x30;
const POOL_BLOCKS: usize = 0x10;
const BLOCK_SHIFT: u32 = 16;
const BLOCK_MASK: u32 = 0xFFFF;
const ENTRY_HEADER: usize = 2;
const MAX_NAME: usize = 64;

// class-ptr -> validity (slot = class | 1 when the class vtable is in-module)
static CLASS_OK: [AtomicU64; 2048] = [const { AtomicU64::new(0) }; 2048];
// class-ptr -> class tag, parallel to CLASS_OK
static CLASS_TAG: [AtomicU64; 2048] = [const { AtomicU64::new(0) }; 2048];
// FName id -> object-name tag (slot = valid | tag<<1 | key<<32)
static NAME_TAG: [AtomicU64; 2048] = [const { AtomicU64::new(0) }; 2048];
static FUNC: AtomicUsize = AtomicUsize::new(0);
static CAM_CALLS: AtomicU64 = AtomicU64::new(0);
static CAMS_LAST: AtomicU64 = AtomicU64::new(0);
static KNOWN_N: AtomicU64 = AtomicU64::new(0);
// last full-scan duration (ms) and full-scan count; idle ticks skip the walk
static SCAN_MS: AtomicU64 = AtomicU64::new(0);
static SCANS: AtomicU64 = AtomicU64::new(0);
static KNOWN: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
// GObjects total seen by the last full scan (u32::MAX = none yet)
static LAST_TOTAL: AtomicU64 = AtomicU64::new(u32::MAX as u64);
// consecutive ticks with no usable Disable UFunction
static FUNC_STALE: AtomicU64 = AtomicU64::new(0);

pub fn cam_calls() -> u64 {
    CAM_CALLS.load(Ordering::Relaxed)
}

pub fn cams_last() -> u64 {
    CAMS_LAST.load(Ordering::Relaxed)
}

pub fn known() -> u64 {
    KNOWN_N.load(Ordering::Relaxed)
}

pub fn walk_ms() -> u64 {
    SCAN_MS.load(Ordering::Relaxed)
}

pub fn scans() -> u64 {
    SCANS.load(Ordering::Relaxed)
}

pub fn func_ok() -> bool {
    FUNC.load(Ordering::Relaxed) != 0
}

fn cache_idx(key: usize) -> usize {
    key & CACHE_MASK
}

struct Described {
    class_tag: u32,
    name_id: u32,
}

fn count(base: usize) -> u32 {
    match read_u32(base + GOBJECTS_RVA + OFF_NUM_ELEMENTS) {
        Some(n) if n <= MAX_ELEMENTS => n,
        _ => 0,
    }
}

fn chunks_ptr(base: usize) -> usize {
    let array = base + GOBJECTS_RVA;
    match read_u32(array + OFF_NUM_CHUNKS) {
        Some(n) if n > 0 && n <= 0x1000 => {}
        _ => return 0,
    }
    read_u64(array + OFF_OBJECTS).unwrap_or(0) as usize
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
    read_u64(chunk + (index % ELEMENTS_PER_CHUNK) as usize * FUOBJECT_ITEM).unwrap_or(0)
        as usize
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
    let limit = core::cmp::min(out.len(), len);
    let mut buf = [0u8; MAX_NAME * 2];
    let want = core::cmp::min(if wide { limit * 2 } else { limit }, buf.len());
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

/// appends the class name of `obj` (`?` when it cannot be resolved)
fn push_class_name(b: &mut Buf, module: &Module, pool: usize, obj: usize) {
    let mut buf = [0u8; MAX_NAME];
    let len = class_name(module, pool, obj, &mut buf);
    if len == 0 {
        b.push_byte(b'?');
    } else {
        b.push_bytes(&buf[..len]);
    }
}

/// validate one object and resolve its class tag (cached). None = dead/skip.
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
    if flags & SKIP_FLAGS != 0 {
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
        // class lives on the heap, but its vtable must sit in the game image
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

/// object-name tag for Function objects (cached by FName id).
fn name_tag(pool: usize, name_id: u32) -> u32 {
    let idx = cache_idx(name_id as usize);
    let v = NAME_TAG[idx].load(Ordering::Relaxed);
    if v & 1 != 0 && ((v >> 32) as u32) == name_id {
        return ((v >> 1) & 0x3) as u32;
    }
    let mut buf = [0u8; 64];
    let len = read_fname(pool, name_id, &mut buf);
    let tag = if len > 0 && matches(&buf[..len], DISABLE_NAME) {
        TAG_DISABLE
    } else {
        TAG_OTHER
    };
    NAME_TAG[idx].store(1 | ((tag as u64) << 1) | ((name_id as u64) << 32), Ordering::Relaxed);
    tag
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

/// returns true when the camera was newly added
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

// the first live camera is logged once for sanity.
static CAM_EVER_LOGGED: AtomicUsize = AtomicUsize::new(0);

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

/// Cheap per-tick check: detect whether anything could have changed and only then pay for a full GObjects walk.
pub fn tick(module: &Module) {
    let pool = module.base + GNAMES_RVA;
    let total = count(module.base);

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
            if p != 0 && describe(module, pool, p).is_some_and(|d| d.class_tag == TAG_CAM) {
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

    let last_total = LAST_TOTAL.load(Ordering::Relaxed);
    // full scan when
    // 1. never scanned
    // 2. GObjects grew/shrank (map change, streaming, level load)
    // 3. a known camera died
    // 4. the Disable UFunction has been missing for a while.
    let need = last_total == u32::MAX as u64
        || total != last_total as u32
        || alive != CAMS_LAST.load(Ordering::Relaxed)
        || (func == 0 && FUNC_STALE.load(Ordering::Relaxed) >= 4);
    if !need {
        return;
    }
    full_scan(module, pool, func);
}

/// Full GObjects walk: (re)acquire the Disable UFunction, disable every live camera, record the new baseline.
fn full_scan(module: &Module, pool: usize, mut func: usize) {
    let t0 = unsafe { ffi::GetTickCount64() };
    let chunks = chunks_ptr(module.base);
    let total = count(module.base);
    if chunks == 0 || total == 0 {
        return;
    }

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
    CAMS_LAST.store(cams, Ordering::Relaxed);
    SCANS.fetch_add(1, Ordering::Relaxed);
    SCAN_MS.store(unsafe { ffi::GetTickCount64() }.wrapping_sub(t0), Ordering::Relaxed);
}
