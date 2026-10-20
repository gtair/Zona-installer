//! Win32 job-object wrapper.
//!
//! Child tools (`7z.exe`, `aria2c.exe`) are assigned to a job object configured
//! with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: closing the last handle terminates
//! every member process, so a child can never outlive the installer and keep
//! file handles on the install folder.

use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use crate::error::{AppError, Result};

/// An owning handle to a Win32 job object that kills its member processes when
/// the handle is closed. Create one with [`JobHandle::new_kill_on_close`], then
/// [`assign`] each child process to it.
///
/// [`assign`]: JobHandle::assign
pub struct JobHandle {
    handle: HANDLE,
}

impl JobHandle {
    /// Creates an unnamed job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    /// set, so its member processes die when this handle is closed.
    pub fn new_kill_on_close() -> Result<Self> {
        // SAFETY: a null security-attributes pointer and a null name ask for a
        // default (unnamed) job; no caller memory is dereferenced.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = JobHandle { handle };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `job.handle` is a live job handle owned by `job`; `limits` is
        // an initialized struct of exactly the size passed for the
        // `JobObjectExtendedLimitInformation` class.
        let ok = unsafe {
            SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error("SetInformationJobObject"));
        }
        Ok(job)
    }

    /// Assigns a process to this job.
    ///
    /// `process` exposes a borrowed process handle (e.g. a
    /// `std::process::Child`); the caller keeps ownership.
    pub fn assign(&self, process: impl AsRawHandle) -> Result<()> {
        let process = process.as_raw_handle();
        // SAFETY: `self.handle` is a live job handle owned by `self`; `process`
        // is a caller-supplied process handle that this call only reads.
        let ok = unsafe { AssignProcessToJobObject(self.handle, process) };
        if ok == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    /// Blocks until every process in this job has exited.
    ///
    /// Associates an I/O completion port with the job and waits for
    /// [`JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`], which fires when the last member
    /// process exits. Unlike `child.wait()`, this also waits for a game whose
    /// launcher process exits quickly while the real game continues as a
    /// grandchild. Other notifications are discarded.
    pub fn wait_until_all_exit(&self) -> Result<()> {
        use windows_sys::Win32::System::JobObjects::{
            JobObjectAssociateCompletionPortInformation, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
        };
        use windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO;
        use windows_sys::Win32::System::Threading::INFINITE;
        use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};

        // Create a fresh I/O completion port (INVALID_HANDLE_VALUE requests a
        // new port; concurrency 0 means "use # of CPUs").
        // SAFETY: the file argument is INVALID_HANDLE_VALUE, so no real handle is
        // touched; the existing-port argument is null (new port requested).
        let port = unsafe {
            CreateIoCompletionPort(
                windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
                std::ptr::null_mut(),
                0,
                0,
            )
        };
        if port.is_null() {
            return Err(last_error("CreateIoCompletionPort"));
        }
        // PortGuard closes the port on drop, including on early error returns.
        struct PortGuard(HANDLE);
        impl Drop for PortGuard {
            fn drop(&mut self) {
                // SAFETY: the handle was returned by CreateIoCompletionPort and is
                // not used after this call.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let _port_guard = PortGuard(port);

        // Associate the job with the completion port. `CompletionKey` is an
        // arbitrary caller-supplied usize; 1 is used since we own only this job.
        let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: 1 as _,
            CompletionPort: port,
        };
        // SAFETY: `self.handle` is a live job handle; `assoc` is fully initialised.
        let ok = unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectAssociateCompletionPortInformation,
                std::ptr::addr_of!(assoc).cast(),
                std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error("SetInformationJobObject (completion port)"));
        }

        // Drain the port, discarding every notification except the one that
        // signals that no processes remain in the job.
        loop {
            let mut msg_type: u32 = 0;
            let mut key: usize = 0;
            let mut overlapped = std::ptr::null_mut();
            // SAFETY: all output pointers are valid locals; INFINITE means no
            // timeout; the port handle is live for this block.
            let ok = unsafe {
                GetQueuedCompletionStatus(port, &mut msg_type, &mut key, &mut overlapped, INFINITE)
            };
            if ok == 0 {
                return Err(last_error("GetQueuedCompletionStatus"));
            }
            if msg_type == JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO {
                return Ok(());
            }
            // Any other notification: keep waiting.
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: `self.handle` came from CreateJobObjectW and is owned solely by
        // this wrapper; it is not used after this call. Closing the last job
        // handle triggers JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE for the members.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Builds an [`AppError::Win32`] carrying the calling thread's last-error code.
fn last_error(context: &str) -> AppError {
    // SAFETY: GetLastError takes no arguments and reads thread-local state only.
    let code = unsafe { GetLastError() };
    AppError::Win32 {
        context: context.to_string(),
        code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::BorrowedHandle;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[test]
    fn creates_a_job_and_assigns_the_current_process() {
        let job = JobHandle::new_kill_on_close().expect("create job object");

        // SAFETY: GetCurrentProcess returns a pseudo-handle that is always valid
        // for the calling process; `BorrowedHandle` does not close it on drop.
        let current = unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) };
        job.assign(current).expect("assign current process to job");

        // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` terminates every member process
        // when the job's last handle closes. The member here is this very test
        // process, so dropping `job` would kill the test runner. Leak the handle
        // instead; the OS reclaims it when the process exits.
        std::mem::forget(job);
    }
}
