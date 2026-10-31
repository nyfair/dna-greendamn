use core::cmp::min;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::xx::buf::Buf;
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
// AController::Pawn
const OFF_PC_PAWN: usize = 0x2D8;
// APlayerCameraManager::{ViewTarget, PendingViewTarget}, FTViewTarget::Target @ +0x00
const OFF_MANAGER_VIEW_TARGET: usize = 0x1020;
const OFF_MANAGER_PENDING_TARGET: usize = 0x16B0;
// AActor::RootComponent, USceneComponent::AttachChildren
const OFF_ACTOR_ROOT: usize = 0x168;
const OFF_SCENE_CHILDREN: usize = 0xD8;
// ABP_PlayerCharacter_C::CharCameraComponent (CppSDK/SDK/BP_PlayerCharacter_classes.hpp)
const OFF_PAWN_CAMERA: usize = 0x3108;
// TArray<UPlayer*>: Data @ +0x00, Num @ +0x08
const OFF_TARRAY_NUM: usize = 0x08;
// Offsets::ProcessEventIdx
const PROCESSEVENT_IDX: usize = 0x44;

// ---- object / array / FName ----
const FNAME_BLOCKS_MAX: usize = 8192;
const FNAME_BLOCK_BYTES_MAX: usize = 0x2_0000;

// object header block
const OBJ_BLOCK: usize = 0x28;
// UE object header: VTable(0x00) ObjectFlags(0x08) InternalIndex(0x0C) Class(0x10) Name(0x18) Outer(0x20)
const OFF_FLAGS: usize = 0x08;
const OFF_CLASS: usize = 0x10;
const OFF_NAME: usize = 0x18;
const OFF_OUTER: usize = 0x20;
// RF_ClassDefaultObject(0x10) | RF_ArchetypeObject(0x20): never live instances
const RF_CDO_OR_ARCHETYPE: u32 = 0x10 | 0x20;
const RF_DESTROYED: u32 = 0x8000 | 0x10000;
// UField::Next, UStruct::{Super, Children}, UFunction::FunctionFlags
// (CppSDK/SDK/CoreUObject_classes.hpp)
const OFF_FIELD_NEXT: usize = 0x28;
const OFF_SUPER_STRUCT: usize = 0x40;
const OFF_STRUCT_CHILDREN: usize = 0x48;
const OFF_FUNCTION_FLAGS: usize = 0xB0;
const FUNC_NATIVE: u32 = 0x400;

const POOL_BLOCKS: usize = 0x10;
const BLOCK_SHIFT: u32 = 16;
const BLOCK_MASK: u32 = 0xFFFF;
const ENTRY_HEADER: usize = 2;
const MAX_NAME: usize = 64;

const PLAYER_CLASS: &[u8] = b"BP_PlayerCharacter_C";
const CAMERA_CLASS: &[u8] = b"PlayerCameraComponent";
const FUNCTION_CLASS: &[u8] = b"Function";
const FN_DITHER: &[u8] = b"DisableCameraDither";

const SCAN_CHUNK: usize = 0x10000;
const MAX_SUPER_DEPTH: usize = 64;
const MAX_FIELDS: usize = 512;
const MAX_SCENE: usize = 256;
// Only the local pawn and the current/pending view targets are visited.
const MAX_CAMERA_OBJS: usize = 16;

static WORLD_SLOT: AtomicUsize = AtomicUsize::new(0);
static GNAMES: AtomicUsize = AtomicUsize::new(0);
static RESOLVE_TRIES: AtomicU64 = AtomicU64::new(0);
static NEXT_SCAN: AtomicU64 = AtomicU64::new(0);

static CUR_MANAGER: AtomicUsize = AtomicUsize::new(0);
static CUR_CAMERA: AtomicUsize = AtomicUsize::new(0);
static FUNC_DITHER: AtomicUsize = AtomicUsize::new(0);
static CAMERA_N: AtomicUsize = AtomicUsize::new(0);
static DITHER_CALLS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct Described {
    vtable: usize,
    class: usize,
    name_id: u32,
    outer: usize,
}

#[derive(Clone, Copy)]
struct Camera {
    addr: usize,
    class: usize,
    owner: usize,
}

const EMPTY_CAMERA: Camera = Camera {
    addr: 0,
    class: 0,
    owner: 0,
};

fn gnames() -> usize {
    GNAMES.load(Ordering::Relaxed)
}

/// Reads the object header once; rejects dead objects and non-instance templates.
fn describe(module: &Module, obj: usize) -> Option<Described> {
    if obj == 0 || obj % 8 != 0 {
        return None;
    }
    let mut blk = [0u8; OBJ_BLOCK];
    if unsafe { read_raw(obj, blk.as_mut_ptr(), blk.len()) } != blk.len() {
        return None;
    }
    let vtable = rd_u64(&blk, 0) as usize;
    let flags = rd_u32(&blk, OFF_FLAGS);
    let class = rd_u64(&blk, OFF_CLASS) as usize;
    if !module.contains(vtable)
        || flags & (RF_CDO_OR_ARCHETYPE | RF_DESTROYED) != 0
        || class == 0
        || class % 8 != 0
    {
        return None;
    }
    Some(Described {
        vtable,
        class,
        name_id: rd_u32(&blk, OFF_NAME),
        outer: rd_u64(&blk, OFF_OUTER) as usize,
    })
}

