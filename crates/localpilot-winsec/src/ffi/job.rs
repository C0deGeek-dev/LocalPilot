//! Kernel-owned process trees. All handles are non-inheritable and RAII-owned.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

/// Owns a command's Windows Job Object. Dropping it terminates every member,
/// including descendants whose original parent has already exited. It is not
/// cloneable, inheritable, or named, so closing it closes the last job handle.
pub struct ProcessJob(OwnedHandle);

impl ProcessJob {
    fn new() -> io::Result<Self> {
        // SAFETY: null attributes/name request an unnamed, non-inheritable job.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        let job = Self(own(raw)?);
        // SAFETY: this Win32 POD uses zero as the default for all unused limits.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the owned job and correctly sized/initialized limits live
        // through the call. Windows copies the information, retaining no pointer.
        let ok = unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        let process = child.raw_handle().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "suspended child has no process handle",
            )
        })?;
        // SAFETY: both borrowed handles remain owned and live through the call.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle() as HANDLE, process as HANDLE) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Spawn suspended, assign to a kill-on-close job, then resume the initial
/// thread. The child cannot spawn descendants before containment is established.
/// Preserve the caller's flags (notably console isolation); this operation
/// always resumes the child and enables immediate-child kill-on-drop as well.
///
/// # Errors
/// Job setup, spawn, assignment, thread lookup, or resume fails. A child spawned
/// before a setup failure is killed while still suspended; there is no fallback
/// to an uncontained running process.
pub fn spawn_in_job(command: &mut Command, creation_flags: u32) -> io::Result<(Child, ProcessJob)> {
    spawn_with_setup(command, creation_flags, |job, child| {
        job.assign(child)?;
        let pid = child.id().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "suspended child has no process id")
        })?;
        resume_initial_thread(pid)
    })
}

fn spawn_with_setup(
    command: &mut Command,
    creation_flags: u32,
    setup: impl FnOnce(&ProcessJob, &Child) -> io::Result<()>,
) -> io::Result<(Child, ProcessJob)> {
    let job = ProcessJob::new()?;
    command
        .creation_flags(creation_flags | CREATE_SUSPENDED)
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    if let Err(error) = setup(&job, &child) {
        // No user code has run if assignment fails. If resume fails, job close
        // kills any assigned members. The immediate child is also killed on drop.
        let _ = child.start_kill();
        return Err(error);
    }
    Ok((child, job))
}

fn own(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: callers pass a newly created real handle, transferring its sole
    // ownership here. OwnedHandle closes it exactly once and is Send + Sync.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw.cast()) })
}

fn resume_initial_thread(pid: u32) -> io::Result<()> {
    // Tokio/stdlib close CreateProcess's primary-thread handle. Its suspended
    // process still owns the thread, discoverable through the documented snapshot
    // API. The live Child handle prevents process-id reuse during this lookup.
    // SAFETY: flags request a thread snapshot; no pointer arguments are involved.
    let snapshot = own(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    // SAFETY: Win32 POD; dwSize declares the initialized buffer's layout.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    // SAFETY: snapshot is live and entry is a writable, correctly sized buffer.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: the suspended child's initial thread cannot exit or have
            // its id reused before resume. The returned real handle is owned.
            let thread = own(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })?;
            // SAFETY: thread has resume rights and stays live through the call.
            let count = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
            return match count {
                1 => Ok(()),
                u32::MAX => Err(io::Error::last_os_error()),
                _ => Err(io::Error::other("unexpected initial thread suspend count")),
            };
        }
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        // SAFETY: as Thread32First; the snapshot cursor is managed by Windows.
        found = unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) };
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended child's initial thread was not found",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_SYNCHRONIZE,
    };

    fn command(script: &str, cwd: &std::path::Path) -> Command {
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    async fn marker(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn closing_one_job_kills_an_exited_parents_descendant_and_leaves_other_jobs_alive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("descendant.ps1"),
            "Set-Content started $PID; while (!(Test-Path released)) { Start-Sleep -Milliseconds 20 }; Set-Content leaked bad",
        )
        .unwrap();
        let script = "Start-Process powershell.exe -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -File ./descendant.ps1' -RedirectStandardOutput out.txt -RedirectStandardError err.txt";
        let (mut child, job) =
            spawn_in_job(&mut command(script, dir.path()), CREATE_NO_WINDOW).unwrap();
        let other = tempfile::tempdir().unwrap();
        let (mut other_child, other_job) = spawn_in_job(
            &mut command(
                "Set-Content started ready; Start-Sleep -Seconds 60",
                other.path(),
            ),
            CREATE_NO_WINDOW,
        )
        .unwrap();
        marker(&dir.path().join("started")).await;
        marker(&other.path().join("started")).await;
        let descendant_pid: u32 = std::fs::read_to_string(dir.path().join("started"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: OpenProcess returns a new owned handle with only wait rights.
        let descendant =
            own(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_pid) }).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        drop(job);
        assert!(other_child.try_wait().unwrap().is_none());
        // SAFETY: live owned process handle; the bounded wait does not dereference memory.
        assert_eq!(
            unsafe { WaitForSingleObject(descendant.as_raw_handle() as HANDLE, 5000) },
            0
        );
        std::fs::write(dir.path().join("released"), "go").unwrap();
        assert!(!dir.path().join("leaked").exists());
        drop(other_job);
        tokio::time::timeout(Duration::from_secs(10), other_child.wait())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn containment_setup_failure_never_runs_user_code() {
        for after_assignment in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut cmd = command(
                "Set-Content escaped bad; Start-Sleep -Seconds 60",
                dir.path(),
            );
            let mut child_pid = None;
            let mut process_handle = None;
            let result = spawn_with_setup(&mut cmd, CREATE_NO_WINDOW, |job, child| {
                child_pid = child.id();
                // SAFETY: the Child owns this valid handle throughout the
                // borrow; try_clone_to_owned duplicates it before Child drops.
                let borrowed = unsafe {
                    std::os::windows::io::BorrowedHandle::borrow_raw(child.raw_handle().unwrap())
                };
                process_handle = Some(borrowed.try_clone_to_owned()?);
                if after_assignment {
                    job.assign(child)?;
                }
                Err(io::Error::other("injected containment failure"))
            });
            assert!(result.is_err());
            assert!(child_pid.is_some());
            // SAFETY: a live owned duplicate of the failed child's handle.
            assert_eq!(
                unsafe {
                    WaitForSingleObject(process_handle.unwrap().as_raw_handle() as HANDLE, 5000)
                },
                0
            );
            assert!(!dir.path().join("escaped").exists());
        }
    }
}
