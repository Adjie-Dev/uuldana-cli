//! Isolated PTY feasibility probe. Runs only the harmless local fixture.
//! Build uuldana-test-child first. Use --scripted or --interactive.
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    error::Error,
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuldana_core::*;
use uuldana_transport::{Frame, Process};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "--scripted".into());
    if mode != "--scripted" && mode != "--interactive" {
        return Err("use --scripted or --interactive".into());
    }
    let fixture = std::env::current_exe()?
        .parent()
        .ok_or("missing example directory")?
        .join(format!(
            "uuldana-test-child{}",
            std::env::consts::EXE_SUFFIX
        ));
    if !fixture.is_file() {
        return Err("first run: cargo build -p uuldana-transport --examples".into());
    }
    let policy = PermissionPolicy::default();
    let permissions = validate_mapping(&policy, &PermissionMapping::new(policy))?;
    let request = LaunchRequest::new(
        std::env::current_dir()?,
        TransportKind::NativePty,
        permissions,
    );
    let plan = LaunchPlan::new(
        fixture,
        request.workspace.clone(),
        TransportSpec::NativePty {
            initial_size: TerminalSize::DEFAULT,
        },
    )
    .arg("interactive");
    let mut process = Process::spawn(&plan, &request).await?;
    if mode == "--scripted" {
        tokio::time::timeout(Duration::from_secs(15), scripted(&mut process)).await??;
    } else {
        interactive(&mut process).await?;
    }
    Ok(())
}

async fn scripted(process: &mut Process) -> Result<(), Box<dyn Error>> {
    let mut bytes = Vec::new();
    let mut cursor = false;
    let mut sent = false;
    let mut exited = false;
    while let Some(frame) = process.next_frame().await? {
        match frame {
            Frame::Output(b) => bytes.extend(b),
            Frame::Exited(exit) => {
                if !exit.success {
                    return Err(format!("fixture failed: {exit:?}").into());
                }
                exited = true;
            }
            _ => return Err("PTY unexpectedly separated streams".into()),
        }
        if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
            process.write_input(b"\x1b[1;1R").await?;
            cursor = true;
        }
        if !sent && String::from_utf8_lossy(&bytes).contains("READY") {
            process.resize(TerminalSize::new(37, 111))?;
            process.write_input(b"size\rhello\rquit\r").await?;
            sent = true;
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    for expected in ["SIZE=37x111", "ECHO=hello", "error-stream", "BYE"] {
        if !text.contains(expected) {
            return Err(format!("missing {expected:?}: {text:?}").into());
        }
    }
    if !exited || !bytes.contains(&0x1b) {
        return Err("missing exit or ANSI output".into());
    }
    println!(
        "PASS: native PTY spawn; input roundtrip; ANSI output; merged stderr; child-observed resize 37x111; drained exit 0"
    );
    Ok(())
}

struct RawTerminal(Arc<AtomicBool>);
impl Drop for RawTerminal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
async fn interactive(process: &mut Process) -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "interactive mode needs a real host terminal; use --scripted for automation".into(),
        );
    }
    println!("Uuldana PTY fixture: type hello, size, or quit; Ctrl+C interrupts the child.");
    let position = crossterm::cursor::position()?;
    let (cols, rows) = crossterm::terminal::size()?;
    process.resize(TerminalSize::new(rows, cols))?;
    crossterm::terminal::enable_raw_mode()?;
    let stopped = Arc::new(AtomicBool::new(false));
    let _guard = RawTerminal(stopped.clone());
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let input = process.input_handle().ok_or("missing input")?;
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        while !stopped.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    let result = match event::read() {
                        Ok(Event::Key(key)) => encode_key(key),
                        Ok(Event::Paste(text)) => {
                            Some(format!("\x1b[200~{text}\x1b[201~").into_bytes())
                        }
                        other => {
                            if tx.blocking_send(other).is_err() {
                                break;
                            }
                            None
                        }
                    };
                    if let Some(bytes) = result
                        && let Err(error) = runtime.block_on(input.write(&bytes))
                    {
                        let _ = tx.blocking_send(Err(io::Error::other(error)));
                        break;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    let mut tail = Vec::<u8>::new();
    let mut cursor_replied = false;
    loop {
        tokio::select! {
            frame = process.next_frame() => match frame? {
                Some(Frame::Output(bytes)) => {
                    io::stdout().write_all(&bytes)?; io::stdout().flush()?;
                    tail.extend_from_slice(&bytes);
                    if !cursor_replied && tail.windows(4).any(|b| b == b"\x1b[6n") {
                        process.write_input(format!("\x1b[{};{}R", position.1 + 1, position.0 + 1).as_bytes()).await?;
                        cursor_replied = true;
                    }
                    if tail.len() > 4 { tail.drain(..tail.len()-4); }
                }
                Some(Frame::Exited(_)) | None => break,
                _ => {},
            },
            input = rx.recv() => if let Event::Resize(cols, rows) = input.ok_or("input worker stopped")?? {
                process.resize(TerminalSize::new(rows, cols))?;
            },
        }
    }
    Ok(())
}

// Basic ANSI keyboard mode only; extended native keyboard protocols need
// negotiated handling before claiming complete native UI fidelity.
fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let mut bytes = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii() => {
            vec![(c.to_ascii_uppercase() as u8) & 0x1f]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Esc => vec![27],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 27);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basic_keyboard_preserves_unicode_interrupt_and_arrows() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('λ'), KeyModifiers::NONE)),
            Some("λ".as_bytes().to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            )),
            None
        );
    }
}