fn valid_object(module: &Module, obj: usize) -> bool {
    describe(module, obj).is_some()
}

fn array(addr: usize, limit: u32) -> Option<(usize, u32)> {
    let data = read_u64(addr)? as usize;
    let num = read_u32(addr + OFF_TARRAY_NUM)?;
    if data == 0 || num == 0 || num > limit {
        return None;
    }
    Some((data, num))
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
/// Returns None when any required link is missing or invalid.
fn resolve_world(module: &Module, world: usize) -> Option<(usize, usize)> {
    if !valid_object(module, world) {
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

fn discover_gworld(module: &Module) -> Option<usize> {
    scan(module, 8, |_at, b, i| {
        let world = rd_u64(b, i) as usize;
        world != 0
            && (world & 7) == 0
            && !module.contains(world)
            && resolve_world(module, world).is_some()
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

/// Finds the GWorld slot and GNames, retrying a few ticks apart.
fn ensure_globals(module: &Module) -> bool {
    if WORLD_SLOT.load(Ordering::Relaxed) != 0 && gnames() != 0 {
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
    if !valid_object(module, world) {
        return false;
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
    gnames() != 0
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

fn named(pool: usize, name_id: u32, target: &[u8]) -> bool {
    let mut buf = [0u8; MAX_NAME];
    let len = read_fname(pool, name_id, &mut buf);
    len > 0 && matches(&buf[..len], target)
}

fn find_class(module: &Module, pool: usize, class: usize, target: &[u8]) -> usize {
    let mut cur = class;
    let mut depth = 0usize;
    while cur != 0 && depth < MAX_SUPER_DEPTH {
        let Some(d) = describe(module, cur) else {
            return 0;
        };
        if named(pool, d.name_id, target) {
            return cur;
        }
        cur = read_u64(cur + OFF_SUPER_STRUCT).unwrap_or(0) as usize;
        depth += 1;
    }
    0
}

fn class_chain_matches(module: &Module, pool: usize, class: usize, target: &[u8]) -> bool {
    find_class(module, pool, class, target) != 0
}

fn camera_object(module: &Module, pool: usize, obj: usize) -> Option<Camera> {
    let d = describe(module, obj)?;
    let class = find_class(module, pool, d.class, CAMERA_CLASS);
    if class == 0 {
        return None;
    }
    Some(Camera {
        addr: obj,
        class: d.class,
        owner: class,
    })
}

fn push_camera(cameras: &mut [Camera], n: &mut usize, camera: Camera) {
    if *n < cameras.len() && !cameras[..*n].iter().any(|c| c.addr == camera.addr) {
        cameras[*n] = camera;
        *n += 1;
    }
}

fn push_unique(objects: &mut [usize], n: &mut usize, obj: usize) {
    if obj != 0 && *n < objects.len() && !objects[..*n].contains(&obj) {
        objects[*n] = obj;
        *n += 1;
    }
}

/// Prefer the player's camera reference; other view targets use a bounded component walk.
/// All references are read again next tick, including while components are still loading.
fn collect_cameras(
    module: &Module,
    pool: usize,
    actor: usize,
    cameras: &mut [Camera],
    n: &mut usize,
) {
    let Some(d) = describe(module, actor) else {
        return;
    };
    // Only this Blueprint class has CharCameraComponent at OFF_PAWN_CAMERA.
    if class_chain_matches(module, pool, d.class, PLAYER_CLASS) {
        let cam = read_u64(actor + OFF_PAWN_CAMERA).unwrap_or(0) as usize;
        if let Some(camera) = camera_object(module, pool, cam) {
            push_camera(cameras, n, camera);
            return;
        }
    }

    let root = read_u64(actor + OFF_ACTOR_ROOT).unwrap_or(0) as usize;
    let mut scene = [0usize; MAX_SCENE];
    let mut scene_n = 0usize;
    push_unique(&mut scene, &mut scene_n, root);
    let mut i = 0usize;
    while i < scene_n && *n < cameras.len() {
        let obj = scene[i];
        i += 1;
        let Some(d) = describe(module, obj) else {
            continue;
        };
        let class = find_class(module, pool, d.class, CAMERA_CLASS);
        if class != 0 {
            push_camera(cameras, n, Camera {
                addr: obj,
                class: d.class,
                owner: class,
            });
        }
        if let Some((data, num)) = array(obj + OFF_SCENE_CHILDREN, MAX_SCENE as u32) {
            let mut k = 0u32;
            while k < num && scene_n < scene.len() {
                let child = read_u64(data + k as usize * 8).unwrap_or(0) as usize;
                push_unique(&mut scene, &mut scene_n, child);
                k += 1;
            }
        }
    }
}

fn is_disable_func(module: &Module, pool: usize, func: usize, owner: usize) -> bool {
    let Some(d) = describe(module, func) else {
        return false;
    };
    disable_func_matches(module, pool, func, owner, &d)
}

fn disable_func_matches(
    module: &Module,
    pool: usize,
    func: usize,
    owner: usize,
    d: &Described,
) -> bool {
    d.outer == owner
        && named(pool, d.name_id, FN_DITHER)
        && class_chain_matches(module, pool, d.class, FUNCTION_CLASS)
        && read_u32(func + OFF_FUNCTION_FLAGS).is_some_and(|f| f & FUNC_NATIVE != 0)
}

/// Mirrors UClass::GetFunction: search this class's Children, not the global object array.
fn find_function(module: &Module, pool: usize, owner: usize) -> usize {
    let mut field = read_u64(owner + OFF_STRUCT_CHILDREN).unwrap_or(0) as usize;
    let mut i = 0usize;
    while field != 0 && i < MAX_FIELDS {
        let Some(d) = describe(module, field) else {
            break;
        };
        if disable_func_matches(module, pool, field, owner, &d) {
            return field;
        }
        field = read_u64(field + OFF_FIELD_NEXT).unwrap_or(0) as usize;
        i += 1;
    }
    0
}

fn ensure_function(module: &Module, pool: usize, owner: usize) -> usize {
    let func = FUNC_DITHER.load(Ordering::Relaxed);
    if func != 0 && is_disable_func(module, pool, func, owner) {
        return func;
    }
    let func = find_function(module, pool, owner);
    FUNC_DITHER.store(func, Ordering::Relaxed);
    if func != 0 {
        log::line("Camera UFunction acquired");
    }
    func
}

unsafe fn call_disable(module: &Module, camera: &Camera, func: usize) -> bool {
    // Recheck immediately before dispatch: the engine can destroy a collected target.
    // A single header read supplies flags, class and vtable for all object checks.
    let Some(d) = describe(module, camera.addr) else {
        return false;
    };
    if func == 0 || d.class != camera.class {
        return false;
    }
    let pe = match read_u64(d.vtable + PROCESSEVENT_IDX * 8) {
        Some(v) if v != 0 => v as usize,
        _ => return false,
    };
    if !module.contains(pe) {
        return false;
    }
    type PeFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void);
    let f: PeFn = core::mem::transmute(pe as *const c_void);
    f(
        camera.addr as *mut c_void,
        func as *mut c_void,
        core::ptr::null_mut(),
    );
    true
}

fn log_object(label: &str, obj: usize) {
    let mut b = Buf::new();
    b.push_str(label);
    b.push_str(" 0x");
    b.push_hex(obj as u64, 0);
    log::log_buf(&b);
}

pub fn tick(module: &Module) {
    // Heartbeat counts describe this tick, not an old scan or stale cache.
    CAMERA_N.store(0, Ordering::Relaxed);
    if !ensure_globals(module) {
        return;
    }
    let world = read_u64(WORLD_SLOT.load(Ordering::Relaxed)).unwrap_or(0) as usize;
    let Some((controller, mgr)) = resolve_world(module, world) else {
        CUR_MANAGER.store(0, Ordering::Relaxed);
        CUR_CAMERA.store(0, Ordering::Relaxed);
        FUNC_DITHER.store(0, Ordering::Relaxed);
        return;
    };
    let pool = gnames();
    if CUR_MANAGER.swap(mgr, Ordering::Relaxed) != mgr {
        log_object("manager", mgr);
    }

    let mut actors = [0usize; 3];
    let mut actors_n = 0usize;
    for addr in [
        controller + OFF_PC_PAWN,
        mgr + OFF_MANAGER_VIEW_TARGET,
        mgr + OFF_MANAGER_PENDING_TARGET,
    ] {
        push_unique(&mut actors, &mut actors_n, read_u64(addr).unwrap_or(0) as usize);
    }
    let mut cameras = [EMPTY_CAMERA; MAX_CAMERA_OBJS];
    let mut n = 0usize;
    for actor in &actors[..actors_n] {
        collect_cameras(module, pool, *actor, &mut cameras, &mut n);
    }
    CAMERA_N.store(n, Ordering::Relaxed);
    let primary = if n == 0 { 0 } else { cameras[0].addr };
    if CUR_CAMERA.swap(primary, Ordering::Relaxed) != primary {
        log_object("camera", primary);
    }
    if n == 0 {
        FUNC_DITHER.store(0, Ordering::Relaxed);
        return;
    }

    for camera in &cameras[..n] {
        let func = ensure_function(module, pool, camera.owner);
        // Dialogue and scene events can restore dither on the same component.
        // Always reapply; DitherDisabled is not a reliable effect-state witness.
        if func != 0 && unsafe { call_disable(module, camera, func) } {
            DITHER_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn dither_calls() -> u64 {
    DITHER_CALLS.load(Ordering::Relaxed)
}

pub fn cameras() -> u64 {
    CAMERA_N.load(Ordering::Relaxed) as u64
}

pub fn func_ok() -> bool {
    FUNC_DITHER.load(Ordering::Relaxed) != 0
}
