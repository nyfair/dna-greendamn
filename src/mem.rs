use core::ffi::c_void;

use crate::ffi;

pub struct Module {
    pub base: usize,
    pub size: usize,
}

impl Module {
    pub fn find(name: &[u8]) -> Option<Module> {
        let mut w = [0u16; 64];
        if name.len() + 1 > w.len() {
            return None;
        }
        let mut i = 0;
        while i < name.len() {
            w[i] = name[i] as u16;
            i += 1;
        }
        w[name.len()] = 0;

        let h = unsafe { ffi::GetModuleHandleW(w.as_ptr()) };
        if h.is_null() {
            return None;
        }
        let base = h as usize;
        let size = unsafe { size_of_image(base)? };
        Some(Module { base, size })
    }

    pub fn end(&self) -> usize {
        self.base + self.size
    }

    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.end()
    }
}

unsafe fn size_of_image(base: usize) -> Option<usize> {
    if *(base as *const u16) != 0x5A4D {
        return None;
    }
    let e_lfanew = *((base + 0x3C) as *const u32) as usize;
    let pe = base + e_lfanew;
    if *(pe as *const u32) != 0x0000_4550 {
        return None;
    }
    let opt = pe + 24;
    if *(opt as *const u16) != 0x20B {
        return None;
    }
    Some(*((opt + 0x38) as *const u32) as usize)
}

pub unsafe fn read_raw(addr: usize, out: *mut u8, len: usize) -> usize {
    if addr == 0 || len == 0 {
        return 0;
    }
    let mut got: usize = 0;
    let ok = ffi::ReadProcessMemory(
        ffi::GetCurrentProcess(),
        addr as *const c_void,
        out as *mut c_void,
        len,
        &mut got,
    );
    if ok == 0 {
        0
    } else {
        got
    }
}

pub fn read_u16(addr: usize) -> Option<u16> {
    let mut v: u16 = 0;
    let got = unsafe { read_raw(addr, &mut v as *mut u16 as *mut u8, 2) };
    if got == 2 {
        Some(v)
    } else {
        None
    }
}

pub fn read_u32(addr: usize) -> Option<u32> {
    let mut v: u32 = 0;
    let got = unsafe { read_raw(addr, &mut v as *mut u32 as *mut u8, 4) };
    if got == 4 {
        Some(v)
    } else {
        None
    }
}

pub fn read_u64(addr: usize) -> Option<u64> {
    let mut v: u64 = 0;
    let got = unsafe { read_raw(addr, &mut v as *mut u64 as *mut u8, 8) };
    if got == 8 {
        Some(v)
    } else {
        None
    }
}
