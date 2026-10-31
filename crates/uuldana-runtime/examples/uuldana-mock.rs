//! Harmless deterministic local provider fixture; never calls an AI service.
use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "--version" {
        println!("uuldana-mock 0.1");
        return Ok(());
    }
    if mode == "heartbeat" || mode == "heartbeat-held" {
        let path = std::env::args().nth(2).expect("heartbeat path");
        let lifetime = if mode == "heartbeat-held" { 60 } else { 30 };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(lifetime);
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
        return Ok(());
    }
    if mode == "probe-overflow-stdout" || mode == "probe-overflow-stderr" {
        let bytes = vec![b'x'; 1024 * 1024 + 1];
        if mode == "probe-overflow-stdout" {
            io::stdout().write_all(&bytes)?;
            io::stdout().flush()?;
        } else {
            io::stderr().write_all(&bytes)?;
            io::stderr().flush()?;
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    println!("READY native-mock-session");
    println!("TURN");
    println!("TEXT deterministic progress");
    io::stdout().flush()?;
    let exit = match mode.as_str() {
        "burst-complete" | "burst-quota" => {
            for index in 0..200 {
                println!("TEXT record-{index:03}");
            }
            println!(
                "{}",
                if mode == "burst-complete" {
                    "DONE"
                } else {
                    "LIMIT quota"
                }
            );
            0
        }
        "tree-exit" | "tree-exit-synchronized" | "tree-graceful" | "tree-force" => {
            let path = std::env::args().nth(2).expect("heartbeat path");
            let child = std::process::Command::new(std::env::current_exe()?)
                .args([
                    if mode == "tree-exit-synchronized" {
                        "heartbeat-held"
                    } else {
                        "heartbeat"
                    },
                    &path,
                ])
                .spawn()?;
            std::fs::write(
                std::path::Path::new(&path).with_extension("pid"),
                child.id().to_string(),
            )?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !std::path::Path::new(&path).is_file() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "descendant did not start"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            println!("DESCENDANT={}", child.id());
            println!("WAITING");
            io::stdout().flush()?;
            if mode == "tree-exit-synchronized" {
                let path = std::path::Path::new(&path);
                // Publishing readiness follows descendant startup and flushed
                // output. The test releases us independently of output EOF.
                std::fs::write(
                    path.with_extension("ready-tmp"),
                    std::process::id().to_string(),
                )?;
                std::fs::rename(
                    path.with_extension("ready-tmp"),
                    path.with_extension("ready"),
                )?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                while !path.with_extension("release").is_file() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "exit release not received"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if mode == "tree-force" {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            if mode == "tree-graceful" {
                for line in io::stdin().lock().lines() {
                    if line? == "quit" {
                        break;
                    }
                }
                println!("CANCELLED");
            }
            0
        }
        "startup" | "complete" | "no-resume" | "no-policy" => {
            println!("DONE");
            0
        }
        "quota" => {
            println!("LIMIT quota");
            1
        }
        "rate" => {
            println!("LIMIT rate");
            1
        }
        "context" => {
            println!("LIMIT context");
            1
        }
        "auth" => {
            println!("ERROR auth");
            1
        }
        "network" => {
            println!("ERROR network");
            1
        }
        "unknown" => {
            println!("ERROR unknown");
            1
        }
        "crash" => 23,
        "bare-exit" => 0,
        "recover-protocol" => {
            println!("not-a-valid-record");
            println!("DONE");
            0
        }
        "recover-rate" => {
            println!("LIMIT rate");
            println!("TURN");
            println!("DONE");
            0
        }
        "recover-network" => {
            println!("ERROR network");
            println!("DONE");
            0
        }
        "recover-inferred-quota" => {
            println!("INFERRED_QUOTA");
            println!("DONE");
            0
        }
        "invalid-then-valid" => {
            io::stdout().write_all(&[0xff, b'\n'])?;
            println!("DONE");
            0
        }
        "huge-then-valid" => {
            io::stdout().write_all(&vec![b'x'; 1024 * 1024 + 1])?;
            println!("\nDONE");
            0
        }
        "multiple-malformed" => {
            io::stdout().write_all(&[0xff, b'\n'])?;
            io::stdout().write_all(&vec![b'x'; 1024 * 1024 + 1])?;
            io::stdout().write_all(&[b'\n', 0xff, b'\n'])?;
            println!("DONE");
            0
        }
        "incomplete-second-turn" => {
            println!("DONE");
            println!("TURN");
            0
        }
        // Terminal-UI stand-ins used by the orchestrator tests. They print what a
        // provider's screen would, but they are plain local processes.
        "pty-limit" => {
            println!("working");
            println!("5-hour limit reached - resets 3pm");
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                if line? == "quit" {
                    break;
                }
            }
            0
        }
        // What an agent Uuldana has no special knowledge of prints when its
        // allowance is used up, and what it prints for a pause that passes.
        "pty-limit-generic" | "pty-rate-pause" => {
            println!("working");
            if mode == "pty-limit-generic" {
                println!("Daily limit reached. Try again tomorrow or upgrade your plan.");
            } else {
                println!("Rate limit exceeded. Retrying in 20 seconds. Upgrade for higher limits.");
            }
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                if line? == "quit" {
                    break;
                }
            }
            0
        }
        // What Claude Code prints when an allowance is gone, in its colors, and
        // the warning it prints before: only the first group is a stop.
        "pty-limit-weekly" | "pty-limit-session" | "pty-limit-opus" | "pty-warning-85" => {
            println!("working");
            match mode.as_str() {
                "pty-limit-weekly" => {
                    println!(
                        "\x1b[1;31mYou've hit your weekly limit\x1b[0m \u{b7} resets Mon 12:00am"
                    )
                }
                "pty-limit-session" => {
                    println!("\x1b[1;31mYou've hit your session limit\x1b[0m \u{b7} resets 3:45pm")
                }
                "pty-limit-opus" => {
                    println!("\x1b[1;31mYou've hit your Opus limit\x1b[0m \u{b7} resets 3:45pm")
                }
                _ => println!(
                    "You've used 85% of your weekly limit \u{b7} resets Mon 12:00am \u{b7} try /effort medium"
                ),
            }
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                if line? == "quit" {
                    break;
                }
            }
            0
        }
        // Shows what it was started with, as an agent shows the prompt it is
        // given, and then waits for the user.
        "pty-continue-wait" => {
            println!("CONTINUED");
            for arg in std::env::args().skip(2) {
                for line in arg.lines() {
                    println!("ARG|{}", line.chars().take(400).collect::<String>());
                }
            }
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                if line? == "quit" {
                    break;
                }
            }
            0
        }
        // A limit message that is followed by more work, as when an agent
        // prints a file or a quotation that happens to read like one.
        "pty-limit-then-work" => {
            println!("working");
            println!("5-hour limit reached - resets 3pm");
            io::stdout().flush()?;
            for tick in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                println!("still working {tick}");
                io::stdout().flush()?;
            }
            for line in io::stdin().lock().lines() {
                if line? == "quit" {
                    break;
                }
            }
            0
        }
        // Stays up and repeats every line it is given, so a test can tell what
        // reached the provider. Leaves on "quit".
        "pty-echo" => {
            println!("READY");
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                let line = line?;
                if line == "quit" {
                    break;
                }
                println!("ECHO|{line}");
                io::stdout().flush()?;
            }
            0
        }
        "pty-continue" => {
            println!("CONTINUED");
            for arg in std::env::args().skip(2) {
                for line in arg.lines() {
                    println!("ARG|{}", line.chars().take(400).collect::<String>());
                }
            }
            io::stdout().flush()?;
            0
        }
        "interrupted" => {
            println!("WAITING");
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                if line? == "quit" {
                    break;
                }
            }
            println!("CANCELLED");
            0
        }
        "unresponsive" => {
            println!("WAITING");
            io::stdout().flush()?;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        _ => {
            eprintln!("unknown mock scenario");
            2
        }
    };
    io::stdout().flush()?;
    std::process::exit(exit);
}
