//! Tie a launched browser's lifetime to this process on Windows.
//!
//! On Linux/macOS the default pipe transport already does this: Chrome
//! exits when its driver's end of the pipe closes. Windows uses the
//! WebSocket transport, where a killed driver (an agent's session server
//! taken down by `taskkill /F`, a crashed shell) would leave Chrome running.
//! A Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` fixes that: the
//! kernel kills every process in the job when the last handle to it closes,
//! which includes this process dying for any reason. It also lets `close`
//! end the whole browser tree at once, like `killpg` on Unix.
//!
//! Elsewhere this is a no-op.

use std::process::Child;

/// A job holding one browser process tree (`None` handle on other
/// platforms, or when the OS refused the job).
pub struct ProcJob {
    #[cfg(windows)]
    handle: win::Handle,
}

impl ProcJob {
    /// Put `child` in a fresh kill-on-close job. Best effort: `None` when
    /// unsupported or refused (the caller still kills the child directly).
    #[cfg(windows)]
    pub fn contain(child: &Child) -> Option<ProcJob> {
        win::contain(child).map(|handle| ProcJob { handle })
    }

    #[cfg(not(windows))]
    pub fn contain(_child: &Child) -> Option<ProcJob> {
        None
    }

    /// Kill every process in the job now.
    pub fn terminate(&self) {
        #[cfg(windows)]
        win::terminate(&self.handle);
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    type Raw = *mut c_void;

    /// Owned job handle; closing it kills the job (KILL_ON_JOB_CLOSE).
    pub struct Handle(Raw);

    // SAFETY: a kernel handle is just a process-wide index; the job object
    // API is thread-safe.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: we own the handle and close it exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: Raw, name: *const u16) -> Raw;
        fn SetInformationJobObject(job: Raw, class: i32, info: *mut c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Raw, process: Raw) -> i32;
        fn TerminateJobObject(job: Raw, exit_code: u32) -> i32;
        fn CloseHandle(handle: Raw) -> i32;
    }

    pub fn contain(child: &Child) -> Option<Handle> {
        // SAFETY: plain Win32 calls; every pointer is valid for the call and
        // the struct matches the documented layout (#[repr(C)]).
        unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let handle = Handle(job);
            let mut limits = ExtendedLimits::default();
            limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut limits as *mut ExtendedLimits as *mut c_void,
                std::mem::size_of::<ExtendedLimits>() as u32,
            );
            if ok == 0 || AssignProcessToJobObject(job, child.as_raw_handle() as Raw) == 0 {
                return None;
            }
            Some(handle)
        }
    }

    pub fn terminate(handle: &Handle) {
        // SAFETY: valid job handle owned by `handle`.
        unsafe {
            TerminateJobObject(handle.0, 1);
        }
    }
}
