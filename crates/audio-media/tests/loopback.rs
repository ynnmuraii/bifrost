use audio_media::{
    crypto::{DirectionMaterial, Opener, Sealer},
    pipeline::{DriftResampler, Receiver, Sender},
};
use audio_model::jitter::Profile;
use std::collections::VecDeque;
use zeroize::Zeroizing;
fn material() -> DirectionMaterial {
    DirectionMaterial::new(Zeroizing::new([42; 32]), Zeroizing::new([12; 4]))
}

#[test]
fn rubato_impulse_group_delay_is_below_budget() {
    for frame in [480, 960] {
        let mut r = DriftResampler::new(frame).unwrap();
        let mut input = vec![0.0; frame * 2];
        input[100 * 2] = 1.0;
        input[100 * 2 + 1] = 1.0;
        let mut output = vec![0.0; r.output_capacity_frames() * 2];
        let n = r.process(&input, 1.0, &mut output).unwrap();
        let peak = (0..n)
            .max_by(|&a, &b| output[a * 2].abs().total_cmp(&output[b * 2].abs()))
            .unwrap();
        let measured = peak - 100;
        println!(
            "rubato septic frame={frame}: reported={} measured={measured} samples ({:.6} ms)",
            r.output_delay(),
            measured as f64 / 48.0
        );
        // rubato conservatively rounds its reported delay up by one sample.
        assert_eq!(measured, 3);
        assert_eq!(r.output_delay(), 4);
        assert!(measured < 480);
    }
}

#[derive(Clone, Copy, Debug)]
enum Loss {
    None,
    RandomFivePercent,
    Burst,
}
#[test]
fn tone_survives_loss_reordering_duplicates_delay_and_clock_drift() {
    for profile in [Profile::Ethernet, Profile::Wifi] {
        for loss in [Loss::None, Loss::RandomFivePercent, Loss::Burst] {
            for drift in [-100.0, 100.0] {
                simulate(profile, loss, drift);
            }
        }
    }
}
fn simulate(profile: Profile, loss: Loss, drift: f64) {
    let frame = usize::from(profile.frame_samples());
    let frame_ms = frame as u64 / 48;
    let mut sender = Sender::new(
        profile,
        Sealer::new(material(), [8; 16], 0).unwrap(),
        u32::MAX - 959,
    )
    .unwrap();
    let mut receiver = Receiver::new(
        profile,
        Opener::new(material(), [8; 16]).unwrap(),
        frame as u32,
    )
    .unwrap();
    let mut network = Vec::<(u64, Vec<u8>)>::new();
    let mut render = VecDeque::<f32>::new();
    let mut pcm = vec![0.0; frame * 2];
    let mut output = vec![0.0; receiver.output_capacity_samples()];
    let mut datagram = [0; 1200];
    let mut sent = 0u64;
    let mut sample = 0u64;
    let mut rng = 1234567u64;
    let mut playing = false;
    let mut first_output = None;
    let mut sum = 0.0f64;
    let mut count = 0u64;
    let mut previous = 0.0f32;
    let mut maximum_jump = 0.0f32;
    let mut starved = 0;
    let mut max_fill = 0;
    for ms in 0..20_000u64 {
        let sender_due = (sent * frame_ms) as f64 / (1.0 + drift * 1e-6);
        if ms as f64 >= sender_due {
            for pair in pcm.as_chunks_mut::<2>().0 {
                let x =
                    (sample as f64 * std::f64::consts::TAU * 1000.0 / 48_000.0).sin() as f32 * 0.2;
                pair[0] = x;
                pair[1] = x;
                sample += 1;
            }
            let n = sender.encode(ms, &pcm, &mut datagram).unwrap();
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let drop = match loss {
                Loss::None => false,
                Loss::RandomFivePercent => rng >> 32 < u64::from(u32::MAX) / 20,
                Loss::Burst => sent % 100 < 3 && sent > 0,
            };
            if !drop {
                let base = if profile == Profile::Ethernet { 3 } else { 10 };
                let extra = if sent % 37 == 5 {
                    frame_ms + 1
                } else {
                    sent % 3
                };
                network.push((ms + base + extra, datagram[..n].to_vec()));
                if sent % 29 == 7 {
                    network.push((ms + base + extra + 1, datagram[..n].to_vec()));
                }
            }
            sent += 1;
        }
        let mut i = 0;
        while i < network.len() {
            if network[i].0 <= ms {
                let (_, mut packet) = network.swap_remove(i);
                let _ = receiver.receive(ms * 48, &mut packet, |_| true);
            } else {
                i += 1;
            }
        }
        let n = receiver
            .playout(ms * 48, render.len() as f64 / 2.0, &mut output)
            .unwrap();
        render.extend(&output[..n * 2]);
        max_fill = max_fill.max(render.len() / 2);
        if !playing && render.len() >= frame * 4 {
            playing = true;
            first_output = Some(ms);
        }
        if playing {
            for _ in 0..48 * 2 {
                if let Some(x) = render.pop_front() {
                    assert!(x.is_finite());
                    maximum_jump = maximum_jump.max((x - previous).abs());
                    previous = x;
                    if ms > 1000 {
                        sum += f64::from(x) * f64::from(x);
                        count += 1;
                    }
                } else {
                    starved += 1;
                }
            }
        }
    }
    let rms = (sum / count as f64).sqrt();
    let health = receiver.health();
    println!("{profile:?} {loss:?} {drift}ppm: rms={rms:.4} first={first_output:?}ms max_fill={max_fill} lost={} underruns={} starved={starved}", health.lost_frames, health.underruns);
    assert!((0.075..0.19).contains(&rms), "{rms}");
    assert!(maximum_jump < 0.5);
    assert!(health.concealed_frames < sent / 5);
    assert!(max_fill <= frame * 4);
    assert!(starved < 48 * 2 * 100); // <100 ms of rendered silence in 20 s.
    assert!(
        first_output.unwrap()
            <= if profile == Profile::Ethernet {
                86
            } else {
                192
            }
    );
}
