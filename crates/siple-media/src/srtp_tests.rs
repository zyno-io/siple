use super::*;

const SUITE: SrtpSuite = SrtpSuite::AesCm128HmacSha1_80;

fn make_test_key() -> String {
    // 30 bytes of test key material, base64 encoded
    let key_material = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, // 16-byte master key
        0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
        0x1E, // 14-byte master salt
    ];
    // Encode to base64
    base64_encode(&key_material)
}

#[test]
fn test_srtp_roundtrip() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    // Build a simple RTP packet
    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);

    // Protect (encrypt + auth)
    let srtp = protect_ctx.protect(&rtp).unwrap();
    assert_eq!(srtp.len(), rtp.len() + SUITE.rtp_auth_tag_len());

    // Payload should be different (encrypted)
    assert_ne!(&srtp[12..12 + 160], &rtp[12..12 + 160]);

    // Unprotect (verify + decrypt)
    let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
    assert_eq!(decrypted, rtp);
}

#[test]
fn test_srtp_auth_failure() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xBB; 80]);
    let mut srtp = protect_ctx.protect(&rtp).unwrap();

    // Tamper with a byte
    srtp[20] ^= 0xFF;

    // Should fail auth
    assert!(unprotect_ctx.unprotect(&srtp).is_err());
}

#[test]
fn test_srtp_multiple_packets() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    for seq in 0..10u16 {
        let rtp = crate::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xABCD,
            false,
            &[seq as u8; 160],
        );
        let srtp = protect_ctx.protect(&rtp).unwrap();
        let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
        assert_eq!(decrypted, rtp, "roundtrip failed for seq {}", seq);
    }
}

#[test]
fn test_srtp_key_lifetime_is_enforced_for_protect_and_unprotect() {
    let key = make_test_key();
    let first_rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 80]);
    let second_rtp = crate::rtp::RtpHeader::build(0, 2, 320, 0x12345678, false, &[0xBB; 80]);

    let mut limited_protector = SrtpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    limited_protector.protect(&first_rtp).unwrap();
    let protect_error = limited_protector.protect(&second_rtp).unwrap_err();
    assert!(protect_error.to_string().contains("lifetime exhausted"));

    let mut normal_protector = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let first_srtp = normal_protector.protect(&first_rtp).unwrap();
    let second_srtp = normal_protector.protect(&second_rtp).unwrap();
    let mut limited_receiver = SrtpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    assert_eq!(limited_receiver.unprotect(&first_srtp).unwrap(), first_rtp);
    let unprotect_error = limited_receiver.unprotect(&second_srtp).unwrap_err();
    assert!(unprotect_error.to_string().contains("lifetime exhausted"));
}

#[test]
fn test_srtp_key_lifetime_is_per_ssrc_stream() {
    let key = make_test_key();
    let a1 = crate::rtp::RtpHeader::build(0, 1, 160, 0xAAAA, false, &[0x11; 80]);
    let a2 = crate::rtp::RtpHeader::build(0, 2, 320, 0xAAAA, false, &[0x22; 80]);
    let b1 = crate::rtp::RtpHeader::build(0, 1, 160, 0xBBBB, false, &[0x33; 80]);
    let b2 = crate::rtp::RtpHeader::build(0, 2, 320, 0xBBBB, false, &[0x44; 80]);

    let mut limited_protector = SrtpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    limited_protector.protect(&a1).unwrap();
    limited_protector.protect(&b1).unwrap();
    assert!(limited_protector.protect(&a2).is_err());
    assert!(limited_protector.protect(&b2).is_err());

    let mut normal_protector = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let a1 = normal_protector.protect(&a1).unwrap();
    let b1 = normal_protector.protect(&b1).unwrap();
    let a2 = normal_protector.protect(&a2).unwrap();
    let b2 = normal_protector.protect(&b2).unwrap();
    let mut limited_receiver = SrtpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    limited_receiver.unprotect(&a1).unwrap();
    limited_receiver.unprotect(&b1).unwrap();
    assert!(limited_receiver.unprotect(&a2).is_err());
    assert!(limited_receiver.unprotect(&b2).is_err());
}

#[test]
fn test_srtp_replay_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xCC; 160]);
    let srtp = protect_ctx.protect(&rtp).unwrap();

    // First unprotect should succeed
    let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
    assert_eq!(decrypted, rtp);

    // Replaying the same packet should fail
    let result = unprotect_ctx.unprotect(&srtp);
    assert!(result.is_err(), "replay should be rejected");
    assert!(result.unwrap_err().to_string().contains("replay"));
}

#[test]
fn test_srtp_old_packet_outside_window_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    // Send packet seq=0
    let rtp0 = crate::rtp::RtpHeader::build(0, 0, 0, 0xABCD, false, &[0x00; 80]);
    let srtp0 = protect_ctx.protect(&rtp0).unwrap();

    // Advance well past the 64-packet window
    for seq in 1..=100u16 {
        let rtp =
            crate::rtp::RtpHeader::build(0, seq, seq as u32 * 160, 0xABCD, false, &[seq as u8; 80]);
        let srtp = protect_ctx.protect(&rtp).unwrap();
        unprotect_ctx.unprotect(&srtp).unwrap();
    }

    // Now try to unprotect seq=0 — should be rejected (outside window)
    let result = unprotect_ctx.unprotect(&srtp0);
    assert!(
        result.is_err(),
        "old packet outside window should be rejected"
    );
}

#[test]
fn test_srtp_reset_sequence_state_accepts_restarted_low_seq() {
    // Reproduces the post-hold "no audio" bug: peer sends RTCP BYE on hold,
    // then resumes with a fresh low sequence number on unhold. Without
    // reset_sequence_state(), the replay window rejects the resumed packets
    // as "too old" and decrypt fails silently for the entire call.
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    // Phone runs up to seq=200 before going on hold
    for seq in 1..=200u16 {
        let rtp =
            crate::rtp::RtpHeader::build(0, seq, seq as u32 * 160, 0xABCD, false, &[seq as u8; 80]);
        let srtp = protect_ctx.protect(&rtp).unwrap();
        unprotect_ctx.unprotect(&srtp).unwrap();
    }

    // Phone unholds, restarts its protect context with the SAME SSRC but a
    // fresh low sequence (a new SSRC would be auto-accepted per-stream; the
    // case that still needs a reset is a same-SSRC sequence restart).
    let mut restarted_protect = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let restart_rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0xABCD, false, &[0xAA; 80]);
    let restart_srtp = restarted_protect.protect(&restart_rtp).unwrap();

    // Without reset, the low-seq packet on the SAME SSRC must be rejected.
    let pre_reset = unprotect_ctx.unprotect(&restart_srtp);
    assert!(
        pre_reset.is_err(),
        "sanity: same-SSRC low-seq packet on a stale context should be rejected before reset"
    );

    // After reset_sequence_state, the same packet must decrypt successfully
    unprotect_ctx.reset_sequence_state();
    let decrypted = unprotect_ctx
        .unprotect(&restart_srtp)
        .expect("restarted low-seq packet should decrypt after reset");
    assert_eq!(decrypted, restart_rtp);
}

