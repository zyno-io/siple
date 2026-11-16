//! Dev tool: mic -> speaker passthrough through the 48 kHz resampling path, or a 440 Hz tone.
//!
//! Usage: audio_loopback [SECONDS] [--tone]
//!   (default 5 s)

use std::time::{Duration, Instant};

use siple_core::audio::{AudioDevices, DeviceSelection, NO_DEVICE, Resampler, mic_permission};

const INTERNAL: u32 = 48_000;

fn main() {
    let mut secs = 5u64;
    let mut tone = false;
    for a in std::env::args().skip(1) {
        if a == "--tone" {
            tone = true;
        } else if let Ok(n) = a.parse() {
            secs = n;
        }
    }

    println!("mic permission: {:?}", mic_permission());
    let mut sel = DeviceSelection::default();
    if tone {
        sel.input = NO_DEVICE.into();
    }
    let audio = AudioDevices::start(sel);
    std::thread::sleep(Duration::from_millis(500)); // let the device thread open streams

    let list = audio.list();
    println!("inputs:");
    for d in &list.inputs {
        println!("  {} {:?}", if d.is_default { "*" } else { " " }, d);
    }
    println!("outputs:");
    for d in &list.outputs {
        println!("  {} {:?}", if d.is_default { "*" } else { " " }, d);
    }
    println!("status: {:#?}", audio.status());

    let worker = {
        let audio = audio.clone();
        std::thread::spawn(move || run(audio, secs, tone))
    };

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let s = audio.status();
        println!(
            "t={:>2}s cap_rate={:?} play_rate={:?} cap_avail={} play_fill={} overflows={} underflows={} gen={} err={:?}",
            start.elapsed().as_secs(),
            audio.capture_rate(),
            audio.playback_rate(),
            audio.capture_available(),
            audio.playback_fill(),
            s.input_overflows,
            s.output_underflows,
            s.generation,
            s.last_error,
        );
    }
    let _ = worker.join();
}

fn run(audio: siple_core::audio::AudioHandle, secs: u64, tone: bool) {
    audio.notifier().register_current_thread();
    let end = Instant::now() + Duration::from_secs(secs);
    let Some(out_rate) = audio.playback_rate() else {
        eprintln!("no output device");
        return;
    };
    let target_fill = (out_rate / 25) as usize; // 40 ms
    let mut phase = 0.0f64;
    let mut up: Option<Resampler> = None; // device -> 48k
    let mut down = Resampler::new(INTERNAL, out_rate, (INTERNAL / 100) as usize).unwrap();
    if !tone {
        match audio.capture_rate() {
            Some(r) => up = Some(Resampler::new(r, INTERNAL, (r / 100) as usize).unwrap()),
            None => eprintln!("no input device (mic permission?); playing silence"),
        }
    }
    let mut cap = vec![0.0f32; 4096];
    let mut at48: Vec<f32> = Vec::new();
    let mut pcm: Vec<f32> = Vec::new();
    // Drop whatever piled up in the capture ring before this thread started.
    while audio.capture_read(&mut cap) == cap.len() {}
    let mut peak = 0.0f32;
    let mut last_report = Instant::now();
    while Instant::now() < end {
        pcm.clear();
        if tone {
            // Produce 10 ms blocks at 48 kHz until the ring holds ~40 ms.
            if audio.playback_fill() < target_fill {
                at48.clear();
                for _ in 0..(INTERNAL / 100) {
                    at48.push((phase.sin() * 0.2) as f32);
                    phase += 2.0 * std::f64::consts::PI * 440.0 / INTERNAL as f64;
                }
                down.process(&at48, &mut pcm);
            }
        } else if let Some(up) = up.as_mut() {
            let n = audio.capture_read(&mut cap);
            peak = cap[..n].iter().fold(peak, |m, v| m.max(v.abs()));
            at48.clear();
            up.process(&cap[..n], &mut at48);
            down.process(&at48, &mut pcm);
        }
        if !pcm.is_empty() && audio.playback_fill() < target_fill * 2 {
            audio.playback_write(&pcm);
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            if !tone {
                println!("  mic peak over last second: {peak:.4}");
            }
            peak = 0.0;
            last_report = Instant::now();
        }
        audio.notifier().wait_timeout(Duration::from_millis(10));
    }
}
