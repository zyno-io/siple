//! Dev tool: two in-process calls A <-> B over loopback RTP.
//!
//! Default (virtual): A sends a 1 kHz tone, B sends 440 Hz; prints stats and
//! tone detection once per second. No audio hardware is touched.
//!
//! `--device` (manual test on a real machine): A is bound to the selected
//! microphone and speaker; B plays a 440 Hz tone, so you hear it on the
//! speaker and B records what the mic picked up (`--record out.wav`,
//! stereo: L = from A's mic, R = B's tone).
//!
//! Usage: media_call [SECONDS] [--device] [--codec pcmu|pcma|g722|opus] [--srtp]
//!                   [--record out.wav] [--ringtone]

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use siple_core::audio::{AudioDevices, DeviceSelection, mic_permission};
use siple_core::config::{AudioSink, AudioSource};
use siple_core::media::{CallMediaSpec, LocalTone, LocalToneKind, MediaEngine, ToneTarget};
use siple_media::codec::AudioCodec;
use siple_media::sdp::SessionDescription;
use siple_media::sdp_neg::{LocalCaps, MediaSession, SrtpPolicy};
use tokio::sync::mpsc::unbounded_channel;

const LO: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn main() -> anyhow::Result<()> {
    let mut secs = 8u64;
    let mut device = false;
    let mut srtp = false;
    let mut ringtone = false;
    let mut codec = AudioCodec::Pcmu;
    let mut record: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--device" => device = true,
            "--srtp" => srtp = true,
            "--ringtone" => ringtone = true,
            "--codec" => {
                codec = args
                    .next()
                    .and_then(|n| AudioCodec::from_name(&n))
                    .expect("--codec pcmu|pcma|g722|opus")
            }
            "--record" => record = args.next().map(PathBuf::from),
            n => secs = n.parse().expect("SECONDS"),
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let audio = device.then(|| {
        println!("mic permission: {:?}", mic_permission());
        let h = AudioDevices::start(DeviceSelection::default());
        std::thread::sleep(Duration::from_millis(500));
        println!("audio: {:#?}", h.status());
        h
    });
    let (ev_tx, mut ev_rx) = unbounded_channel();
    let engine = MediaEngine::start(audio, rt.handle().clone(), ev_tx);

    let mut sa = CallMediaSpec::new("A", LO, LO);
    if device {
        sa.source = AudioSource::Device;
        sa.sink = AudioSink::Device;
        sa.device_bound = true;
    } else {
        sa.source = AudioSource::Tone(1000.0);
    }
    let mut sb = CallMediaSpec::new("B", LO, LO);
    sb.source = AudioSource::Tone(440.0);
    let a = engine.create_call(sa)?;
    let b = engine.create_call(sb)?;
    println!(
        "A rtp {} / B rtp {}",
        a.local_rtp_port(),
        b.local_rtp_port()
    );

    let caps = LocalCaps {
        codecs: vec![codec],
        srtp: if srtp {
            SrtpPolicy::Mandatory
        } else {
            SrtpPolicy::Off
        },
        ..Default::default()
    };
    let mut ma = MediaSession::new(caps.clone(), LO, a.local_rtp_port());
    let mut mb = MediaSession::new(caps, LO, b.local_rtp_port());
    let offer = SessionDescription::parse(&ma.create_offer())?;
    let (answer, neg_b) = mb.apply_offer(&offer).map_err(|e| anyhow::anyhow!("{e}"))?;
    let neg_a = ma
        .apply_answer(&SessionDescription::parse(&answer)?)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    a.apply(neg_a);
    b.apply(neg_b);
    if let Some(p) = &record {
        b.record_start(p, true);
    }
    if ringtone {
        engine.start_local_tone(LocalTone {
            kind: LocalToneKind::Ringtone,
            target: ToneTarget::Ringer,
        });
    }

    rt.spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            println!("  event {}: {:?}", ev.call_id, ev.kind);
        }
    });
    rt.block_on(async {
        for t in 0..secs {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let (sa, sb) = (a.stats(), b.stats());
            let heard_a = a.detect_tone(440.0, Duration::from_millis(300), -50.0);
            let heard_b = b.detect_tone(1000.0, Duration::from_millis(300), -50.0);
            println!(
                "t={}s A: tx={} rx={} act={} lvl={:.0}dBFS 440Hz={} | B: tx={} rx={} act={} 1kHz={} jitter={}ms",
                t + 1,
                sa.tx_packets,
                sa.rx_packets,
                sa.rx_active,
                sa.rx_level_dbfs,
                heard_a.await.map(|r| r.detected).unwrap_or(false),
                sb.tx_packets,
                sb.rx_packets,
                sb.rx_active,
                heard_b.await.map(|r| r.detected).unwrap_or(false),
                sb.jitter.current_delay_ms,
            );
        }
    });
    b.record_stop();
    a.close();
    b.close();
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
