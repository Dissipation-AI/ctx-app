//! Reload the config (and theme files) when they change, with an FSEvents stream on the config
//! folder, delivered on the main queue.

use std::ffi::c_void;
use std::path::Path;

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    describe: *const c_void,
}

type Callback = extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: *const c_void,
        cb: Callback,
        ctx: *mut Context,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(s: *mut c_void, q: *const c_void);
    fn FSEventStreamStart(s: *mut c_void) -> bool;
    fn FSEventStreamStop(s: *mut c_void);
    fn FSEventStreamInvalidate(s: *mut c_void);
    fn FSEventStreamRelease(s: *mut c_void);
}

const SINCE_NOW: u64 = u64::MAX;
const FILE_EVENTS: u32 = 0x10;

extern "C" fn changed(
    _s: *const c_void,
    _info: *mut c_void,
    _n: usize,
    _paths: *mut c_void,
    _flags: *const u32,
    _ids: *const u64,
) {
    super::super::later(|a| a.reload_config());
}

pub struct Watcher {
    stream: *mut c_void,
}

impl Watcher {
    pub fn start(config: &Path) -> Option<Watcher> {
        let dir = config.parent()?;
        let _ = std::fs::create_dir_all(dir);
        let paths = objc2_foundation::NSArray::from_retained_slice(&[objc2_foundation::NSString::from_str(
            &dir.display().to_string(),
        )]);
        let mut ctx = Context {
            version: 0,
            info: std::ptr::null_mut(),
            retain: std::ptr::null(),
            release: std::ptr::null(),
            describe: std::ptr::null(),
        };
        unsafe {
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                changed,
                &mut ctx,
                objc2::rc::Retained::as_ptr(&paths).cast(),
                SINCE_NOW,
                0.3,
                FILE_EVENTS,
            );
            if stream.is_null() {
                return None;
            }
            let q: *const dispatch2::DispatchQueue = dispatch2::DispatchQueue::main();
            FSEventStreamSetDispatchQueue(stream, q.cast());
            if !FSEventStreamStart(stream) {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return None;
            }
            Some(Watcher { stream })
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
        }
    }
}
