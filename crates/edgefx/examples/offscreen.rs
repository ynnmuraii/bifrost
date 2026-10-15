//! Offscreen only: `cargo run -p edgefx --release --example offscreen`.
//! Writes shared-rasterizer PNGs to target/ui-shots (or the first argument), then
//! measures process CPU over three seconds each of idle, 30 Hz approach, and
//! 256 alternating enter/cross updates per 30 Hz frame. No windows or input APIs.
use edgefx::{Canvas, Edge, Effect, Strip, Target, Update, FRAME_S};
use std::{
    error::Error,
    fs,
    hint::black_box,
    path::Path,
    thread,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn Error>> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/ui-shots".into());
    fs::create_dir_all(&output)?;
    for (name, edge) in [
        ("top", Edge::Top),
        ("bottom", Edge::Bottom),
        ("left", Edge::Left),
        ("right", Edge::Right),
    ] {
        let target = Target {
            edge,
            start: 0.0,
            end: 640.0,
            along: 320.0,
        };
        let mut fx = Effect::new();
        for _ in 0..18 {
            fx.update(Update::Proximity {
                target,
                distance: 24.0,
            });
            fx.step(FRAME_S);
        }
        shot(&output, name, "approach", &fx)?;
        fx.update(Update::Crossed(Some(target)));
        fx.step(FRAME_S * 2.0);
        shot(&output, name, "crossed", &fx)?;
        fx.update(Update::Entered(target));
        fx.step(FRAME_S * 2.0);
        shot(&output, name, "entered", &fx)?;
    }
    println!("CPU method: process kernel+user time / monotonic wall time; 100% = one core.\n3 seconds/scenario, 30 Hz deadlines; shared step+draw, preallocated strip buffer; no compositor/IPC/GPU cost.");
    println!("CPU counters may be quantized: a zero sample is below counter resolution, not proof of zero cost. frame-work is monotonic elapsed update+step+draw time, not CPU time.");
    for mode in ["idle", "active", "ping-pong"] {
        measure(mode)?;
    }
    Ok(())
}

fn shot(output: &str, edge: &str, phase: &str, fx: &Effect) -> Result<(), Box<dyn Error>> {
    let (w, h) = fx.strip().expect("active screenshot").size_px(1.5);
    let mut pixels = vec![0; (w * h) as usize];
    fx.draw(&mut Canvas::new(&mut pixels, w as usize, h as usize), 1.5);
    assert!(pixels.iter().any(|p| p >> 24 != 0), "empty screenshot");
    let path = Path::new(output).join(format!("edgefx-{edge}-{phase}.png"));
    png(&path, w, h, &pixels)?;
    println!(
        "{} ({w}x{h}; premultiplied pixels composited over #181818)",
        path.display()
    );
    Ok(())
}

fn measure(mode: &str) -> Result<(), Box<dyn Error>> {
    let target = Target {
        edge: Edge::Top,
        start: 0.0,
        end: 1600.0,
        along: 800.0,
    };
    let mut fx = Effect::new();
    let (fw, fh) = Strip::of(&target).size_px(1.0);
    let mut pixels = vec![0; (fw * fh) as usize];
    let interval = Duration::from_secs_f64(1.0 / 30.0);
    let duration = Duration::from_secs(3);
    let cpu_start = cpu_time()?;
    let start = Instant::now();
    let mut frames = 0;
    let mut updates = 0;
    let mut work = Duration::ZERO;
    if mode == "idle" {
        assert!(!fx.is_active());
        thread::sleep(duration); // An idle host has no frame timer or draw work.
    } else {
        while start.elapsed() < duration {
            let frame_start = Instant::now();
            if mode == "active" {
                fx.update(Update::Proximity {
                    target,
                    distance: 24.0,
                });
                updates += 1;
            } else {
                for i in 0..256 {
                    fx.update(if i % 2 == 0 {
                        Update::Entered(target)
                    } else {
                        Update::Crossed(Some(target))
                    });
                }
                updates += 256;
            }
            fx.step(FRAME_S);
            fx.draw(&mut Canvas::new(&mut pixels, fw as usize, fh as usize), 1.0);
            black_box(&pixels);
            frames += 1;
            work += frame_start.elapsed();
            thread::sleep((interval * frames).saturating_sub(start.elapsed()));
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let cpu = cpu_time()?.saturating_sub(cpu_start).as_secs_f64();
    println!("{mode}: wall={wall:.6}s cpu={cpu:.6}s one-core={:.3}% frame-work={:.3}ms frames={frames} updates={updates}", cpu / wall * 100.0, work.as_secs_f64() * 1000.0);
    Ok(())
}

#[cfg(windows)]
fn cpu_time() -> std::io::Result<Duration> {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            process: *mut std::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let (mut creation, mut exit, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: current-process pseudo-handle is valid; all output pointers refer to live FILETIME-layout values.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ticks = |t: FileTime| (u64::from(t.high) << 32) | u64::from(t.low);
    Ok(Duration::from_nanos((ticks(kernel) + ticks(user)) * 100))
}

#[cfg(not(windows))]
fn cpu_time() -> std::io::Result<Duration> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "CPU measurement currently uses Windows GetProcessTimes; PNG generation is portable",
    ))
}

// PNG with RGB scanlines and uncompressed DEFLATE: std-only and easy to inspect.
// Screenshots intentionally show the real premultiplied raster on a dark background.
fn png(path: &Path, w: u32, h: u32, pixels: &[u32]) -> std::io::Result<()> {
    let mut raw = Vec::with_capacity((w * h * 3 + h) as usize);
    for row in pixels.chunks_exact(w as usize) {
        raw.push(0); // PNG filter: none.
        for &p in row {
            let inv = 255 - (p >> 24);
            for shift in [16, 8, 0] {
                raw.push((((p >> shift) & 255) + 24 * inv / 255).min(255) as u8);
            }
        }
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks = raw.chunks(65535);
    let count = blocks.len();
    for (i, block) in blocks.enumerate() {
        zlib.push(u8::from(i + 1 == count));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::from(w.to_be_bytes());
    header.extend_from_slice(&h.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    fs::write(path, out)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = !0u32;
    for byte in kind.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
}