#[test]
fn test_srtp_reset_sequence_state_preserves_keys() {
    // The key derivation should survive a reset — only the replay/sequence
    // tracking is cleared. Verified by checking that the cipher_key/auth_key
    // bytes are unchanged across a reset.
    let key = make_test_key();
    let mut ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    // Pump the context so it has non-default state
    let rtp = crate::rtp::RtpHeader::build(0, 50, 8000, 0x11223344, false, &[0x55; 80]);
    let srtp = {
        let mut twin = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
        twin.protect(&rtp).unwrap()
    };
    ctx.unprotect(&srtp).unwrap();

    let cipher_before = ctx.cipher_key.clone();
    let salt_before = ctx.cipher_salt;
    let auth_before = ctx.auth_key;

    ctx.reset_sequence_state();

    assert_eq!(
        ctx.cipher_key, cipher_before,
        "cipher_key must be preserved"
    );
    assert_eq!(
        ctx.cipher_salt, salt_before,
        "cipher_salt must be preserved"
    );
    assert_eq!(ctx.auth_key, auth_before, "auth_key must be preserved");
    let stream = ctx
        .streams
        .get(&0x11223344)
        .expect("reset must retain per-SSRC lifetime accounting");
    assert!(!stream.seq_initialized, "sequence state must be reset");
    assert_eq!(stream.replay_window, 0, "replay state must be reset");
    assert_eq!(
        stream.accepted_packets, 1,
        "reset must not renew the master-key lifetime"
    );
}

#[test]
fn test_srtcp_same_key_restart_stays_rejected() {
    // SRTCP analog of the SRTP test: peer restarts with a fresh SRTCP
    // index of 0 after RTCP BYE / hold. Without reset_recv_state(), the
    // replay window rejects the restarted low-index packets.
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    let mut stats = crate::rtcp::RtcpStats::new();
    let rtcp = crate::rtcp::build_sr_rr(0x11111111, 0x22222222, &mut stats, 0, 8000);

    // Run the protect context past the 64-packet replay window
    for _ in 0..200 {
        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
        unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
    }

    // Peer restarts SRTCP from index 0
    let mut restarted_protect = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let restart_srtcp = restarted_protect.protect_rtcp(&rtcp).unwrap();

    let pre_reset = unprotect_ctx.unprotect_rtcp(&restart_srtcp);
    assert!(
        pre_reset.is_err(),
        "sanity: low-index SRTCP on a stale context should be rejected before reset"
    );

    assert!(unprotect_ctx.unprotect_rtcp(&restart_srtcp).is_err());
    let current = protect_ctx.protect_rtcp(&rtcp).unwrap();
    assert!(unprotect_ctx.unprotect_rtcp(&current).is_ok());
}

#[test]
fn test_base64_decode() {
    let decoded = base64_decode("AQIDBA==").unwrap();
    assert_eq!(decoded, vec![1, 2, 3, 4]);
}

#[test]
fn test_base64_invalid_chars() {
    let result = base64_decode("AQID!A==");
    assert!(
        result.is_err(),
        "base64_decode should reject invalid characters"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Invalid base64"),
        "error message should mention invalid base64: {}",
        err_msg
    );
}

// ---- SRTCP tests ----

#[test]
fn test_srtcp_roundtrip() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    // Build a simple RTCP SR (use the rtcp module)
    let mut stats = crate::rtcp::RtcpStats::new();
    stats.packets_sent = 50;
    stats.octets_sent = 8000;
    let rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

    let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
    // Should be longer: +4 (E+index) +10 (auth tag)
    assert_eq!(srtcp.len(), rtcp.len() + 4 + SRTCP_AUTH_TAG_LEN);

    // First 8 bytes (header+SSRC) should be in the clear
    assert_eq!(&srtcp[..4], &rtcp[..4], "RTCP header should be in clear");
    assert_eq!(&srtcp[4..8], &rtcp[4..8], "SSRC should be in clear");

    // Payload should be different (encrypted)
    if rtcp.len() > 8 {
        assert_ne!(
            &srtcp[8..rtcp.len()],
            &rtcp[8..],
            "payload should be encrypted"
        );
    }

    let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
    assert_eq!(decrypted, rtcp);
}

#[test]
fn test_srtcp_key_lifetime_is_enforced_for_protect_and_unprotect() {
    let key = make_test_key();
    let mut stats = crate::rtcp::RtcpStats::new();
    let first_rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
    let second_rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 160, 8000);

    let mut limited_protector = SrtcpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    limited_protector.protect_rtcp(&first_rtcp).unwrap();
    let protect_error = limited_protector.protect_rtcp(&second_rtcp).unwrap_err();
    assert!(protect_error.to_string().contains("lifetime exhausted"));

    let mut normal_protector = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let first_srtcp = normal_protector.protect_rtcp(&first_rtcp).unwrap();
    let second_srtcp = normal_protector.protect_rtcp(&second_rtcp).unwrap();
    let mut limited_receiver = SrtcpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    assert_eq!(
        limited_receiver.unprotect_rtcp(&first_srtcp).unwrap(),
        first_rtcp
    );
    let unprotect_error = limited_receiver.unprotect_rtcp(&second_srtcp).unwrap_err();
    assert!(unprotect_error.to_string().contains("lifetime exhausted"));
}

#[test]
fn test_srtcp_key_lifetime_is_per_ssrc_stream() {
    let key = make_test_key();
    let a = [0x80, 201, 0, 1, 0, 0, 0xAA, 0xAA];
    let b = [0x80, 201, 0, 1, 0, 0, 0xBB, 0xBB];

    let mut limited_protector = SrtcpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    limited_protector.protect_rtcp(&a).unwrap();
    limited_protector.protect_rtcp(&b).unwrap();
    assert!(limited_protector.protect_rtcp(&a).is_err());
    assert!(limited_protector.protect_rtcp(&b).is_err());

    let mut normal_protector = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let a1 = normal_protector.protect_rtcp(&a).unwrap();
    let b1 = normal_protector.protect_rtcp(&b).unwrap();
    let a2 = normal_protector.protect_rtcp(&a).unwrap();
    let b2 = normal_protector.protect_rtcp(&b).unwrap();
    let mut limited_receiver = SrtcpContext::from_sdes_key_with_lifetime(SUITE, &key, 1).unwrap();
    limited_receiver.unprotect_rtcp(&a1).unwrap();
    limited_receiver.unprotect_rtcp(&b1).unwrap();
    assert!(limited_receiver.unprotect_rtcp(&a2).is_err());
    assert!(limited_receiver.unprotect_rtcp(&b2).is_err());
}

