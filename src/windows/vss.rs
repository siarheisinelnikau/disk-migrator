use crate::{
    Error, Result, invalid,
    snapshot::{SnapshotProvider, WriterStatus},
};
use std::{
    ffi::c_void,
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

type Cancelled = extern "C" fn(*const c_void) -> u32;
#[repr(C)]
struct NativeWriter {
    name: [u16; 256],
    id: [u16; 40],
    state: u32,
    failure: i32,
}
unsafe extern "C" {
    fn dm_vss_create(output: *mut *mut c_void) -> i32;
    fn dm_vss_initialize(session: *mut c_void) -> i32;
    fn dm_vss_metadata(session: *mut c_void, cancel: Cancelled, context: *const c_void) -> i32;
    fn dm_vss_begin(session: *mut c_void, output: *mut u16, capacity: u32) -> i32;
    fn dm_vss_add(session: *mut c_void, volume: *const u16, output: *mut u16, capacity: u32)
    -> i32;
    fn dm_vss_prepare(session: *mut c_void, cancel: Cancelled, context: *const c_void) -> i32;
    fn dm_vss_snapshot(session: *mut c_void, cancel: Cancelled, context: *const c_void) -> i32;
    fn dm_vss_writers(
        session: *mut c_void,
        cancel: Cancelled,
        context: *const c_void,
        output: *mut NativeWriter,
        capacity: u32,
        count: *mut u32,
    ) -> i32;
    fn dm_vss_path(session: *mut c_void, id: *const u16, output: *mut u16, capacity: u32) -> i32;
    fn dm_vss_complete(session: *mut c_void, cancel: Cancelled, context: *const c_void) -> i32;
    fn dm_vss_abort(session: *mut c_void) -> i32;
    fn dm_vss_delete(session: *mut c_void) -> i32;
    fn dm_vss_destroy(session: *mut c_void);
}
/// COM apartment and IVssBackupComponents stay on their creating worker thread.
pub struct WindowsVss {
    raw: *mut c_void,
    _thread: PhantomData<Rc<()>>,
}
impl WindowsVss {
    pub fn new() -> Self {
        Self {
            raw: std::ptr::null_mut(),
            _thread: PhantomData,
        }
    }
    fn ptr(&self) -> Result<*mut c_void> {
        if self.raw.is_null() {
            Err(invalid("VSS requester is not initialized."))
        } else {
            Ok(self.raw)
        }
    }
}
impl Default for WindowsVss {
    fn default() -> Self {
        Self::new()
    }
}
extern "C" fn cancelled(context: *const c_void) -> u32 {
    unsafe { (&*context.cast::<AtomicBool>()).load(Ordering::Relaxed) as u32 }
}
fn check(hr: i32, operation: &str) -> Result<()> {
    if hr == 0x800704c7u32 as i32 {
        Err(Error::Cancelled)
    } else if hr < 0 {
        Err(invalid(format!(
            "VSS {operation} failed: HRESULT 0x{:08x}. Run explicitly as administrator and check Windows VSS writer/provider health; no elevation prompt is started by Disk Migrator.",
            hr as u32
        )))
    } else {
        Ok(())
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn string(buffer: &[u16]) -> String {
    String::from_utf16_lossy(&buffer[..buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len())])
}
impl SnapshotProvider for WindowsVss {
    fn initialize(&mut self) -> Result<()> {
        if !self.raw.is_null() {
            return Err(invalid("VSS requester was already initialized."));
        }
        check(unsafe { dm_vss_create(&mut self.raw) }, "create requester")?;
        check(unsafe { dm_vss_initialize(self.raw) }, "initialize backup")
    }
    fn gather_metadata(&mut self, c: &AtomicBool) -> Result<()> {
        check(
            unsafe { dm_vss_metadata(self.ptr()?, cancelled, (c as *const AtomicBool).cast()) },
            "gather writer metadata",
        )
    }
    fn start_set(&mut self) -> Result<String> {
        let mut out = [0; 40];
        check(
            unsafe { dm_vss_begin(self.ptr()?, out.as_mut_ptr(), 40) },
            "start snapshot set",
        )?;
        Ok(string(&out))
    }
    fn add_volume(&mut self, volume: &str) -> Result<String> {
        let mut out = [0; 40];
        check(
            unsafe { dm_vss_add(self.ptr()?, wide(volume).as_ptr(), out.as_mut_ptr(), 40) },
            "add volume",
        )?;
        Ok(string(&out))
    }
    fn prepare(&mut self, c: &AtomicBool) -> Result<()> {
        check(
            unsafe { dm_vss_prepare(self.ptr()?, cancelled, (c as *const AtomicBool).cast()) },
            "prepare writers",
        )
    }
    fn take_snapshot(&mut self, c: &AtomicBool) -> Result<()> {
        check(
            unsafe { dm_vss_snapshot(self.ptr()?, cancelled, (c as *const AtomicBool).cast()) },
            "create snapshot set",
        )
    }
    fn writer_status(&mut self, c: &AtomicBool) -> Result<Vec<WriterStatus>> {
        let mut out: Vec<NativeWriter> = (0..256)
            .map(|_| NativeWriter {
                name: [0; 256],
                id: [0; 40],
                state: 0,
                failure: 0,
            })
            .collect();
        let mut count = 0;
        check(
            unsafe {
                dm_vss_writers(
                    self.ptr()?,
                    cancelled,
                    (c as *const AtomicBool).cast(),
                    out.as_mut_ptr(),
                    out.len() as u32,
                    &mut count,
                )
            },
            "gather writer status",
        )?;
        if count as usize > out.len() {
            return Err(invalid("Invalid VSS writer count."));
        }
        Ok(out[..count as usize]
            .iter()
            .map(|w| WriterStatus {
                id: string(&w.id),
                name: string(&w.name),
                state: w.state,
                failure: w.failure,
            })
            .collect())
    }
    fn device_path(&mut self, id: &str) -> Result<String> {
        let mut out = [0; 1024];
        check(
            unsafe { dm_vss_path(self.ptr()?, wide(id).as_ptr(), out.as_mut_ptr(), 1024) },
            "get snapshot properties",
        )?;
        Ok(string(&out))
    }
    fn complete(&mut self, c: &AtomicBool) -> Result<()> {
        check(
            unsafe { dm_vss_complete(self.ptr()?, cancelled, (c as *const AtomicBool).cast()) },
            "complete backup",
        )
    }
    fn abort(&mut self) -> Result<()> {
        if self.raw.is_null() {
            Ok(())
        } else {
            check(unsafe { dm_vss_abort(self.raw) }, "abort backup")
        }
    }
    fn delete_set(&mut self, _: &str) -> Result<()> {
        if self.raw.is_null() {
            Ok(())
        } else {
            check(unsafe { dm_vss_delete(self.raw) }, "delete snapshot set")
        }
    }
}
impl Drop for WindowsVss {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { dm_vss_destroy(self.raw) };
        }
    }
}
