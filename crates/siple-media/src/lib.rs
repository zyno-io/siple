//! Pure media primitives for Siple: RTP/RTCP, SRTP, codecs, DTMF, tones.
//!
//! No I/O, no async, no sockets. Callers own pacing and transport.
//! Errors are `anyhow::Error`.

#![cfg_attr(test, allow(clippy::needless_range_loop, clippy::identity_op))]

pub mod codec;
pub mod dtmf;
pub mod g711;
pub mod goertzel;
pub mod jitter;
pub mod pcm_frames;
pub mod rtcp;
pub mod rtp;
pub mod sdp;
pub mod sdp_neg;
pub mod srtp;
pub mod tone;
pub mod vad;