#[test]
fn test_srtcp_auth_failure() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    let mut stats = crate::rtcp::RtcpStats::new();
    let rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
    let mut srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();

    // Tamper with the encrypted payload
    srtcp[10] ^= 0xFF;

    assert!(unprotect_ctx.unprotect_rtcp(&srtcp).is_err());
}

#[test]
fn test_srtp_sequence_wraparound() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    // Send a realistic stream of packets up through and past the 16-bit
    // sequence number wraparound boundary (65534, 65535, 0, 1, ...).
    // Start well before the boundary to establish steady state.
    let start: u16 = 65500;
    let count: u32 = 100; // crosses from 65500 through 0 up to ~64

    for i in 0..count {
        let seq = start.wrapping_add(i as u16);
        let ts = i.wrapping_mul(160);
        let rtp = crate::rtp::RtpHeader::build(0, seq, ts, 0x12345678, false, &[seq as u8; 160]);
        let srtp = protect_ctx.protect(&rtp).unwrap();
        let decrypted = unprotect_ctx
            .unprotect(&srtp)
            .unwrap_or_else(|e| panic!("unprotect failed for seq {} (i={}): {}", seq, i, e));
        assert_eq!(decrypted, rtp, "roundtrip failed for seq {} (i={})", seq, i);
    }
}

#[test]
fn test_srtp_out_of_order_within_window() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    // Protect packets 0..=4 in order
    let mut srtp_packets = Vec::new();
    for seq in 0u16..=4 {
        let rtp =
            crate::rtp::RtpHeader::build(0, seq, seq as u32 * 160, 0xABCD, false, &[seq as u8; 80]);
        let srtp = protect_ctx.protect(&rtp).unwrap();
        srtp_packets.push((seq, rtp, srtp));
    }

    // Unprotect in out-of-order sequence: 0, 2, 4, 1, 3
    let unprotect_order = [0usize, 2, 4, 1, 3];
    for &idx in &unprotect_order {
        let (seq, ref original_rtp, ref srtp) = srtp_packets[idx];
        let decrypted = unprotect_ctx
            .unprotect(srtp)
            .unwrap_or_else(|e| panic!("unprotect failed for seq {} (out-of-order): {}", seq, e));
        assert_eq!(
            &decrypted, original_rtp,
            "roundtrip failed for seq {} (out-of-order)",
            seq
        );
    }
}

#[test]
fn test_srtcp_different_keys_from_srtp() {
    let key = make_test_key();
    let srtp_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let srtcp_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    // SRTCP uses labels 0x03/0x04/0x05 vs SRTP 0x00/0x01/0x02
    // so derived keys must differ
    assert_ne!(srtp_ctx.cipher_key, srtcp_ctx.cipher_key);
    assert_ne!(srtp_ctx.auth_key, srtcp_ctx.auth_key);
    assert_ne!(srtp_ctx.cipher_salt, srtcp_ctx.cipher_salt);
}

#[test]
fn test_srtcp_multiple_packets() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    for i in 0..5 {
        let mut stats = crate::rtcp::RtcpStats::new();
        stats.packets_sent = i * 10;
        let rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
        let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
        assert_eq!(decrypted, rtcp, "SRTCP roundtrip failed for packet {}", i);
    }
}

#[test]
fn test_srtcp_replay_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    let mut stats = crate::rtcp::RtcpStats::new();
    let rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
    let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();

    // First unprotect should succeed
    unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();

    // Replaying the same packet should fail
    let result = unprotect_ctx.unprotect_rtcp(&srtcp);
    assert!(result.is_err(), "SRTCP replay should be rejected");
    assert!(result.unwrap_err().to_string().contains("replay"));
}

#[test]
fn test_srtcp_old_packet_outside_window_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    let mut stats = crate::rtcp::RtcpStats::new();

    // Protect 100 RTCP packets
    let mut srtcp_packets = Vec::new();
    for _ in 0..100 {
        stats.packets_sent += 10;
        let rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
        srtcp_packets.push(protect_ctx.protect_rtcp(&rtcp).unwrap());
    }

    // Unprotect all in order
    for srtcp in &srtcp_packets {
        unprotect_ctx.unprotect_rtcp(srtcp).unwrap();
    }

    // Replaying packet 0 should fail (outside 64-packet window)
    let result = unprotect_ctx.unprotect_rtcp(&srtcp_packets[0]);
    assert!(
        result.is_err(),
        "old SRTCP packet outside window should be rejected"
    );
}

#[test]
fn test_srtcp_index_increments() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    let mut stats = crate::rtcp::RtcpStats::new();
    let rtcp = crate::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

    // Protect 3 RTCP packets and extract the E+index field from each
    let mut indices = Vec::new();
    for _ in 0..3 {
        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
        // E+index is 4 bytes just before the 10-byte auth tag
        let idx_offset = srtcp.len() - SRTCP_AUTH_TAG_LEN - 4;
        let e_and_index = u32::from_be_bytes([
            srtcp[idx_offset],
            srtcp[idx_offset + 1],
            srtcp[idx_offset + 2],
            srtcp[idx_offset + 3],
        ]);
        indices.push(e_and_index);
    }

    // E-flag should be set (0x80000000) and indices should be 0, 1, 2
    assert_eq!(
        indices[0],
        0x80000000 | 0,
        "first packet should have index 0"
    );
    assert_eq!(
        indices[1],
        0x80000000 | 1,
        "second packet should have index 1"
    );
    assert_eq!(
        indices[2],
        0x80000000 | 2,
        "third packet should have index 2"
    );
}

#[test]
fn test_srtp_different_keys_fail() {
    // Create two SrtpContexts with DIFFERENT keys
    let key_material_a = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
    ];
    let key_material_b = [
        0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
        0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB, 0xBC, 0xBD, 0xBE,
    ];
    let key_a = base64_encode(&key_material_a);
    let key_b = base64_encode(&key_material_b);

    let mut ctx_a = SrtpContext::from_sdes_key(SUITE, &key_a).unwrap();
    let mut ctx_b = SrtpContext::from_sdes_key(SUITE, &key_b).unwrap();

    // Protect a packet with context A
    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);
    let srtp = ctx_a.protect(&rtp).unwrap();

    // Try to unprotect with context B — should fail with auth tag mismatch
    let result = ctx_b.unprotect(&srtp);
    assert!(result.is_err(), "unprotect with different key should fail");
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("auth tag mismatch"),
        "error should mention auth tag mismatch: {err_msg}"
    );
}

#[test]
fn test_srtp_short_key_material() {
    // Base64 of 20 bytes (less than the required 30)
    let short_material = [0x01u8; 20];
    let short_key = base64_encode(&short_material);

    let result = SrtpContext::from_sdes_key(SUITE, &short_key);
    assert!(result.is_err(), "short key material should fail");
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("too short"),
        "error should mention key material too short: {err_msg}"
    );
}

