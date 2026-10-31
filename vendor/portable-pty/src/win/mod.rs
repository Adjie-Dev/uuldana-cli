use crate::{Child, ChildKiller, ExitStatus};
use anyhow::Context as _;
use std::io::{Error as IoError, Result as IoResult};
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use winapi::shared::minwindef::DWORD;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::{INFINITE, WAIT_FAILED, WAIT_OBJECT_0};

pub mod conpty;
mod consoleflags;
mod containment;
mod procthreadattr;
mod psuedocon;
pub use containment::{ProcessStatus, SuspendedChild};

use filedescriptor::OwnedHandle;

#[derive(Debug)]
pub struct WinChild {
    proc: Mutex<OwnedHandle>,
    waiter: Option<Arc<Mutex<WaitState>>>,
}

#[derive(Debug)]
struct WaitState {
    result: Option<IoResult<ExitStatus>>,
    waker: Waker,
}

impl WinChild {
    fn wait_for_exit(proc: &OwnedHandle) -> IoResult<ExitStatus> {
        // Only a signaled, still-owned process handle makes its exit code final.
        match unsafe { WaitForSingleObject(proc.as_raw_handle() as _, INFINITE) } {
            WAIT_OBJECT_0 => {}
            WAIT_FAILED => return Err(IoError::last_os_error()),
            result => {
                return Err(IoError::other(format!(
                    "unexpected process wait result: {result:#x}"
                )))
            }
        }
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            Ok(ExitStatus::with_exit_code(status))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn start_wait_worker(proc: OwnedHandle, waker: Waker) -> IoResult<Arc<Mutex<WaitState>>> {
        let state = Arc::new(Mutex::new(WaitState {
            result: None,
            waker,
        }));
        let worker_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("portable-pty-wait".into())
            .spawn(move || {
                // Move the owned duplicate into the worker; cancellation cannot close
                // a handle while the kernel is waiting on it.
                let result = Self::wait_for_exit(&proc);
                let waker = {
                    let mut state = worker_state.lock().unwrap();
                    state.result = Some(result);
                    state.waker.clone()
                };
                waker.wake();
            })?;
        Ok(state)
    }

    fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let proc = self
            .proc
            .lock()
            .unwrap()
            .try_clone()
            .map_err(IoError::other)?;
        // An exit code is data, not a liveness test: 259 is also a valid final
        // code. Observe the owned handle before reading authoritative status.
        if !ProcessStatus::from_handle(&proc)
            .map_err(IoError::other)?
            .has_exited()?
        {
            return Ok(None);
        }
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            Ok(Some(ExitStatus::with_exit_code(status)))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn do_kill(&mut self) -> IoResult<()> {
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
        let err = IoError::last_os_error();
        if res != 0 {
            Err(err)
        } else {
            Ok(())
        }
    }
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        self.do_kill().ok();
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        Box::new(WinChildKiller { proc })
    }
}

#[derive(Debug)]
pub struct WinChildKiller {
    proc: OwnedHandle,
}

impl ChildKiller for WinChildKiller {
    fn kill(&mut self) -> IoResult<()> {
        let res = unsafe { TerminateProcess(self.proc.as_raw_handle() as _, 1) };
        let err = IoError::last_os_error();
        if res != 0 {
            Err(err)
        } else {
            Ok(())
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self.proc.try_clone().unwrap();
        Box::new(WinChildKiller { proc })
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        let proc = self
            .proc
            .lock()
            .unwrap()
            .try_clone()
            .map_err(IoError::other)?;
        Self::wait_for_exit(&proc)
    }

    fn process_id(&self) -> Option<u32> {
        let res = unsafe { GetProcessId(self.proc.lock().unwrap().as_raw_handle() as _) };
        if res == 0 {
            None
        } else {
            Some(res)
        }
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        let proc = self.proc.lock().unwrap();
        Some(proc.as_raw_handle())
    }

    fn process_status(&self) -> anyhow::Result<ProcessStatus> {
        ProcessStatus::from_handle(&*self.proc.lock().unwrap())
    }
}

impl std::future::Future for WinChild {
    type Output = anyhow::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        if let Some(waiter) = &self.waiter {
            let mut state = waiter.lock().unwrap();
            if let Some(result) = state.result.take() {
                return Poll::Ready(result.context("Failed to wait for process exit"));
            }
            state.waker = cx.waker().clone();
            return Poll::Pending;
        }
        match self.is_complete() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                let proc = self.proc.lock().unwrap().try_clone()?;
                self.waiter = Some(Self::start_wait_worker(proc, cx.waker().clone())?);
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod exit_status_tests {
    use super::*;
    use std::process::{Command, Stdio};

    struct Notify(std::sync::mpsc::Sender<()>);
    impl std::task::Wake for Notify {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    fn worker_result(proc: OwnedHandle) -> IoResult<ExitStatus> {
        let (tx, rx) = std::sync::mpsc::channel();
        let state = WinChild::start_wait_worker(proc, Waker::from(Arc::new(Notify(tx)))).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let result = state.lock().unwrap().result.take().unwrap();
        result
    }

    #[test]
    fn queryable_process_without_synchronize_rejects_failed_wait() {
        use std::os::windows::io::FromRawHandle;
        use winapi::um::handleapi::DuplicateHandle;
        use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
        let mut native = fixture("running");
        let mut raw = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    AsRawHandle::as_raw_handle(&native) as _,
                    GetCurrentProcess(),
                    &mut raw,
                    PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    0,
                )
            },
            0
        );
        let query_only = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let mut code = 0;
        assert_ne!(unsafe { GetExitCodeProcess(raw, &mut code) }, 0);
        assert_eq!(code, 259, "control: query succeeds for a live process");
        let result = WinChild::wait_for_exit(&query_only);
        let worker = worker_result(query_only);
        native.kill().unwrap();
        native.wait().unwrap();
        assert_eq!(
            result.unwrap_err().raw_os_error(),
            Some(5),
            "WAIT_FAILED must preserve ERROR_ACCESS_DENIED, never return completion"
        );
        assert_eq!(worker.unwrap_err().raw_os_error(), Some(5));
    }

