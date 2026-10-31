//! Deterministic codec/mixer loopback; the native FIFO is explicitly modeled, not hardware.
use audio_media::{
    crypto::{DirectionMaterial, Opener, Sealer},
    pipeline::{Receiver, Sender},
};
use audio_model::jitter::Profile;
use std::collections::VecDeque;
use zeroize::Zeroizing;

fn material() -> DirectionMaterial {
    DirectionMaterial::new(Zeroizing::new([42; 32]), Zeroizing::new([12; 4]))
}

#[test]
fn wired_chirp_latency_and_continuous_playout() {
    let (before, before_starved) = measure_chirp(480);
    let (after, after_starved) = measure_chirp(240);
    println!("aggregate quantum | cross-correlated ms | warmup starvation frames\n10ms (before) | {before:.3} | {before_starved}\n5ms (after) | {after:.3} | {after_starved}");
    assert!((55.0..=86.0).contains(&after), "{after}ms");
    assert!(after <= before);
    assert!(after_starved <= before_starved);
    assert!(after_starved <= 48);
}

fn measure_chirp(mix_block: usize) -> (f64, usize) {
    let mut sender = Sender::new(
        Profile::Ethernet,
        Sealer::new(material(), [8; 16], 0).unwrap(),
        98_765_431,
    )
    .unwrap();
    let codec_lookahead_us = sender.codec_lookahead_us().unwrap();
    let mut receiver = Receiver::new(
        Profile::Ethernet,
        Opener::new(material(), [8; 16]).unwrap(),
        480,
    )
    .unwrap();
    let source: Vec<f32> = (0..96_000)
        .map(|i| {
            let t = (i as f64 - 9_600.0) / 48_000.0;
            if (0.0..0.020).contains(&t) {
                (std::f64::consts::TAU * (700.0 * t + 45_000.0 * t * t)).sin() as f32 * 0.3
            } else {
                0.0
            }
        })
        .collect();
    let mut network = VecDeque::new();
    let mut decoded = VecDeque::new();
    let mut aggregate = VecDeque::new();
    // Known 20 ms native device buffer. This is not a Windows latency measurement.
    let mut native = VecDeque::from(vec![0.0f32; 960]);
    let mut rendered = Vec::new();
    let mut output = vec![0.0; receiver.output_capacity_samples()];
    let mut packet = [0; 1200];
    let mut starved = 0;
    let mut max_decoded = 0;
    for ms in 0..2_000usize {
        // A packet becomes available only after its 10 ms capture frame completes.
        if ms >= 10 && ms % 10 == 0 {
            let first = (ms - 10) * 48;
            let pcm: Vec<f32> = source[first..first + 480]
                .iter()
                .flat_map(|x| [*x, *x])
                .collect();
            let n = sender.encode(ms as u64, &pcm, &mut packet).unwrap();
            network.push_back((ms + 3, packet[..n].to_vec()));
        }
        while network.front().is_some_and(|(due, _)| *due <= ms) {
            let (_, mut bytes) = network.pop_front().unwrap();
            receiver
                .receive(ms as u64 * 48, &mut bytes, |_| true)
                .unwrap();
        }
        let fill = decoded.len() + aggregate.len();
        let n = if decoded.len() < mix_block {
            receiver
                .playout(ms as u64 * 48, fill as f64, &mut output)
                .unwrap()
        } else {
            0
        };
        decoded.extend(
            output[..n * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| pair[0]),
        );
        max_decoded = max_decoded.max(decoded.len());
        if aggregate.len() < 480 {
            for _ in 0..mix_block {
                if let Some(sample) = decoded.pop_front() {
                    aggregate.push_back(sample);
                } else {
                    aggregate.push_back(0.0);
                    if ms > 100 {
                        starved += 1;
                    }
                }
            }
        }
        for _ in 0..48 {
            native.push_back(aggregate.pop_front().unwrap_or(0.0));
            rendered.push(native.pop_front().unwrap());
        }
    }
    // Cross-correlate the captured chirp with rendered PCM, not packet-count proxies.
    let reference = &source[9_600..10_560];
    let lag = (0..4_800)
        .max_by(|&a, &b| {
            let correlation = |lag: usize| {
                reference
                    .iter()
                    .enumerate()
                    .map(|(i, x)| f64::from(*x) * f64::from(rendered[9_600 + i + lag]))
                    .sum::<f64>()
            };
            correlation(a).total_cmp(&correlation(b))
        })
        .unwrap();
    let measured_ms = lag as f64 / 48.0;
    println!("modeled-native=20ms cross-correlated={measured_ms:.3}ms max-decoded={max_decoded}frames warmup-starved={starved} health={:?} drift={:?}", receiver.health(), receiver.drift_diagnostics());
    assert_eq!(receiver.health().lost_frames, 0);
    assert_eq!(receiver.health().underruns, 0);
    let latency = receiver.latency_diagnostics();
    println!("stage | us\npacketization | 10000\ncodec lookahead | {codec_lookahead_us}\njitter target | {}\nencoded timeline span (overlaps target) | {}\nmax decoded queue | {}\naggregate queue ceiling | {}\nresampler reported | {}\nnative FIFO (modeled) | 20000", latency.jitter_target_us, latency.jitter_buffer_us, max_decoded * 1_000_000 / 48_000, (480 + mix_block) * 1_000_000 / 48_000, latency.resampler_delay_us);
    assert_eq!(latency.jitter_target_us, 30_000);
    assert!(latency.jitter_buffer_us <= 40_000);
    assert!((1..=20_000).contains(&codec_lookahead_us));
    assert!((1..=1_000).contains(&latency.resampler_delay_us));
    assert!(max_decoded <= 1_440);
    (measured_ms, starved)
}