#[test]
fn test_srtp_exact_30_byte_key() {
    // Exactly 30 bytes of key material (16 master key + 14 master salt)
    let key_material: [u8; 30] = [
        0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
        0x0C, // 16-byte master key
        0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0,
        0xE0, // 14-byte master salt
    ];
    let key_b64 = base64_encode(&key_material);

    // Creating the context should succeed (no panic, no error)
    let ctx = SrtpContext::from_sdes_key(SUITE, &key_b64);
    assert!(
        ctx.is_ok(),
        "exact 30-byte key material should succeed: {:?}",
        ctx.err()
    );

    let ctx = ctx.unwrap();
    // Derived keys should be non-zero (KDF actually ran)
    assert_ne!(
        ctx.cipher_key,
        vec![0u8; 16],
        "cipher key should be derived (non-zero)"
    );
    assert_ne!(
        ctx.cipher_salt, [0u8; 14],
        "cipher salt should be derived (non-zero)"
    );
    assert_ne!(
        ctx.auth_key, [0u8; 20],
        "auth key should be derived (non-zero)"
    );

    // Verify the context can actually protect and unprotect a packet
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key_b64).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key_b64).unwrap();

    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0xCAFEBABE, false, &[0x55; 160]);
    let srtp = protect_ctx.protect(&rtp).unwrap();
    let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
    assert_eq!(
        decrypted, rtp,
        "roundtrip with exact 30-byte key should succeed"
    );
}

#[test]
fn test_srtp_key_29_bytes_rejected() {
    // Exactly 1 byte short of the required 30
    let key_material = [0xAA; 29];
    let key_b64 = base64_encode(&key_material);
    let result = SrtpContext::from_sdes_key(SUITE, &key_b64);
    assert!(result.is_err(), "29-byte key material should be rejected");
    let err = result.err().unwrap();
    assert!(err.to_string().contains("too short"));
}

#[test]
fn test_srtp_key_31_bytes_rejected() {
    let key_b64 = base64_encode(&[0xBB; 31]);
    assert!(SrtpContext::from_sdes_key(SUITE, &key_b64).is_err());
    assert!(SrtcpContext::from_sdes_key(SUITE, &key_b64).is_err());
}

#[test]
fn test_srtp_replay_exact_duplicate() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    let rtp = crate::rtp::RtpHeader::build(0, 42, 6720, 0xABCD, false, &[0x55; 160]);
    let srtp = protect_ctx.protect(&rtp).unwrap();

    // First unprotect should succeed
    unprotect_ctx.unprotect(&srtp).unwrap();

    // Exact duplicate should be rejected (replay)
    let result = unprotect_ctx.unprotect(&srtp);
    assert!(result.is_err(), "exact duplicate packet should be rejected");
    assert!(
        result.unwrap_err().to_string().contains("replay"),
        "error should mention replay"
    );
}

#[test]
fn test_srtp_replay_outside_window() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    // Protect packets 0..100
    let mut srtp_packets = Vec::new();
    for seq in 0u16..100 {
        let rtp =
            crate::rtp::RtpHeader::build(0, seq, seq as u32 * 160, 0xABCD, false, &[seq as u8; 80]);
        srtp_packets.push(protect_ctx.protect(&rtp).unwrap());
    }

    // Unprotect packets 0..100 in order
    for srtp in &srtp_packets {
        unprotect_ctx.unprotect(srtp).unwrap();
    }

    // Now try to replay packet 0 — should be outside the 64-packet replay window
    let result = unprotect_ctx.unprotect(&srtp_packets[0]);
    assert!(
        result.is_err(),
        "packet far outside replay window should be rejected"
    );
}

#[test]
fn test_srtp_key_malformed_base64_variants() {
    // Embedded null byte
    let result = SrtpContext::from_sdes_key(SUITE, "AQID\x00BA==");
    assert!(result.is_err(), "base64 with null byte should be rejected");

    // Whitespace in middle
    let result = SrtpContext::from_sdes_key(SUITE, "AQ ID BA==");
    assert!(result.is_err(), "base64 with space should be rejected");

    // Unicode
    let result = SrtpContext::from_sdes_key(SUITE, "AQIDé==");
    assert!(result.is_err(), "base64 with unicode should be rejected");

    // Empty string → too-short key material
    let result = SrtpContext::from_sdes_key(SUITE, "");
    assert!(result.is_err(), "empty string should be rejected");
}

// ── IV computation tests ─────────────────────────────────────────

#[test]
fn test_compute_iv_rfc3711_layout() {
    // Verify RFC 3711 §4.1.1 layout:
    // IV = (k_s * 2^16) XOR (SSRC * 2^64) XOR (i * 2^16)
    //
    // With all-zero salt, SSRC=0, index=0, IV should be all zeros.
    let salt = [0u8; 14];
    let iv = compute_iv(&salt, 0, 0);
    assert_eq!(iv, [0u8; 16], "all-zero inputs should produce all-zero IV");

    // With non-zero salt, zero SSRC and index:
    // IV should equal salt at bytes 0-13, bytes 14-15 = 0
    let salt = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
    ];
    let iv = compute_iv(&salt, 0, 0);
    assert_eq!(
        &iv[0..14],
        &salt,
        "salt-only IV should have salt at bytes 0-13"
    );
    assert_eq!(iv[14], 0, "byte 14 must be zero");
    assert_eq!(iv[15], 0, "byte 15 must be zero");
}

