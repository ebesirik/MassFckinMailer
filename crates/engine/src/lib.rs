//! The gpui↔tokio bridge.
//!
//! gpui runs its own executors while the mail stack (lettre, reqwest, aws-sdk)
//! needs tokio, so the app owns exactly one dedicated OS thread running a tokio
//! runtime ("mail runtime"). Communication is via flume channels, which offer
//! both sync and async APIs on both ends — the UI side sends commands
//! synchronously (non-blocking) and awaits events with `recv_async` on gpui's
//! foreground executor; flume's async support is runtime-agnostic, so no tokio
//! is needed on the gpui side.
//!
//! M0 ships a dummy job that simulates a campaign send to prove the whole
//! path: command in → progress events out → cancel.

use mmm_providers::Account;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum Command {
    /// Simulate sending `total` emails (M0 bridge proof; replaced by the real
    /// campaign job in M4).
    StartDummyJob { total: u32 },
    CancelJob,
    /// Run a provider connectivity/credentials check ("Test connection").
    /// `secret` is the keychain value (SMTP password / API key) supplied by the
    /// caller so the engine needn't touch the keychain.
    TestAccount { account: Account, secret: String },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Event {
    JobProgress {
        done: u32,
        total: u32,
    },
    JobFinished {
        done: u32,
        total: u32,
        cancelled: bool,
    },
    /// Result of a [`Command::TestAccount`], correlated by `account_id`.
    TestResult {
        account_id: String,
        ok: bool,
        message: String,
    },
}

pub struct MailRuntime {
    cmd_tx: flume::Sender<Command>,
    evt_rx: flume::Receiver<Event>,
    thread: Option<JoinHandle<()>>,
}

impl MailRuntime {
    /// Spawn the mail-runtime thread. Call once at app start.
    pub fn start() -> Self {
        let (cmd_tx, cmd_rx) = flume::bounded::<Command>(64);
        let (evt_tx, evt_rx) = flume::bounded::<Event>(512);

        let thread = std::thread::Builder::new()
            .name("mail-runtime".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("failed to build mail runtime");
                runtime.block_on(run_loop(cmd_rx, evt_tx));
            })
            .expect("failed to spawn mail-runtime thread");

        Self {
            cmd_tx,
            evt_rx,
            thread: Some(thread),
        }
    }

    /// Non-blocking; drops the command if the runtime is gone (app shutdown).
    pub fn command(&self, command: Command) {
        let _ = self.cmd_tx.send(command);
    }

    /// Clone of the event stream. Await with `recv_async` from any executor.
    pub fn events(&self) -> flume::Receiver<Event> {
        self.evt_rx.clone()
    }
}

impl Drop for MailRuntime {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run_loop(cmd_rx: flume::Receiver<Command>, evt_tx: flume::Sender<Event>) {
    let mut current_job: Option<CancellationToken> = None;

    while let Ok(command) = cmd_rx.recv_async().await {
        match command {
            Command::StartDummyJob { total } => {
                // Cancel any previous job before starting a new one.
                if let Some(token) = current_job.take() {
                    token.cancel();
                }
                let token = CancellationToken::new();
                current_job = Some(token.clone());
                tokio::spawn(dummy_job(total, token, evt_tx.clone()));
            }
            Command::CancelJob => {
                if let Some(token) = current_job.take() {
                    token.cancel();
                }
            }
            Command::TestAccount { account, secret } => {
                // Independent of any running campaign; runs concurrently.
                tokio::spawn(test_account(account, secret, evt_tx.clone()));
            }
            Command::Shutdown => {
                if let Some(token) = current_job.take() {
                    token.cancel();
                }
                break;
            }
        }
    }
}

/// Build the account's provider and run its `verify()`, reporting the outcome
/// as a [`Event::TestResult`].
async fn test_account(account: Account, secret: String, events: flume::Sender<Event>) {
    let account_id = account.id.clone();
    let (ok, message) = match mmm_providers::build_provider(&account, secret) {
        Ok(provider) => match provider.verify().await {
            Ok(()) => (true, "Connection succeeded.".to_string()),
            Err(e) => (false, e.to_string()),
        },
        Err(e) => (false, e.to_string()),
    };
    let _ = events
        .send_async(Event::TestResult {
            account_id,
            ok,
            message,
        })
        .await;
}

/// Simulates a campaign: ~25 msg/s with a progress event per message.
/// The real send job (M4) will coalesce progress events (~20 Hz) so huge
/// campaigns don't flood the UI thread; at this scale per-message is fine.
async fn dummy_job(total: u32, cancel: CancellationToken, events: flume::Sender<Event>) {
    for done in 1..=total {
        if cancel.is_cancelled() {
            let _ = events
                .send_async(Event::JobFinished {
                    done: done - 1,
                    total,
                    cancelled: true,
                })
                .await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ = events.send_async(Event::JobProgress { done, total }).await;
    }
    let _ = events
        .send_async(Event::JobFinished {
            done: total,
            total,
            cancelled: false,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dummy_job_completes() {
        let runtime = MailRuntime::start();
        let events = runtime.events();
        runtime.command(Command::StartDummyJob { total: 5 });

        let mut last_done = 0;
        loop {
            let event = events
                .recv_timeout(Duration::from_secs(5))
                .expect("event stream stalled");
            match event {
                Event::JobProgress { done, .. } => last_done = done,
                Event::JobFinished {
                    done,
                    total,
                    cancelled,
                } => {
                    assert!(!cancelled);
                    assert_eq!(done, 5);
                    assert_eq!(total, 5);
                    break;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(last_done, 5);
    }

    #[test]
    fn dummy_job_cancels() {
        let runtime = MailRuntime::start();
        let events = runtime.events();
        runtime.command(Command::StartDummyJob { total: 1000 });

        // Wait for a bit of progress, then cancel.
        loop {
            match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::JobProgress { done, .. } if done >= 3 => break,
                _ => {}
            }
        }
        runtime.command(Command::CancelJob);

        loop {
            match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::JobFinished {
                    done, cancelled, ..
                } => {
                    assert!(cancelled);
                    assert!(done < 1000);
                    break;
                }
                _ => {}
            }
        }
    }
}
