#![no_std]
#![allow(non_camel_case_types, non_snake_case, unsafe_op_in_unsafe_fn)]

mod buf;
mod dither;
mod ffi;
mod log;
mod mem;
mod names;

use core::ffi::c_void;
use core::panic::PanicInfo;

use buf::Buf;

const TARGET_MODULE: &[u8] = b"EM-Win64-Shipping.exe";
const TICK_MS: u32 = 2_500;
const HEARTBEAT_MS: u64 = 60_000;

#[panic_handler]
fn on_panic(_info: &PanicInfo) -> ! {
    log::line("dna-greendamn panic!");
    loop {
        unsafe { ffi::Sleep(HEARTBEAT_MS as u32) };
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    hinst: ffi::HINSTANCE,
    reason: u32,
    _: *mut c_void,
) -> ffi::BOOL {
    if reason == ffi::DLL_PROCESS_ATTACH {
        unsafe {
            ffi::DisableThreadLibraryCalls(hinst);
            let h = ffi::CreateThread(
                core::ptr::null_mut(),
                0,
                worker_thread,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
            );
            if !h.is_null() {
                ffi::CloseHandle(h);
            }
        }
    }
    ffi::TRUE
}

extern "system" fn worker_thread(_param: *mut c_void) -> ffi::DWORD {
    run();
    0
}

pub fn log_buf(b: &Buf) {
    log::line(unsafe { core::str::from_utf8_unchecked(b.as_bytes()) });
}

fn run() {
    if !log::init() {
        return;
    }
    let module = match wait_for_module(TARGET_MODULE) {
        Some(m) => m,
        None => {
            log::line("Not loaded by EM-Win64-Shipping.exe");
            return;
        }
    };

    let mut b = Buf::new();
    b.push_str("EM-Win64-Shipping.exe @ 0x");
    b.push_hex(module.base as u64, 0);
    b.push_str(" size=0x");
    b.push_hex(module.size as u64, 0);
    log_buf(&b);

    let mut last_report = unsafe { ffi::GetTickCount64() };

    loop {
        let now = unsafe { ffi::GetTickCount64() };
        dither::tick(&module);
        if now.wrapping_sub(last_report) >= HEARTBEAT_MS {
            last_report = now;
            let mut b = Buf::new();
            b.push_str("called ");
            b.push_u64(dither::cam_calls());
            b.push_str(" times, cameras ");
            b.push_u64(dither::cams_last());
            b.push_str(", known ");
            b.push_u64(dither::known());
            b.push_str(", walk ");
            b.push_u64(dither::walk_ms());
            b.push_str("ms x");
            b.push_u64(dither::scans());
            b.push_str(", func ");
            b.push_str(if dither::func_ok() { "ok" } else { "MISSING" });
            log_buf(&b);
        }

        unsafe { ffi::Sleep(TICK_MS) };
    }
}

fn wait_for_module(name: &[u8]) -> Option<mem::Module> {
    let mut waited: u32 = 0;
    loop {
        if let Some(m) = mem::Module::find(name) {
            return Some(m);
        }
        if waited >= 30_000 {
            return None;
        }
        unsafe { ffi::Sleep(500) };
        waited += 500;
    }
}
