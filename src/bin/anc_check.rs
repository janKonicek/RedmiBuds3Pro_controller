// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hardware check: cycle ANC On -> Transparency -> Off, reading the mode back
//! (run info) ~3 s after each change.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use redmibuds::client::{Client, Command, Link, Mode, Update};
    use redmibuds::hex;
    use redmibuds::protocol::{AncMode, Event};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    let (tx, rx) = channel();
    let client = Client::spawn("Redmi Buds 3 Pro".into(), Mode::Hold, move |u| {
        let _ = tx.send(u);
    });
    let t0 = Instant::now();
    let log = |u: &Update| match u {
        Update::Raw(sent, b) => println!("[{:>5} ms] {} {}", t0.elapsed().as_millis(), if *sent { "TX" } else { "RX" }, hex(b)),
        Update::Event(Event::Frame(_)) => {}
        other => println!("[{:>5} ms] {other:?}", t0.elapsed().as_millis()),
    };

    // Wait for an authenticated session and the initial ANC state.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut connected = false;
    let mut initial = None;
    while Instant::now() < deadline && !(connected && initial.is_some()) {
        if let Ok(u) = rx.recv_timeout(Duration::from_millis(500)) {
            log(&u);
            match u {
                Update::Link(Link::Connected) => connected = true,
                Update::Event(Event::Anc(m)) => initial = Some(m),
                _ => {}
            }
        }
    }
    let Some(initial) = initial else {
        println!("FAILED: no session / no ANC state within 30 s");
        std::process::exit(1);
    };
    println!("== Initial ANC mode: {initial:?}");

    let mut results = Vec::new();
    for target in [AncMode::NoiseCancelling, AncMode::Transparency, AncMode::Off] {
        println!("== Set {target:?}");
        client.send(Command::SetAnc(target));
        let mut notified = Vec::new();
        let until = Instant::now() + Duration::from_secs(3);
        while let Ok(u) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            log(&u);
            if let Update::Event(Event::Anc(m)) = u {
                notified.push(m);
            }
        }
        println!("== Read back (GET_DEVICE_RUN_INFO)");
        client.send(Command::Refresh);
        let mut readback = None;
        let until = Instant::now() + Duration::from_secs(2);
        while let Ok(u) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            log(&u);
            if let Update::Event(Event::Anc(m)) = u {
                readback = Some(m);
            }
        }
        results.push((target, notified, readback));
    }

    println!("\n== Summary");
    for (target, notified, readback) in &results {
        let ok = *readback == Some(*target);
        println!(
            "  set {:<16} notify during 3 s: {:<28} readback: {:<20} {}",
            format!("{target:?}"),
            format!("{notified:?}"),
            format!("{readback:?}"),
            if ok { "OK" } else { "MISMATCH" }
        );
    }
    client.send(Command::Shutdown);
    std::thread::sleep(Duration::from_millis(300));
    if results.iter().any(|(t, _, r)| *r != Some(*t)) {
        std::process::exit(2);
    }
}
