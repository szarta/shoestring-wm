//! Resource use of the headless backend under fixed workloads.
//!
//! Not a pass/fail test: it starts a fresh `shoestring-wm --backend headless`
//! per workload, measures the compositor and its helpers as separate
//! processes, and prints a table. Run it from a release build, since a debug
//! build's numbers say little:
//!
//!   scripts/measure-headless.sh
//!
//! Per process it reports CPU (percent of one core), wakeups (voluntary plus
//! involuntary context switches across all threads, per second), RSS and peak
//! RSS, and thread count. With a remote viewer attached it also reports the
//! stream's bandwidth and frame rate, and the latency from a client commit to
//! the viewer seeing the new pixels.
//!
//! Linux only (it reads `/proc`). Knobs:
//!
//! - `SHOESTRING_MEASURE_SECS`: seconds per measurement window (default 10).
//! - `SHOESTRING_MEASURE_RENDERER`: `software`, `gpu`, or `both` (default:
//!   `both` when a render node exists, else `software`).
//! - `SHOESTRING_MEASURE_IDLE_SUSPEND`: `off` (default), `on`, or `both`, for
//!   `[headless] idle_suspend`.
//! - `SHOESTRING_MEASURE_JSON`: also write the results to this file.
//! - `SHOESTRING_E2E_BIN_DIR`: measure the `shoestring-wm`,
//!   `shoestring-remote-server`, and `shoestring-lock` in this directory
//!   instead of this build's, such as a release tag's build.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const OUTPUT: (u32, u32) = (1920, 1080);
const WINDOW: (u32, u32) = (800, 600);
const ANIMATION_FPS: u32 = 30;
const LATENCY_SAMPLES: usize = 100;

/// One `/proc` reading of a process.
#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    /// utime + stime, in clock ticks.
    cpu_ticks: u64,
    /// Context switches summed over the threads alive now.
    switches: u64,
    rss_kib: u64,
    peak_rss_kib: u64,
    threads: u64,
}

