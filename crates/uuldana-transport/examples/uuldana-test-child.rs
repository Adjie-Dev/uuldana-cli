//! Harmless local process fixture for PTY feasibility tests.
use std::io::{self, BufRead, Read, Write};

fn main() -> io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("containment-early") => {
            // First observable user-code action creates a descendant, before
            // any terminal read/write or readiness handshake can delay it.
            let path = std::path::PathBuf::from(std::env::args_os().nth(2).expect("PID path"));
            let heartbeat = path.with_extension("heartbeat");
            let child = std::process::Command::new(std::env::current_exe()?)
                .arg("heartbeat")
                .arg(&heartbeat)
                .spawn()?;
            std::fs::write(&path, format!("{} {}", std::process::id(), child.id()))?;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        Some("records") => {
            print!("first\r\nsecond\ntail");
            eprint!("error-stream");
        }
        Some("burst-done" | "burst-limit" | "burst-large") => {
            for index in 0..200 {
                if std::env::args().nth(1).as_deref() == Some("burst-large") {
                    println!("record-{index:03}-{}", "x".repeat(8192));
                } else {
                    println!("record-{index:03}");
                }
            }
            println!(
                "{}",
                if std::env::args().nth(1).as_deref() == Some("burst-limit") {
                    "LIMIT quota"
                } else {
                    "DONE"
                }
            );
        }
        Some("arguments") => {
            for arg in std::env::args().skip(2) {
                println!("{arg}");
            }
        }
        Some("environment") => {
            println!(
                "SET={}",
                std::env::var("UULDANA_FIXTURE_SET").unwrap_or_default()
            );
            println!(
                "REMOVE={}",
                std::env::var("UULDANA_FIXTURE_REMOVE").unwrap_or_else(|_| "absent".into())
            );
            println!("CWD={}", std::env::current_dir()?.display());
        }
        Some("eof") => {
            let mut s = String::new();
            io::stdin().read_line(&mut s)?;
            println!("EOF={}", s.is_empty());
        }
        Some("flood") => {
            let err = std::thread::spawn(|| {
                for _ in 0..256 {
                    eprint!("{}", "e".repeat(8192));
                }
            });
            for _ in 0..256 {
                println!("{}", "x".repeat(8191));
            }
            err.join().unwrap();
        }
        Some("invalid-utf8") => {
            io::stdout().write_all(b"\xff\nvalid\n")?;
        }
        Some("huge-record") => {
            io::stdout().write_all(&vec![b'x'; 1024 * 1024 + 1])?;
            io::stdout().write_all(b"\nvalid\n")?;
        }
        Some("multiple-errors") => {
            io::stdout().write_all(b"\xff\nfirst\n\xfe\n")?;
            io::stdout().write_all(&vec![b'x'; 1024 * 1024 + 1])?;
            io::stdout().write_all(b"\nlast\n")?;
        }
        Some("descendant-exit") | Some("descendant-wait") => {
            let child = std::process::Command::new(std::env::current_exe()?)
                .arg("heartbeat")
                .arg(std::env::args().nth(2).expect("heartbeat path"))
                .spawn()?;
            let heartbeat_path = std::env::args().nth(2).expect("heartbeat path");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !std::path::Path::new(&heartbeat_path).is_file() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "descendant did not start"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            println!("DESCENDANT={}", child.id());
            io::stdout().flush()?;
            if std::env::args().nth(1).as_deref() == Some("descendant-wait") {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
        Some("heartbeat") => {
            let path = std::env::args().nth(2).expect("heartbeat path");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                std::fs::write(
                    &path,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                        .to_string(),
                )?;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        Some("exit259") => std::process::exit(259),
        Some("duplex") => {
            io::stdout().write_all(&vec![b'x'; 2 * 1024 * 1024])?;
            io::stdout().flush()?;
            let mut bytes = vec![0; 2 * 1024 * 1024];
            io::stdin().read_exact(&mut bytes)?;
            assert!(bytes.iter().all(|b| *b == b'y'));
            println!("READ={}", bytes.len());
        }
        Some("interruptible") => {
            println!("READY");
            io::stdout().flush()?;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        Some("wait") => loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        Some("interactive") => {
            println!("\x1b[32mREADY\x1b[0m");
            eprintln!("error-stream");
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                match line?.trim() {
                    "quit" => {
                        println!("BYE");
                        break;
                    }
                    "size" => {
                        let (cols, rows) = crossterm::terminal::size()?;
                        println!("SIZE={rows}x{cols}");
                    }
                    line => println!("ECHO={line}"),
                }
                io::stdout().flush()?;
            }
        }
        _ => std::process::exit(2),
    }
    Ok(())
}
