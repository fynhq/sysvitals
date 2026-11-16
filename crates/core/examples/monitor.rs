//! Headless sampling and formatting harness.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysvitals_core::{format_frame, Config, Sampler};

fn main() {
    let mut cfg = Config::default();
    cfg.sanitize();

    let mut sampler = Sampler::new();
    let tick = Duration::from_millis(cfg.sample.interval_ms);

    println!("SysVitals core monitor (Ctrl-C to stop) — single row, then dual row\n");

    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let snap = sampler.sample(now);

        let single = format_frame(&snap, &cfg);
        println!("[single] {}", single.lines[0].render());

        cfg.display.dual_row = true;
        let dual = format_frame(&snap, &cfg);
        for line in &dual.lines {
            println!("[dual  ] {}", line.render());
        }
        cfg.display.dual_row = false;

        println!(
            "raw: cpu={:.1} mem={:.1} up={:.0}B/s down={:.0}B/s\n",
            snap.cpu_pct, snap.mem_pct, snap.up_bps, snap.down_bps
        );

        std::thread::sleep(tick);
    }
}