fn sample(pid: u32) -> Sample {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("read stat");
    // Fields after the parenthesised comm, which may contain spaces.
    let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
    let cpu_ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read status");
    let kib = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    };
    let mut switches = 0;
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))
        .expect("read tasks")
        .flatten()
    {
        let Ok(s) = std::fs::read_to_string(task.path().join("status")) else {
            continue;
        };
        switches += s
            .lines()
            .filter(|l| l.contains("ctxt_switches:"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .sum::<u64>();
    }
    Sample {
        at: Instant::now(),
        cpu_ticks,
        switches,
        rss_kib: kib("VmRSS:"),
        peak_rss_kib: kib("VmHWM:"),
        threads: kib("Threads:"),
    }
}

fn clock_ticks() -> f64 {
    // SAFETY: sysconf has no memory effects.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

/// Usage between two samples of one process.
fn usage(a: Sample, b: Sample) -> Value {
    let secs = (b.at - a.at).as_secs_f64();
    json!({
        "cpu_percent": round(100.0 * (b.cpu_ticks - a.cpu_ticks) as f64 / clock_ticks() / secs),
        "wakeups_per_s": round((b.switches.saturating_sub(a.switches)) as f64 / secs),
        "rss_mib": round(b.rss_kib as f64 / 1024.0),
        "peak_rss_mib": round(b.peak_rss_kib as f64 / 1024.0),
        "threads": b.threads,
    })
}

fn round(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The processes to measure: the WM, the remote server if running, and any
/// Xwayland the WM spawned.
fn processes(wm: &Wm) -> Vec<(&'static str, u32)> {
    let mut procs = vec![("shoestring-wm", wm.pid())];
    if let Some(pid) = wm.server_pid() {
        procs.push(("remote-server", pid));
    }
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let comm = &stat[stat.find('(').unwrap() + 1..close];
        let ppid = stat[close + 2..]
            .split(' ')
            .nth(1)
            .and_then(|p| p.parse().ok());
        if comm == "Xwayland" && ppid == Some(wm.pid()) {
            procs.push(("Xwayland", pid));
        }
    }
    procs
}

/// Measure every process while `workload` runs for `window`.
fn measure(wm: &Wm, window: Duration, workload: impl FnOnce(Duration)) -> Value {
    measure_with(wm, &[], window, workload)
}

/// [`measure`], plus `extra` processes the WM didn't spawn.
fn measure_with(
    wm: &Wm,
    extra: &[(&'static str, u32)],
    window: Duration,
    workload: impl FnOnce(Duration),
) -> Value {
    let mut procs = processes(wm);
    procs.extend_from_slice(extra);
    let before: Vec<Sample> = procs.iter().map(|&(_, pid)| sample(pid)).collect();
    workload(window);
    let mut out = serde_json::Map::new();
    for (&(name, pid), a) in procs.iter().zip(before) {
        out.insert(name.into(), usage(a, sample(pid)));
    }
    Value::Object(out)
}

fn settle() {
    // Let startup work (Xwayland, first frames) finish before measuring.
    std::thread::sleep(Duration::from_secs(2));
}

/// Repaint the client's window at [`ANIMATION_FPS`] for `window`, alternating
/// two colours. `between` runs in the gap after each repaint (until the next
/// one is due) and returns how many viewer frames it applied.
fn animate(
    client: &TestClient,
    window: Duration,
    mut between: impl FnMut(Duration) -> usize,
) -> usize {
    let period = Duration::from_secs(1) / ANIMATION_FPS;
    let end = Instant::now() + window;
    let mut frames = 0;
    let mut i = 0;
    while Instant::now() < end {
        let next = Instant::now() + period;
        client.paint([IDLE, CLICKED][i % 2]);
        i += 1;
        frames += between(next.saturating_duration_since(Instant::now()));
    }
    frames
}

/// Workload results in run order.
struct Results(Vec<Value>);

impl Results {
    fn add(&mut self, workload: impl Into<String>, mut result: Value) {
        result["workload"] = json!(workload.into());
        self.0.push(result);
    }
}

/// Run every workload on one renderer.
fn run_workloads(software: bool, idle_suspend: bool, window: Duration) -> Value {
    let options = WmOptions {
        output: OUTPUT,
        software,
        idle_suspend,
        ..WmOptions::default()
    };
    let mut results = Results(Vec::new());
    let animated = format!(
        "{}x{} window repainting at {ANIMATION_FPS} fps",
        WINDOW.0, WINDOW.1
    );

    // Idle, nothing mapped.
    let wm = Wm::start_with(options.clone());
    settle();
    results.add(
        "idle, no windows",
        json!({"processes": measure(&wm, window, std::thread::sleep)}),
    );
    drop(wm);

    // Idle, one static window.
    let mut wm = Wm::start_with(options);
    let client = TestClient::map_sized(&wm, WINDOW);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    settle();
    results.add(
        "idle, one window",
        json!({"processes": measure(&wm, window, std::thread::sleep)}),
    );

    // Rendering alone: the window repaints, nobody is watching.
    let processes = measure(&wm, window, |w| {
        animate(&client, w, |gap| {
            std::thread::sleep(gap);
            0
        });
    });
    results.add(animated.clone(), json!({"processes": processes}));

    // Idle again, now with a remote viewer attached.
    client.paint(IDLE);
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);
    viewer.wait_pixel(centre, rgb(IDLE), "first frame");
    settle();
    viewer.frames_within(Duration::from_millis(100));
    let bytes = viewer.bytes_received();
    let mut frames = 0;
    let processes = measure(&wm, window, |w| frames = viewer.frames_within(w));
    results.add(
        "idle, one window, viewer attached",
        json!({
            "processes": processes,
            "stream": stream_stats(viewer.bytes_received() - bytes, frames, window),
        }),
    );

    // A window repainting at a steady rate, watched by the viewer.
    let bytes = viewer.bytes_received();
    let mut frames = 0;
    let processes = measure(&wm, window, |w| {
        frames = animate(&client, w, |gap| viewer.frames_within(gap));
    });
    results.add(
        format!("{animated}, viewer attached"),
        json!({
            "processes": processes,
            "stream": stream_stats(viewer.bytes_received() - bytes, frames, window),
        }),
    );

    // Latency from a client commit to the viewer holding the new pixels.
    viewer.frames_within(Duration::from_millis(300));
    let samples: Vec<f64> = (0..LATENCY_SAMPLES)
        .map(|i| {
            let colour = [KEYED, CLICKED][i % 2];
            let start = Instant::now();
            client.paint(colour);
            viewer.wait_pixel(centre, rgb(colour), "latency sample");
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            // Space the samples so each one starts from an idle desktop.
            viewer.frames_within(Duration::from_millis(50));
            ms
        })
        .collect();
    results.add(
        "commit-to-viewer latency",
        json!({"latency_ms": percentiles(samples)}),
    );

    // Locked: shoestring-lock's maze redraws continuously, so a locked box is
    // never idle and keeps streaming to the viewer.
    let lock_bin = bin("shoestring-lock");
    assert!(
        lock_bin.exists(),
        "{} is missing; build it with `cargo build -p shoestring-lock`",
        lock_bin.display()
    );
    let mut lock = Command::new(lock_bin)
        .env("WAYLAND_DISPLAY", wm.wayland_socket())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn shoestring-lock");
    viewer.frames_within(Duration::from_secs(2));
    assert!(lock.try_wait().unwrap().is_none(), "shoestring-lock exited");
    let bytes = viewer.bytes_received();
    let mut frames = 0;
    let processes = measure_with(&wm, &[("shoestring-lock", lock.id())], window, |w| {
        frames = viewer.frames_within(w);
    });
    results.add(
        "locked (maze screensaver), viewer attached",
        json!({
            "processes": processes,
            "stream": stream_stats(viewer.bytes_received() - bytes, frames, window),
        }),
    );
    let _ = lock.kill();
    let _ = lock.wait();

    Value::Array(results.0)
}

fn percentiles(mut samples: Vec<f64>) -> Value {
    samples.sort_by(f64::total_cmp);
    let pct = |p: f64| round(samples[((samples.len() - 1) as f64 * p) as usize]);
    json!({"median": pct(0.5), "p95": pct(0.95), "max": pct(1.0), "samples": samples.len()})
}

fn stream_stats(bytes: u64, frames: usize, window: Duration) -> Value {
    let secs = window.as_secs_f64();
    json!({
        "kib_per_s": round(bytes as f64 / 1024.0 / secs),
        "frames_per_s": round(frames as f64 / secs),
    })
}

/// Print one renderer's results as a Markdown table.
fn print_table(renderer: &str, results: &Value) {
    println!(
        "\n### {renderer} renderer, {}x{} output\n",
        OUTPUT.0, OUTPUT.1
    );
    println!("| Workload | Process | CPU % | Wakeups/s | RSS MiB | Peak RSS MiB | Threads |");
    println!("|---|---|---|---|---|---|---|");
    for r in results.as_array().unwrap() {
        let workload = r["workload"].as_str().unwrap();
        if let Some(procs) = r["processes"].as_object() {
            for (name, u) in procs {
                println!(
                    "| {workload} | {name} | {} | {} | {} | {} | {} |",
                    u["cpu_percent"],
                    u["wakeups_per_s"],
                    u["rss_mib"],
                    u["peak_rss_mib"],
                    u["threads"]
                );
            }
        }
        if let Some(s) = r.get("stream") {
            println!(
                "| {workload} | stream | {} KiB/s, {} frames/s | | | | |",
                s["kib_per_s"], s["frames_per_s"]
            );
        }
        if let Some(l) = r.get("latency_ms") {
            println!(
                "| {workload} | viewer | median {} ms, p95 {} ms, max {} ms ({} samples) | | | | |",
                l["median"], l["p95"], l["max"], l["samples"]
            );
        }
    }
}

#[test]
#[ignore]
fn measure_headless_resource_use() {
    let secs = std::env::var("SHOESTRING_MEASURE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let window = Duration::from_secs(secs);
    let has_gpu =
        (128..=130).any(|n| std::path::Path::new(&format!("/dev/dri/renderD{n}")).exists());
    let renderers: Vec<(&str, bool)> = match std::env::var("SHOESTRING_MEASURE_RENDERER").as_deref()
    {
        Ok("software") => vec![("software", true)],
        Ok("gpu") => vec![("gpu", false)],
        Ok("both") => vec![("software", true), ("gpu", false)],
        Ok(other) => panic!("SHOESTRING_MEASURE_RENDERER={other}: use software, gpu, or both"),
        Err(_) if has_gpu => vec![("software", true), ("gpu", false)],
        Err(_) => vec![("software", true)],
    };

    let suspend: Vec<bool> = match std::env::var("SHOESTRING_MEASURE_IDLE_SUSPEND").as_deref() {
        Ok("off") | Err(_) => vec![false],
        Ok("on") => vec![true],
        Ok("both") => vec![false, true],
        Ok(other) => panic!("SHOESTRING_MEASURE_IDLE_SUSPEND={other}: use off, on, or both"),
    };

    let mut all = serde_json::Map::new();
    for (renderer, software) in renderers {
        for &idle_suspend in &suspend {
            let name = if idle_suspend {
                format!("{renderer}, idle_suspend on")
            } else {
                renderer.to_string()
            };
            let results = run_workloads(software, idle_suspend, window);
            print_table(&name, &results);
            all.insert(name, results);
        }
    }
    let report = json!({
        "output": format!("{}x{}", OUTPUT.0, OUTPUT.1),
        "window_secs": secs,
        "runs": all,
    });
    if let Ok(path) = std::env::var("SHOESTRING_MEASURE_JSON") {
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).expect("write JSON");
    }
}
