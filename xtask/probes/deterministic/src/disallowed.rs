//! Probe: every entry of the canonical determinism configuration
//! (`xtask/lint/deterministic.clippy.toml`) is rejected (§7.14, P4). The probe runner
//! checks that each configured method, type and macro is reported, so an entry that
//! stops resolving (for example after a toolchain bump) fails the probe.

use std::time::Duration;

/// Deliberate violations: one call per disallowed method and macro.
pub fn methods_and_macros() {
    let _ = matches!(Some(1), Some(_));
    let _ = std::time::Instant::now();
    let _ = std::time::SystemTime::now();
    let _ = std::thread::spawn(|| ());
    let _ = std::thread::Builder::new().spawn(|| ());
    std::thread::scope(|_| ());
    std::thread::sleep(Duration::ZERO);
    let _ = std::hash::RandomState::new();
    let _ = std::env::var("PROBE");
    let _ = std::env::var_os("PROBE");
    let _ = std::env::vars();
    let _ = std::env::args();
    let _ = std::env::args_os();
    let _ = std::env::current_dir();
    let _ = std::env::temp_dir();
    let _ = std::fs::read("probe");
    let _ = std::fs::read_to_string("probe");
    let _ = std::fs::write("probe", b"");
    let _ = std::fs::read_dir("probe");
    let _ = std::fs::create_dir_all("probe");
    let _ = std::fs::remove_file("probe");
    let _ = std::fs::rename("probe", "probe2");
    let _ = std::fs::metadata("probe");
    let _ = std::fs::File::open("probe");
    let _ = std::fs::File::create("probe");
    let _ = std::fs::OpenOptions::new().open("probe");
    let _ = std::io::stdin();
    let _ = std::io::stdout();
    let _ = std::io::stderr();
    if std::hint::black_box(false) {
        std::process::exit(1);
    }
    if std::hint::black_box(false) {
        std::process::abort();
    }
    let _ = std::process::Command::new("probe");
    let _ = std::net::TcpStream::connect("127.0.0.1:9");
    let _ = std::net::TcpListener::bind("127.0.0.1:0");
    let _ = std::net::UdpSocket::bind("127.0.0.1:0");
    println!("probe");
    print!("probe");
    eprintln!("probe");
    eprint!("probe");
    let _ = dbg!(1_u8);
}

std::thread_local! {
    static PROBE: u8 = const { 0 };
}

/// Deliberate violations: one mention per disallowed type.
pub const fn types(
    _map: Option<&std::collections::HashMap<u8, u8>>,
    _set: Option<&std::collections::HashSet<u8>>,
    _state: Option<&std::hash::RandomState>,
    _instant: Option<&std::time::Instant>,
    _system: Option<&std::time::SystemTime>,
    _file: Option<&std::fs::File>,
    _options: Option<&std::fs::OpenOptions>,
    _stream: Option<&std::net::TcpStream>,
    _listener: Option<&std::net::TcpListener>,
    _socket: Option<&std::net::UdpSocket>,
    _command: Option<&std::process::Command>,
    _child: Option<&std::process::Child>,
    _handle: Option<&std::thread::JoinHandle<()>>,
) {
}