#[test]
fn test_compute_iv_ssrc_position() {
    // SSRC * 2^64 places SSRC at bytes 4-7
    let salt = [0u8; 14];
    let iv = compute_iv(&salt, 0xDEADBEEF, 0);
    assert_eq!(iv[4], 0xDE);
    assert_eq!(iv[5], 0xAD);
    assert_eq!(iv[6], 0xBE);
    assert_eq!(iv[7], 0xEF);
    // Bytes 0-3 and 8-15 should be zero (no salt, no index)
    assert_eq!(&iv[0..4], &[0, 0, 0, 0]);
    assert_eq!(&iv[8..16], &[0, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn test_compute_iv_index_shifted_by_16() {
    // i * 2^16 places the 48-bit index at bytes 8-13 (shifted left by 16)
    let salt = [0u8; 14];
    // Index = ROC=1, SEQ=0 → raw 48-bit index = (1 << 16) | 0 = 0x10000
    let index: u64 = 0x10000;
    let iv = compute_iv(&salt, 0, index);
    // index << 16 = 0x1_0000_0000 → bytes: [0,0,0,1,0,0,0,0]
    // placed at iv[8..16]
    assert_eq!(
        &iv[8..16],
        &[0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]
    );
    // Bytes 14-15 must be zero
    assert_eq!(iv[14], 0);
    assert_eq!(iv[15], 0);
}

#[test]
fn test_compute_iv_full_combination() {
    // Combine salt, SSRC, and index and verify XOR
    let salt: [u8; 14] = [
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ];
    let ssrc: u32 = 0x12345678;
    // 48-bit index: ROC=0, SEQ=1 → index = 1
    let index: u64 = 1;

    let iv = compute_iv(&salt, ssrc, index);

    // Expected (manually computed):
    // index=1, shifted: 1u64 << 16 = 0x0000_0000_0001_0000
    //   to_be_bytes() = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]
    //   placed at iv[8..16]
    //
    // Before salt XOR:
    //   iv = [0,0,0,0, 0x12,0x34,0x56,0x78, 0,0,0,0, 0,0x01,0,0]
    // After XOR with salt (all 0xFF at bytes 0-13):
    //   iv[0..4]   = 0xFF
    //   iv[4..8]   = SSRC XOR salt = [0xED, 0xCB, 0xA9, 0x87]
    //   iv[8..14]  = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
    //   iv[14..16] = [0, 0] (no salt contribution)
    assert_eq!(iv[0], 0xFF);
    assert_eq!(iv[1], 0xFF);
    assert_eq!(iv[2], 0xFF);
    assert_eq!(iv[3], 0xFF);
    assert_eq!(iv[4], 0xFF ^ 0x12);
    assert_eq!(iv[5], 0xFF ^ 0x34);
    assert_eq!(iv[6], 0xFF ^ 0x56);
    assert_eq!(iv[7], 0xFF ^ 0x78);
    // shifted index [0,0,0,0,0,0x01,0,0] at iv[8..16], XOR with salt[8..14]
    assert_eq!(iv[8], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[9], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[10], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[11], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[12], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[13], 0xFE); // 0x01 ^ 0xFF
    assert_eq!(iv[14], 0x00); // no salt at byte 14
    assert_eq!(iv[15], 0x00);
}

#[test]
fn test_compute_srtcp_iv_layout() {
    // Same layout rules as SRTP IV but with 32-bit SRTCP index
    let salt = [0u8; 14];

    // All zeros
    let iv = compute_srtcp_iv(&salt, 0, 0);
    assert_eq!(iv, [0u8; 16]);

    // Salt only
    let salt = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
    ];
    let iv = compute_srtcp_iv(&salt, 0, 0);
    assert_eq!(&iv[0..14], &salt);
    assert_eq!(iv[14], 0);
    assert_eq!(iv[15], 0);

    // SSRC only
    let salt = [0u8; 14];
    let iv = compute_srtcp_iv(&salt, 0xAABBCCDD, 0);
    assert_eq!(&iv[4..8], &[0xAA, 0xBB, 0xCC, 0xDD]);

    // Index shifted by 16: SRTCP index 1 → (1u64 << 16) = 0x10000
    let iv = compute_srtcp_iv(&salt, 0, 1);
    // [0,0,0,0,0,0x01,0x00,0x00] at iv[8..16]
    assert_eq!(&iv[8..14], &[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
    assert_eq!(iv[14], 0);
    assert_eq!(iv[15], 0);
}

#[test]
fn test_srtcp_protect_unprotect_roundtrip() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();

    // Build a minimal RTCP Sender Report by hand:
    // 4-byte header + 4-byte SSRC + 20-byte SR body = 28 bytes
    // V=2, P=0, RC=0 → first byte = 0x80
    // PT=200 (SR)
    // Length=6 (28 bytes / 4 - 1 = 6)
    let mut rtcp = [0u8; 28];
    rtcp[0] = 0x80; // V=2, P=0, RC=0
    rtcp[1] = 200; // PT = Sender Report
    rtcp[2] = 0x00; // length high byte
    rtcp[3] = 0x06; // length low byte (6 32-bit words after header)
    // SSRC
    rtcp[4] = 0x12;
    rtcp[5] = 0x34;
    rtcp[6] = 0x56;
    rtcp[7] = 0x78;
    // Fill SR body (NTP timestamp, RTP timestamp, packet/octet counts)
    // with recognizable non-zero data so we can verify roundtrip
    for i in 8..28 {
        rtcp[i] = i as u8;
    }
    let original = rtcp.to_vec();

    // Protect (encrypt + authenticate)
    let srtcp = protect_ctx.protect_rtcp(&original).unwrap();
    // SRTCP adds 4-byte E+index field and 10-byte auth tag
    assert_eq!(srtcp.len(), original.len() + 4 + SRTCP_AUTH_TAG_LEN);

    // Unprotect (verify + decrypt) with a separate context from the same key
    let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
    assert_eq!(
        decrypted, original,
        "SRTCP roundtrip with hand-built SR should recover original packet"
    );
}

#[test]
fn test_srtp_new_ssrc_accepted_without_reset() {
    // Per-SSRC state: after one source runs to a high sequence, a DIFFERENT
    // SSRC starting at a low sequence is accepted with no reset call — the
    // bug this fixes (the shared window previously rejected it as "too old").
    let key = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    for seq in 1..=300u16 {
        let rtp = crate::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xAAAA_0001,
            false,
            &[seq as u8; 80],
        );
        let srtp = tx.protect(&rtp).unwrap();
        rx.unprotect(&srtp).unwrap();
    }

    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0xBBBB_0002, false, &[0x99; 80]);
    let srtp = tx.protect(&rtp).unwrap();
    let decrypted = rx
        .unprotect(&srtp)
        .expect("new SSRC at a low sequence must be accepted without a reset");
    assert_eq!(decrypted, rtp);
}

#[test]
fn test_srtp_same_seq_distinct_ssrc_independent_replay() {
    // The same sequence number on two SSRCs is not a cross-SSRC replay; each
    // SSRC has an independent window. Replaying within one SSRC still fails.
    let key = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    let a = crate::rtp::RtpHeader::build(0, 7, 1120, 0xA000_0000, false, &[1u8; 80]);
    let b = crate::rtp::RtpHeader::build(0, 7, 1120, 0xB000_0000, false, &[2u8; 80]);
    let sa = tx.protect(&a).unwrap();
    let sb = tx.protect(&b).unwrap();

    assert_eq!(rx.unprotect(&sa).unwrap(), a);
    assert_eq!(
        rx.unprotect(&sb).unwrap(),
        b,
        "same seq, other SSRC is not a replay"
    );
    assert!(
        rx.unprotect(&sa)
            .unwrap_err()
            .to_string()
            .contains("replay"),
        "duplicate on the same SSRC must still be a replay"
    );
}

#[test]
fn test_srtp_independent_roc_per_ssrc() {
    // Two SSRCs cross the 16-bit sequence wrap independently; their ROCs must
    // advance separately. Interleaved so neither corrupts the other's ROC.
    let key = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    let ssrc_a = 0xA0A0_A0A0; // starts near the top → wraps mid-loop
    let ssrc_b = 0xB0B0_B0B0; // starts low → no wrap
    for i in 0..100u32 {
        let seq_a = 65500u16.wrapping_add(i as u16);
        let seq_b = 1000u16.wrapping_add(i as u16);
        let ra = crate::rtp::RtpHeader::build(
            0,
            seq_a,
            i.wrapping_mul(160),
            ssrc_a,
            false,
            &[i as u8; 80],
        );
        let rb = crate::rtp::RtpHeader::build(
            0,
            seq_b,
            i.wrapping_mul(160),
            ssrc_b,
            false,
            &[(i as u8) ^ 0xFF; 80],
        );
        let pa = tx.protect(&ra).unwrap();
        let pb = tx.protect(&rb).unwrap();
        assert_eq!(rx.unprotect(&pa).unwrap(), ra, "A failed at i={i}");
        assert_eq!(rx.unprotect(&pb).unwrap(), rb, "B failed at i={i}");
    }
}

#[test]
fn test_srtp_recv_ssrc_cap_rejects_excess() {
    // The receive context bounds distinct SSRCs to MAX_RECV_SSRCS; one more
    // authenticated SSRC is rejected rather than evicting a live stream.
    let key = make_test_key();
    let mut rx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();

    for i in 0..MAX_RECV_SSRCS as u32 {
        let mut tx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
        let rtp = crate::rtp::RtpHeader::build(0, 0, 0, 0x1000_0000 + i, false, &[i as u8; 80]);
        let srtp = tx.protect(&rtp).unwrap();
        rx.unprotect(&srtp).unwrap();
    }

    let mut tx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let rtp = crate::rtp::RtpHeader::build(0, 0, 0, 0x2000_0000, false, &[0u8; 80]);
    let srtp = tx.protect(&rtp).unwrap();
    let err = rx.unprotect(&srtp).unwrap_err().to_string();
    assert!(err.contains("too many"), "expected cap error, got: {err}");

    // An already-tracked SSRC keeps working (not locked out by the cap).
    let mut tx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 0x1000_0000, false, &[7u8; 80]);
    let srtp = tx.protect(&rtp).unwrap();
    rx.unprotect(&srtp)
        .expect("an already-tracked SSRC must keep working at the cap");
}

