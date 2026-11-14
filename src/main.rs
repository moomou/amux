use std::io::Read;

fn debug_emu(args: &[String]) {
    let cols: usize = args[2].parse().unwrap();
    let rows: usize = args[3].parse().unwrap();
    let mut data = Vec::new();
    std::io::stdin().read_to_end(&mut data).unwrap();
    let mut e = amux::term::Emulator::new(cols, rows, 5000);
    e.feed(&data);
    if let (Some(nc), Some(nr)) = (args.get(4), args.get(5)) {
        e.term.resize(nc.parse().unwrap(), nr.parse().unwrap());
    }
    for l in e.term.hist.iter() {
        println!("H|{}|", amux::history::line_text(l));
    }
    for (i, l) in e.term.screen_text().iter().enumerate() {
        println!("{:2}|{}|", i, l);
    }
    let (x, y) = e.term.cursor();
    println!("cursor {} {} pending={}", x, y, e.term.cursor_pending());
}

/// `amux __bench-emu FILE [cols rows]`: emulator throughput without any IO.
fn bench_emu(args: &[String]) {
    let data = std::fs::read(&args[2]).expect("read input");
    let cols = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(200);
    let rows = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(50);
    let mut e = amux::term::Emulator::new(cols, rows, 10000);
    let t = std::time::Instant::now();
    for chunk in data.chunks(4096) {
        e.feed(chunk);
    }
    let s = t.elapsed().as_secs_f64();
    println!("{:.1} MB in {:.3}s = {:.1} MB/s ({} history lines)", data.len() as f64 / 1e6, s, data.len() as f64 / 1e6 / s, e.term.hist.len());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("debug-emu") => debug_emu(&args),
        Some("__bench-emu") => bench_emu(&args),
        Some("__keeper") => std::process::exit(amux::keeper::main(args.get(2).map(|s| s.as_str()).unwrap_or("{}"))),
        Some("__tmux-bridge") => std::process::exit(amux::bridge::main(&args[2..])),
        _ => {}
    }
}
