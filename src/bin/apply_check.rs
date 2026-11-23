// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hardware check of setting changes through the on-demand client (same path
//! as the tray app): ANC on, wear detection off, wear detection on, ANC off.
//! Prints raw frames, client logs and the apply result of each step.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use redmibuds::client::{Apply, Client, Command, Mode, Update};
    use redmibuds::hex;
    use redmibuds::protocol::{AncMode, Event};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    let (tx, rx) = channel();
    let client = Client::spawn(
        "Redmi Buds 3 Pro".into(),
        Mode::OnDemand { refresh_every: Duration::from_secs(3600), linger: Duration::from_millis(1500) },
        move |u| {
            let _ = tx.send(u);
        },
    );
    let t0 = Instant::now();
    let log = |u: &Update| match u {
        Update::Raw(sent, b) => println!("[{:>6} ms] {} {}", t0.elapsed().as_millis(), if *sent { "TX" } else { "RX" }, hex(b)),
        Update::Event(Event::Frame(_) | Event::Gesture { .. } | Event::DeviceInfo { .. }) => {}
        other => println!("[{:>6} ms] {other:?}", t0.elapsed().as_millis()),
    };
    // Wait for the startup read to finish (session closed).
    let wait_idle = |rx: &std::sync::mpsc::Receiver<Update>, max: Duration| -> Vec<Update> {
        let mut got = Vec::new();
        let end = Instant::now() + max;
        while let Ok(u) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            log(&u);
            let stop = matches!(u, Update::Apply(Apply::Done | Apply::Failed { .. }));
            got.push(u);
            if stop {
                break;
            }
        }
        got
    };
    println!("== startup read");
    let _ = wait_idle(&rx, Duration::from_secs(6));

    let steps = [
        ("ANC on", Command::SetAnc(AncMode::NoiseCancelling)),
        ("wear detection off", Command::SetWearDetection(false)),
        ("wear detection on", Command::SetWearDetection(true)),
        ("ANC off", Command::SetAnc(AncMode::Off)),
    ];
    let mut summary = Vec::new();
    for (name, cmd) in steps {
        println!("\n== {name}");
        let t = Instant::now();
        client.send(cmd);
        let got = wait_idle(&rx, Duration::from_secs(45));
        let nacks = got.iter().filter(|u| matches!(u, Update::Event(Event::Nack { .. }))).count();
        let result = got.iter().rev().find_map(|u| if let Update::Apply(a) = u { Some(*a) } else { None });
        summary.push((name, result, nacks, t.elapsed().as_millis()));
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("\n== Summary");
    for (name, result, nacks, ms) in summary {
        println!("  {name:<20} result: {result:<40} rejected (NACK) frames: {nacks}  took {ms} ms", result = format!("{result:?}"));
    }
    client.send(Command::Shutdown);
    std::thread::sleep(Duration::from_millis(300));
}
