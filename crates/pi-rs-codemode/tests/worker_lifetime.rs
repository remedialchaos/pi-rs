use pi_rs_codemode::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
#[test]
fn denied_admission_does_not_start_worker() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let starts = Arc::new(AtomicUsize::new(0));
        let count = starts.clone();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            acquire_worker: Some(Arc::new(|| Err(HostError("capacity exhausted".into())))),
            on_worker_started: Some(Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
            ..Default::default()
        })?;
        let result = sandbox
            .execute("return 1", ExecuteOptions::default())
            .await?;
        assert_eq!(
            result.error().map(|e| (e.kind, e.message.as_str())),
            Some((ErrorKind::Sandbox, "capacity exhausted"))
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        Ok(())
    })
}
#[test]
fn successful_worker_releases_admission_on_actual_exit() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let releases = Arc::new(AtomicUsize::new(0));
        let count = releases.clone();
        let exit = Arc::new(Mutex::new(None));
        let save = exit.clone();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            acquire_worker: Some(Arc::new(move || {
                let count = count.clone();
                Ok(Box::new(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                }))
            })),
            on_worker_started: Some(Arc::new(move |worker| {
                *save.lock().map_err(|e| HostError(e.to_string()))? = Some(worker);
                Ok(())
            })),
            ..Default::default()
        })?;
        assert!(
            sandbox
                .execute("return 1", ExecuteOptions::default())
                .await?
                .is_ok()
        );
        let exit = exit
            .lock()
            .map_err(|e| HostError(e.to_string()))?
            .clone()
            .ok_or_else(|| HostError("worker hook not called".into()))?;
        exit.wait().await;
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        Ok(())
    })
}
#[test]
fn failing_start_hook_terminates_worker_and_releases_admission() -> Result<(), HostError> {
    futures::executor::block_on(async {
        let releases = Arc::new(AtomicUsize::new(0));
        let count = releases.clone();
        let exit = Arc::new(Mutex::new(None));
        let save = exit.clone();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            acquire_worker: Some(Arc::new(move || {
                let count = count.clone();
                Ok(Box::new(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                }))
            })),
            on_worker_started: Some(Arc::new(move |worker| {
                *save.lock().map_err(|e| HostError(e.to_string()))? = Some(worker);
                Err(HostError("hook failed".into()))
            })),
            ..Default::default()
        })?;
        let result = sandbox
            .execute("return 1", ExecuteOptions::default())
            .await?;
        assert_eq!(
            result.error().map(|e| (e.kind, e.message.as_str())),
            Some((ErrorKind::Sandbox, "hook failed"))
        );
        let exit = exit
            .lock()
            .map_err(|e| HostError(e.to_string()))?
            .clone()
            .ok_or_else(|| HostError("worker hook not called".into()))?;
        exit.wait().await;
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        Ok(())
    })
}