    #[test]
    fn native_exit_fixture() {
        if let Ok(code) = std::env::var("UULDANA_NATIVE_EXIT_FIXTURE") {
            if code == "running" {
                std::thread::sleep(std::time::Duration::from_secs(30));
            } else {
                std::process::exit(code.parse::<u32>().unwrap() as i32);
            }
        }
    }

    fn fixture(code: &str) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "win::exit_status_tests::native_exit_fixture"])
            .env("UULDANA_NATIVE_EXIT_FIXTURE", code)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn signaled_native_status_preserves_259_and_ntstatus_bits() {
        for code in [0_u32, 259, 0xc000004b, u32::MAX] {
            let mut native = fixture(&code.to_string());
            let mut backend = WinChild {
                proc: Mutex::new(OwnedHandle::dup(&native).unwrap()),
                waiter: None,
            };
            let observer = backend.process_status().unwrap();
            let expected = native.wait().unwrap().code().unwrap() as u32;
            assert_eq!(expected, code, "native status must preserve all DWORD bits");
            assert!(observer.has_exited().unwrap());
            let polled = backend.try_wait().unwrap();
            assert_eq!(
                polled.as_ref().map(ExitStatus::exit_code),
                Some(expected),
                "signaled owned process handle; native code={code:#010x}"
            );
            assert_eq!(backend.wait().unwrap().exit_code(), expected);
            assert_eq!(backend.wait().unwrap().exit_code(), expected);
            assert_eq!(
                worker_result(OwnedHandle::dup(&native).unwrap())
                    .unwrap()
                    .exit_code(),
                expected
            );
        }
    }

    #[test]
    fn future_wait_worker_survives_owner_drop_and_wakes_latest_poller() {
        use std::future::Future;
        let mut native = fixture("running");
        let mut backend = WinChild {
            proc: Mutex::new(OwnedHandle::dup(&native).unwrap()),
            waiter: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let first = Waker::from(Arc::new(Notify(tx)));
        assert!(Pin::new(&mut backend)
            .poll(&mut Context::from_waker(&first))
            .is_pending());
        let state = Arc::clone(backend.waiter.as_ref().unwrap());
        let (tx, latest_rx) = std::sync::mpsc::channel();
        let latest = Waker::from(Arc::new(Notify(tx)));
        assert!(Pin::new(&mut backend)
            .poll(&mut Context::from_waker(&latest))
            .is_pending());
        assert!(
            Arc::ptr_eq(&state, backend.waiter.as_ref().unwrap()),
            "one worker per pending future"
        );
        drop(backend);
        native.kill().unwrap();
        let expected = native.wait().unwrap().code().unwrap() as u32;
        latest_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(rx.try_recv().is_err(), "worker must wake the latest poller");
        assert_eq!(
            state
                .lock()
                .unwrap()
                .result
                .take()
                .unwrap()
                .unwrap()
                .exit_code(),
            expected
        );
    }

    #[test]
    fn unsignaled_native_process_is_not_complete() {
        let mut native = fixture("running");
        let mut backend = WinChild {
            proc: Mutex::new(OwnedHandle::dup(&native).unwrap()),
            waiter: None,
        };
        let observer = backend.process_status().unwrap();
        let signaled = observer.has_exited().unwrap();
        let polled = backend.try_wait().unwrap();
        // Always release the real process before asserting, including on failure.
        native.kill().unwrap();
        native.wait().unwrap();
        assert!(!signaled);
        assert!(polled.is_none());
    }
}