#[test]
fn test_srtcp_new_ssrc_accepted_without_reset() {
    // SRTCP analog: after one source runs its index past the window, a
    // different source at index 0 is accepted without a reset.
    let key = make_test_key();
    let mut rx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut stats = crate::rtcp::RtcpStats::new();

    let mut px = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let rtcp_x = crate::rtcp::build_sr_rr(0x1111_1111, 0x2222_2222, &mut stats, 0, 8000);
    for _ in 0..200 {
        let s = px.protect_rtcp(&rtcp_x).unwrap();
        rx.unprotect_rtcp(&s).unwrap();
    }

    let mut py = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let rtcp_y = crate::rtcp::build_sr_rr(0x3333_3333, 0x4444_4444, &mut stats, 0, 8000);
    let sy = py.protect_rtcp(&rtcp_y).unwrap();
    rx.unprotect_rtcp(&sy)
        .expect("new SRTCP SSRC at index 0 must be accepted without a reset");
}

#[test]
fn test_srtcp_distinct_ssrc_independent_replay() {
    // Same SRTCP index on two SSRCs is independent; replay within one fails.
    let key = make_test_key();
    let mut rx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut pa = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut pb = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let mut stats = crate::rtcp::RtcpStats::new();
    let rtcp_a = crate::rtcp::build_sr_rr(0xAAAA_0000, 0x1, &mut stats, 0, 8000);
    let rtcp_b = crate::rtcp::build_sr_rr(0xBBBB_0000, 0x1, &mut stats, 0, 8000);
    let sa = pa.protect_rtcp(&rtcp_a).unwrap();
    let sb = pb.protect_rtcp(&rtcp_b).unwrap();

    rx.unprotect_rtcp(&sa).unwrap();
    rx.unprotect_rtcp(&sb)
        .expect("same index on a different SSRC is independent, not a replay");
    assert!(
        rx.unprotect_rtcp(&sa)
            .unwrap_err()
            .to_string()
            .contains("replay"),
        "SRTCP duplicate on the same SSRC must still be rejected"
    );
}

// ── Suite metadata ───────────────────────────────────────────────

#[test]
fn suite_names_and_sizes() {
    for (name, suite, klen, tag) in [
        (
            "AES_CM_128_HMAC_SHA1_80",
            SrtpSuite::AesCm128HmacSha1_80,
            16,
            10,
        ),
        (
            "AES_CM_128_HMAC_SHA1_32",
            SrtpSuite::AesCm128HmacSha1_32,
            16,
            4,
        ),
        (
            "AES_256_CM_HMAC_SHA1_80",
            SrtpSuite::Aes256CmHmacSha1_80,
            32,
            10,
        ),
        (
            "AES_256_CM_HMAC_SHA1_32",
            SrtpSuite::Aes256CmHmacSha1_32,
            32,
            4,
        ),
    ] {
        assert_eq!(SrtpSuite::from_sdp_name(name), Some(suite));
        assert_eq!(suite.sdp_name(), name);
        assert_eq!(suite.master_key_len(), klen);
        assert_eq!(suite.master_salt_len(), 14);
        assert_eq!(suite.rtp_auth_tag_len(), tag);
        assert_eq!(suite.srtcp_auth_tag_len(), 10);
    }
    // FreeSWITCH-style aliases
    assert_eq!(
        SrtpSuite::from_sdp_name("AES_CM_256_HMAC_SHA1_80"),
        Some(SrtpSuite::Aes256CmHmacSha1_80)
    );
    assert_eq!(
        SrtpSuite::from_sdp_name("AES_CM_256_HMAC_SHA1_32"),
        Some(SrtpSuite::Aes256CmHmacSha1_32)
    );
    assert_eq!(SrtpSuite::from_sdp_name("AEAD_AES_128_GCM"), None);
}

// ── RFC test vectors ─────────────────────────────────────────────

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// RFC 3711 Appendix B.2: AES-CM keystream (first blocks and the last ones of
/// the 1044512-octet segment).
#[test]
fn rfc3711_b2_aes_cm_keystream() {
    let key = hex("2B7E151628AED2A6ABF7158809CF4F3C");
    let iv: [u8; 16] = hex("F0F1F2F3F4F5F6F7F8F9FAFBFCFD0000").try_into().unwrap();
    let mut ks = vec![0u8; 1044512];
    aes_ctr_apply(&key, &iv, &mut ks);
    assert_eq!(&ks[0..16], &hex("E03EAD0935C95E80E166B16DD92B4EB4")[..]);
    assert_eq!(&ks[16..32], &hex("D23513162B02D0F72A43A2FE4A5F97AB")[..]);
    assert_eq!(&ks[32..48], &hex("41E95B3BB0A2E8DD477901E4FCA894C0")[..]);
    let n = ks.len();
    assert_eq!(
        &ks[n - 48..n - 32],
        &hex("EC8CDF7398607CB0F2D21675EA9EA1E4")[..]
    );
    assert_eq!(
        &ks[n - 32..n - 16],
        &hex("362B7C3C6773516318A077D7FC5073AE")[..]
    );
    assert_eq!(&ks[n - 16..], &hex("6A2CC3787889374FBEB4C81B17BA6C44")[..]);
}

