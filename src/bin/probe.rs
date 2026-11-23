// SPDX-License-Identifier: AGPL-3.0-or-later
//! Feasibility probe: connect to paired Redmi Buds 3 Pro, authenticate, print
//! battery (left / right / case) and ANC mode, plus raw frames in hex.
//! If the control service refuses the link, runs connection diagnostics on
//! all custom RFCOMM services (WinRT by UUID, Winsock by UUID and by channel).
//!
//! Usage: probe [device-name-substring] [timeout-seconds]

#[cfg(not(windows))]
fn main() {
    eprintln!("probe: only the Windows transport is implemented so far");
    std::process::exit(2);
}

#[cfg(windows)]
fn diagnostics(name: &str, services: &[redmibuds::windows_bt::ServiceInfo]) {
    use redmibuds::windows_bt::{Connection, winsock_connect_test};
    // Skip the standard audio profiles (Bluetooth base UUID xxxx-0000-1000-8000-00805F9B34FB < 0x2000).
    let custom = services.iter().filter(|s| {
        let std_base = s.uuid.data2 == 0 && s.uuid.data3 == 0x1000 && s.uuid.data4 == [0x80, 0, 0, 0x80, 0x5F, 0x9B, 0x34, 0xFB];
        !(std_base && s.uuid.data1 < 0x2000)
    });
    println!("Diagnostics on custom RFCOMM services:");
    for s in custom {
        let ch = s.channel.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
        println!("  {:?} (channel {ch})", s.uuid);
        match Connection::connect_uuid(name, s.uuid, false) {
            Ok(c) => {
                println!("    WinRT by UUID:      CONNECTED");
                c.closer().close();
            }
            Err(e) => println!("    WinRT by UUID:      {e}"),
        }
        println!("    Winsock by UUID:    {:?}", winsock_connect_test(name, s.uuid, 0));
        if let Some(c) = s.channel {
            println!("    Winsock channel {c:>2}: {:?}", winsock_connect_test(name, s.uuid, c as u32));
        }
    }
}

#[cfg(windows)]
fn main() {
    use redmibuds::hex;
    use redmibuds::protocol::{Battery, Event, Session};
    use redmibuds::windows_bt::{Connection, list_rfcomm_services, paired_devices};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "Redmi Buds 3 Pro".to_owned());
    let timeout = Duration::from_secs(args.next().and_then(|s| s.parse().ok()).unwrap_or(15));

    match paired_devices() {
        Ok(list) => {
            println!("Paired Bluetooth devices:");
            for d in &list {
                println!("  - {}", d.name);
            }
        }
        Err(e) => println!("Could not list paired devices: {e}"),
    }

    let services = match list_rfcomm_services(&name) {
        Ok(list) => {
            println!("RFCOMM services (uncached SDP):");
            for s in &list {
                let ch = s.channel.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                println!("  - {:?}  channel {ch}", s.uuid);
            }
            list
        }
        Err(e) => {
            println!("RFCOMM service listing failed: {e}");
            Vec::new()
        }
    };

    let t0 = Instant::now();
    println!("Connecting to \"{name}\" (RFCOMM {:032x}) ...", redmibuds::SERVICE_UUID);
    let conn = match Connection::connect(&name) {
        Ok(c) => c,
        Err(e) => {
            println!("CONNECT FAILED: {e}");
            diagnostics(&name, &services);
            std::process::exit(1);
        }
    };
    println!("Connected to {} in {} ms", conn.device_name, t0.elapsed().as_millis());

    // Watchdog: close the socket after the timeout to unblock the read.
    let done = Arc::new(AtomicBool::new(false));
    {
        let closer = conn.closer();
        let done = done.clone();
        std::thread::spawn(move || {
            let end = Instant::now() + timeout;
            while Instant::now() < end && !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            if !done.load(Ordering::Relaxed) {
                eprintln!("(timeout after {}s, closing)", timeout.as_secs());
                closer.close();
            }
        });
    }

    let ms = |t: Instant| t.elapsed().as_millis();
    let send = |data: &[u8]| {
        println!("[{:>5} ms] TX {}", ms(t0), hex(data));
        if let Err(e) = conn.write(data) {
            println!("WRITE FAILED: 0x{:08X} {}", e.code().0 as u32, e.message());
        }
    };

    let mut session = Session::new();
    send(&session.start());

    let fmt_bat = |b: Option<Battery>| match b {
        Some(b) => format!("{:>3}%{}", b.level, if b.charging { " (charging)" } else { "" }),
        None => "n/a".to_owned(),
    };

    let (mut got_battery, mut got_anc, mut got_gestures) = (false, false, false);
    let mut buf = [0u8; 1024];
    loop {
        let n = match conn.read(&mut buf) {
            Ok(0) => {
                println!("Connection closed by peer");
                break;
            }
            Ok(n) => n,
            Err(e) => {
                println!("READ ended: 0x{:08X} {}", e.code().0 as u32, e.message());
                break;
            }
        };
        println!("[{:>5} ms] RX {}", ms(t0), hex(&buf[..n]));
        let (events, replies) = session.on_bytes(&buf[..n]);
        for ev in events {
            match ev {
                Event::Frame(m) => println!(
                    "           frame type={:?} op={:?} seq={} payload=[{}]",
                    m.kind, m.opcode, m.seq, hex(&m.payload)
                ),
                Event::Authenticated => println!(">> Authenticated"),
                Event::DeviceInfo { fw, fw2, vid, pid } => {
                    println!(">> Firmware {fw} / {fw2}, VID 0x{vid:04X} PID 0x{pid:04X}")
                }
                Event::Battery(b) => {
                    got_battery = true;
                    println!(
                        ">> Battery: left {} | right {} | case {}",
                        fmt_bat(b.left),
                        fmt_bat(b.right),
                        fmt_bat(b.case)
                    );
                }
                Event::Anc(m) => {
                    got_anc = true;
                    println!(">> ANC mode: {m:?}");
                }
                Event::WearDetection(on) => println!(">> Wear detection: {}", if on { "on" } else { "off" }),
                Event::Gesture { tap, left, right } => {
                    got_gestures = true;
                    println!(">> Gesture {tap:?}: left 0x{left:02X}, right 0x{right:02X}");
                }
                Event::InEar(b) => println!(">> In-ear bits: {b:08b}"),
                Event::Nack { opcode, seq, status } => println!(">> REJECTED {opcode:?} seq {seq} status 0x{status:02X}"),
                Event::Unhandled(_) => {}
            }
        }
        for r in replies {
            send(&r);
        }
        if got_battery && got_anc && got_gestures {
            println!("Got battery, ANC and gestures, done in {} ms.", ms(t0));
            break;
        }
    }
    done.store(true, Ordering::Relaxed);
    conn.closer().close();
    if !(got_battery && got_anc) {
        std::process::exit(3);
    }
}
