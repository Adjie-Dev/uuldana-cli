//! Terminal relay against a real child in a real pseudo terminal.
mod common;
mod support;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{DecoderMode, FixtureAdapter, runtime};
use tokio::sync::mpsc;
use uuldana_core::*;
use uuldana_runtime::{
    StartOptions,
    relay::{CursorPosition, RelayEnd, relay},
};

/// Output sink that lets the test watch what the "terminal" received.
#[derive(Clone, Default)]
struct Screen(Arc<Mutex<Vec<u8>>>);
impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Screen {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[tokio::test]
async fn relays_pty_output_input_and_resize_until_the_provider_exits() {
    let mut adapter = FixtureAdapter::new("interrupted", DecoderMode::Normal);
    adapter.descriptor.transports = vec![TransportKind::NativePty];
    let mut runtime = runtime(Arc::new(adapter));
    let mut options = StartOptions::new(
        ProviderId::new("test.fixture").unwrap(),
        RunId::new("r").unwrap(),
    );
    options.transport = Some(TransportKind::NativePty);
    runtime.start(options).await.unwrap();

    let screen = Screen::default();
    let (keys, input) = mpsc::unbounded_channel();
    // The "user" types `quit` once the provider says it is waiting. On Windows
    // the relay itself answers ConPTY's cursor query.
    let watcher = {
        let screen = screen.clone();
        tokio::spawn(async move {
            for _ in 0..300 {
                if screen.text().contains("WAITING") {
                    keys.send(b"quit\r".to_vec()).unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    };
    let mut position = || (1u16, 1u16);
    let cursor: Option<CursorPosition<'_>> = cfg!(windows).then_some(&mut position);
    let mut polls = 0u32;
    let exit = tokio::time::timeout(
        Duration::from_secs(20),
        relay(
            &mut runtime,
            input,
            screen.clone(),
            || {
                polls += 1;
                // The user's terminal grows after the first poll.
                Some(if polls == 1 {
                    TerminalSize::new(24, 80)
                } else {
                    TerminalSize::new(40, 120)
                })
            },
            cursor,
            |_, _| false,
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("relay timed out\n{}", screen.text()))
    .0
    .unwrap();
    watcher.abort();

    let text = screen.text();
    assert!(text.contains("WAITING"), "{text}");
    assert!(text.contains("CANCELLED"), "{text}");
    assert!(
        !text.contains("\x1b[6n") || !cfg!(windows),
        "query was forwarded: {text}"
    );
    let RelayEnd::ProviderExited(exit) = exit else {
        panic!("relay was stopped: {text}")
    };
    assert_eq!(exit.map(|e| e.success), Some(true), "{text}");
    assert!(polls >= 2, "terminal size was never re-read");
}