/// RFC 3711 Appendix B.3: AES-128 key derivation.
#[test]
fn rfc3711_b3_key_derivation() {
    let mk = hex("E1F97A0D3E018BE0D64FA32C06DE4139");
    let ms: [u8; 14] = hex("0EC675AD498AFEEBB6960B3AABE6").try_into().unwrap();
    assert_eq!(
        srtp_kdf(&mk, &ms, 0x00, 16),
        hex("C61E7A93744F39EE10734AFE3FF7A087")
    );
    assert_eq!(
        srtp_kdf(&mk, &ms, 0x02, 14),
        hex("30CBBC08863D8C85D49DB34A9AE1")
    );
    let auth = srtp_kdf(&mk, &ms, 0x01, 94);
    assert_eq!(
        auth,
        hex(
            "CEBE321F6FF7716B6FD4AB49AF256A15 6D38BAA48F0A0ACF3C34E2359E6CDBCE
             E049646C43D9327AD175578EF7227098 6371C10C9A369AC2F94A8C5FBCDDDC25
             6D6E919A48B610EF17C2041E47403576 6B68642C59BBFC2F34DB60DBDFB2"
        )
    );
}

/// The same B.3 inputs through the public constructor.
#[test]
fn rfc3711_b3_via_context() {
    let mut km = hex("E1F97A0D3E018BE0D64FA32C06DE4139");
    km.extend(hex("0EC675AD498AFEEBB6960B3AABE6"));
    let ctx = SrtpContext::from_sdes_key(SUITE, &base64_encode(&km)).unwrap();
    assert_eq!(ctx.cipher_key, hex("C61E7A93744F39EE10734AFE3FF7A087"));
    assert_eq!(
        &ctx.cipher_salt[..],
        &hex("30CBBC08863D8C85D49DB34A9AE1")[..]
    );
    assert_eq!(
        &ctx.auth_key[..],
        &hex("CEBE321F6FF7716B6FD4AB49AF256A15 6D38BAA4")[..]
    );
}

/// RFC 6188 section 7.1: AES-256-CM keystream.
#[test]
fn rfc6188_7_1_aes256_cm_keystream() {
    let key = hex("57f82fe3613fd170a85ec93c40b1f092 2ec4cb0dc025b58272147cc438944a98");
    let iv: [u8; 16] = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfd0000").try_into().unwrap();
    let mut ks = vec![0u8; 1044512];
    aes_ctr_apply(&key, &iv, &mut ks);
    assert_eq!(&ks[0..16], &hex("92bdd28a93c3f52511c677d08b5515a4")[..]);
    assert_eq!(&ks[16..32], &hex("9da71b2378a854f67050756ded165bac")[..]);
    assert_eq!(&ks[32..48], &hex("63c4868b7096d88421b563b8c94c9a31")[..]);
    let n = ks.len();
    assert_eq!(
        &ks[n - 48..n - 32],
        &hex("cea518c90fd91ced9cbb18c078a54711")[..]
    );
    assert_eq!(
        &ks[n - 32..n - 16],
        &hex("3dbc4814f4da5f00a08772b63c6a046d")[..]
    );
    assert_eq!(&ks[n - 16..], &hex("6eb246913062a16891433e97dd01a57f")[..]);
}

/// RFC 6188 section 7.2: AES_256_CM_PRF key derivation.
#[test]
fn rfc6188_7_2_aes256_key_derivation() {
    let mk = hex("f0f04914b513f2763a1b1fa130f10e29 98f6f6e43e4309d1e622a0e332b9f1b6");
    let ms: [u8; 14] = hex("3b04803de51ee7c96423ab5b78d2").try_into().unwrap();
    assert_eq!(
        srtp_kdf(&mk, &ms, 0x00, 32),
        hex("5ba1064e30ec51613cad926c5a28ef73 1ec7fb397f70a960653caf06554cd8c4")
    );
    assert_eq!(
        srtp_kdf(&mk, &ms, 0x02, 14),
        hex("fa31791685ca444a9e07c6c64e93")
    );
    assert_eq!(
        srtp_kdf(&mk, &ms, 0x01, 20),
        hex("fd9c32d39ed5fbb5a9dc96b30818454d 1313dc05")
    );

    // And through the public constructor for both AES-256 suites.
    let mut km = mk.clone();
    km.extend_from_slice(&ms);
    for suite in [
        SrtpSuite::Aes256CmHmacSha1_80,
        SrtpSuite::Aes256CmHmacSha1_32,
    ] {
        let ctx = SrtpContext::from_sdes_key(suite, &base64_encode(&km)).unwrap();
        assert_eq!(
            ctx.cipher_key,
            hex("5ba1064e30ec51613cad926c5a28ef73 1ec7fb397f70a960653caf06554cd8c4")
        );
        assert_eq!(
            &ctx.cipher_salt[..],
            &hex("fa31791685ca444a9e07c6c64e93")[..]
        );
        assert_eq!(
            &ctx.auth_key[..],
            &hex("fd9c32d39ed5fbb5a9dc96b30818454d 1313dc05")[..]
        );
    }
}

// ── All-suite behavior ──────────────────────────────────────────

#[test]
fn all_suites_roundtrip_and_tag_lengths() {
    for suite in SrtpSuite::ALL {
        let km = SrtpKeyMaterial::generate(suite);
        let key = km.to_base64();
        let mut tx = SrtpContext::from_sdes_key(suite, &key).unwrap();
        let mut rx = SrtpContext::from_sdes_key(suite, &key).unwrap();
        for seq in 0..5u16 {
            let rtp =
                crate::rtp::RtpHeader::build(0, seq, seq as u32 * 160, 0xFEED, false, &[7; 160]);
            let srtp = tx.protect(&rtp).unwrap();
            assert_eq!(
                srtp.len(),
                rtp.len() + suite.rtp_auth_tag_len(),
                "{suite:?}"
            );
            assert_ne!(&srtp[12..172], &rtp[12..172]);
            assert_eq!(rx.unprotect(&srtp).unwrap(), rtp);
            // replay rejected
            assert!(rx.unprotect(&srtp).is_err());
        }

        let mut tx = SrtcpContext::from_sdes_key(suite, &key).unwrap();
        let mut rx = SrtcpContext::from_sdes_key(suite, &key).unwrap();
        let mut stats = crate::rtcp::RtcpStats::new();
        let rtcp = crate::rtcp::build_sr_rr(0x1234, 0x5678, &mut stats, 0, 8000);
        let s = tx.protect_rtcp(&rtcp).unwrap();
        // SRTCP always carries the 80-bit tag
        assert_eq!(s.len(), rtcp.len() + 4 + 10, "{suite:?}");
        assert_eq!(rx.unprotect_rtcp(&s).unwrap(), rtcp);
        assert!(rx.unprotect_rtcp(&s).is_err());
    }
}

