//! Explicitly audible diagnostic; never run as part of cargo test.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use platform_audio_win::stream::*;
    use std::{thread, time::Duration};
    println!("Audible 1 kHz probe: in-memory PCM only; no endpoint settings changed.");
    let (exclude_tx, mut exclude_rx) = rtrb::RingBuffer::new(48_000 * 6);
    let (include_tx, mut include_rx) = rtrb::RingBuffer::new(48_000 * 6);
    let mut exclude = start_capture_excluding(std::process::id(), exclude_tx)?;
    let mut include = start_capture_including_for_probe(std::process::id(), include_tx)?;
    thread::sleep(Duration::from_millis(300));
    let (mut tx, rx) = rtrb::RingBuffer::new(48_000 * 4);
    for i in 0..48_000 * 3 {
        let value = ((2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin()
            * 10_f64.powf(-12.0 / 20.0)) as f32;
        tx.push([value; 2]).unwrap();
    }
    let mut render = start_render(EndpointSelection::FollowDefaultMultimedia, rx)?;
    thread::sleep(Duration::from_millis(3200));
    render.stop();
    thread::sleep(Duration::from_millis(200));
    exclude.stop();
    include.stop();
    fn measure(rx: &mut rtrb::Consumer<CapturedFrame>) -> (usize, f64, f64, usize) {
        let mut samples = Vec::new();
        let mut gaps = 0;
        while let Ok(frame) = rx.pop() {
            samples.push(frame.samples[0]);
            gaps += usize::from(frame.discontinuity);
        }
        let total = samples.len();
        let a = 36_000.min(total);
        let b = (a + 96_000).min(total);
        let samples = &samples[a..b];
        if samples.is_empty() {
            return (total, 0.0, 0.0, gaps);
        }
        let rms = (samples.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>()
            / samples.len() as f64)
            .sqrt();
        let coefficient = 2.0 * (2.0 * std::f64::consts::PI * 1000.0 / 48_000.0).cos();
        let (mut s1, mut s2) = (0.0, 0.0);
        for &x in samples {
            let s = f64::from(x) + coefficient * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = 2.0 * (s1 * s1 + s2 * s2 - coefficient * s1 * s2).max(0.0)
            / (samples.len() as f64).powi(2);
        (total, rms, power, gaps)
    }
    let ex = measure(&mut exclude_rx);
    let inc = measure(&mut include_rx);
    let render_stats = render.snapshot();
    println!(
        "EXCLUDE frames={} rms={:.9} tone_power={:.9} discontinuities={} dropped={}",
        ex.0,
        ex.1,
        ex.2,
        ex.3,
        exclude.snapshot().dropped_capture_frames
    );
    println!(
        "INCLUDE frames={} rms={:.9} tone_power={:.9} discontinuities={} dropped={}",
        inc.0,
        inc.1,
        inc.2,
        inc.3,
        include.snapshot().dropped_capture_frames
    );
    println!("RENDER frames={} underrun_frames={} clock_position={} clock_frequency={} qpc_100ns={} status={:?}", render_stats.transferred_frames, render_stats.underrun_frames, render_stats.device_position, render_stats.clock_frequency, render_stats.qpc_100ns, render_stats.status);
    if ex.0 >= 132_000
        && inc.0 >= 132_000
        && ex.2 < 0.000001
        && inc.2 > 0.01
        && exclude.snapshot().dropped_capture_frames == 0
        && include.snapshot().dropped_capture_frames == 0
        && render_stats.status == StreamStatus::Stopped
    {
        println!("PASS capture-exclude + positive INCLUDE control + event-driven render");
        Ok(())
    } else {
        Err("FAIL capture/render thresholds".into())
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("hw_probe requires Windows");
}