#[test]
fn mismatched_suites_fail() {
    let km = SrtpKeyMaterial::generate(SrtpSuite::AesCm128HmacSha1_80);
    // A 30-byte key is too short for AES-256
    let err = SrtpContext::from_sdes_key(SrtpSuite::Aes256CmHmacSha1_80, &km.to_base64())
        .err()
        .unwrap();
    assert!(err.to_string().contains("too short"), "{err}");
    // Same key, 80 vs 32 tag: unprotect fails on auth
    let key = km.to_base64();
    let mut tx = SrtpContext::from_sdes_key(SrtpSuite::AesCm128HmacSha1_80, &key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(SrtpSuite::AesCm128HmacSha1_32, &key).unwrap();
    let rtp = crate::rtp::RtpHeader::build(0, 1, 160, 1, false, &[1; 80]);
    assert!(rx.unprotect(&tx.protect(&rtp).unwrap()).is_err());
}

#[test]
fn generate_produces_correct_sizes_and_is_random() {
    for suite in SrtpSuite::ALL {
        let a = SrtpKeyMaterial::generate(suite);
        let b = SrtpKeyMaterial::generate(suite);
        assert_eq!(a.master_key().len(), suite.master_key_len());
        assert_eq!(a.master_salt().len(), 14);
        assert_ne!(a.to_base64(), b.to_base64());
        assert!(a.to_inline().starts_with("inline:"));
        // Contexts can be built straight from the inline string.
        SrtpContext::from_inline(suite, &a.to_inline()).unwrap();
        SrtcpContext::from_inline(suite, &a.to_inline()).unwrap();
    }
}

// ── SDES inline parsing ──────────────────────────────────────────

#[test]
fn inline_parsing_lifetime_forms() {
    let k = make_test_key();
    let p = parse_sdes_inline(&format!("inline:{k}")).unwrap();
    assert_eq!(
        p,
        SdesInline {
            key_b64: k.clone(),
            lifetime: None
        }
    );
    let p = parse_sdes_inline(&format!("inline:{k}|2^31")).unwrap();
    assert_eq!(p.lifetime, Some(1 << 31));
    let p = parse_sdes_inline(&format!("{k}|1000000")).unwrap();
    assert_eq!(p.key_b64, k);
    assert_eq!(p.lifetime, Some(1_000_000));
    assert!(parse_sdes_inline(&format!("inline:{k}|2^abc")).is_err());
    assert!(parse_sdes_inline(&format!("inline:{k}|2^64")).is_err());
}

#[test]
fn inline_mki_is_rejected_with_clear_error() {
    let k = make_test_key();
    for s in [format!("inline:{k}|2^31|1:1"), format!("inline:{k}|1:4")] {
        let err = parse_sdes_inline(&s).unwrap_err().to_string();
        assert!(
            err.contains("MKI") && err.contains("not supported"),
            "{err}"
        );
        assert!(SrtpContext::from_inline(SUITE, &s).is_err());
    }
}

#[test]
fn inline_lifetime_is_honored() {
    let k = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(SUITE, &k).unwrap();
    let mut limited = SrtpContext::from_inline(SUITE, &format!("inline:{k}|2")).unwrap();
    assert_eq!(limited.packet_lifetime(), 2);
    for seq in 1..=2u16 {
        let rtp = crate::rtp::RtpHeader::build(0, seq, 0, 9, false, &[0; 40]);
        limited.unprotect(&tx.protect(&rtp).unwrap()).unwrap();
    }
    let rtp = crate::rtp::RtpHeader::build(0, 3, 0, 9, false, &[0; 40]);
    let e = limited.unprotect(&tx.protect(&rtp).unwrap()).unwrap_err();
    assert!(e.to_string().contains("lifetime exhausted"));

    // 2^31 parses as 2147483648 packets
    let c = SrtpContext::from_inline(SUITE, &format!("inline:{k}|2^31")).unwrap();
    assert_eq!(c.packet_lifetime(), 1 << 31);
    // Over RFC 3711's 2^48 is refused
    assert!(SrtpContext::from_inline(SUITE, &format!("inline:{k}|2^49")).is_err());
}

#[test]
fn base64_without_padding_is_accepted() {
    // 46 bytes -> 64 chars with 2 '=' padding; strip it.
    let km = SrtpKeyMaterial::generate(SrtpSuite::Aes256CmHmacSha1_80);
    let b = km.to_base64();
    let stripped = b.trim_end_matches('=');
    SrtpContext::from_sdes_key(SrtpSuite::Aes256CmHmacSha1_80, stripped).unwrap();
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn test_debug_never_prints_key_material() {
    let key = make_test_key();
    let inline = parse_sdes_inline(&format!("inline:{key}|2^31")).unwrap();
    let dbg = format!("{inline:?}");
    assert!(!dbg.contains(&key), "{dbg}");
    assert!(dbg.contains("redacted") && dbg.contains("lifetime"));

    let ctx = SrtpContext::from_sdes_key(SUITE, &key).unwrap();
    let rctx = SrtcpContext::from_sdes_key(SUITE, &key).unwrap();
    let km = SrtpKeyMaterial::generate(SUITE);
    for d in [format!("{ctx:?}"), format!("{rctx:?}"), format!("{km:?}")] {
        assert!(d.contains("redacted"), "{d}");
        assert!(!d.contains(&km.to_base64()) && !d.contains(&key), "{d}");
    }
}

/// Packet-level known answer: key 0x01..=0x1e, SSRC 0xcafebabe, ROC 1.
#[test]
fn test_srtp_packet_known_answer_roc1() {
    let mut ctx = SrtpContext::from_sdes_key(SUITE, &make_test_key()).unwrap();
    let ssrc = 0xcafe_babe;
    // Drive the sender into ROC 1: highest seq 0xffff, then wrap to seq 1.
    ctx.protect(&crate::rtp::RtpHeader::build(
        0,
        0xffff,
        0x0572,
        ssrc,
        false,
        &[0x22; 20],
    ))
    .unwrap();
    let rtp = crate::rtp::RtpHeader::build(0, 0x0001, 0x0572, ssrc, false, &[0x22; 20]);
    let out = ctx.protect(&rtp).unwrap();
    assert_eq!(
        to_hex(&out),
        "8000000100000572cafebabe8bc2ba1ed1bbb52c3796bc74366e8b805d6322202f94beb15f593ce5dc48"
    );

    // And a receiver at ROC 0 that has seen seq 0xffff recovers ROC 1.
    let mut rx = SrtpContext::from_sdes_key(SUITE, &make_test_key()).unwrap();
    let first = SrtpContext::from_sdes_key(SUITE, &make_test_key())
        .unwrap()
        .protect(&crate::rtp::RtpHeader::build(
            0,
            0xffff,
            0x0572,
            ssrc,
            false,
            &[0x22; 20],
        ))
        .unwrap();
    rx.unprotect(&first).unwrap();
    assert_eq!(rx.unprotect(&out).unwrap(), rtp);
}
