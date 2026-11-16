//! Reliable provisional responses as the UAS (RFC 3262): `Require: 100rel` + `RSeq`
//! on the 180, retransmission with T1 doubling until the PRACK arrives, PRACK matching, and the
//! rule that no 2xx goes out while a reliable provisional is unacknowledged.
//!
//! No vendor patch is needed: the 180 is an ordinary provisional response (`ringing()` with the
//! extra headers; a retransmission is the same call again with the same `RSeq`), and PRACK is
//! intercepted by [`Call::on_prack`] before rsipstack's dialog handling.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::{Header, StatusCode};
use rsipstack::transaction::Transaction;
use siple_config::PrackMode;
use tokio::task::AbortHandle;

use super::util;
use super::{AnswerOptions, Call, CallState};
use crate::types::ALLOW;

/// RFC 3261 T1 in milliseconds. Test hook: [`set_prack_t1_ms`].
pub(crate) static PRACK_T1_MS: AtomicU64 = AtomicU64::new(500);

/// Test hook: overrides T1 (500 ms) for the 18x retransmissions. Process-wide.
#[doc(hidden)]
pub fn set_prack_t1_ms(ms: u64) {
    PRACK_T1_MS.store(ms.max(1), Ordering::Relaxed);
}

/// Reliable-provisional state of an inbound call.
#[derive(Default)]
pub(crate) struct Reliable {
    /// `RSeq` of the unacknowledged reliable 18x, if any.
    pub outstanding: Option<u32>,
    /// `RSeq` of the last acknowledged one (a retransmitted PRACK is answered 200).
    pub acked: Option<u32>,
    /// `answer()` arrived while a PRACK was outstanding: the 200 waits for it.
    pub deferred: Option<AnswerOptions>,
    /// Retransmission task.
    pub task: Option<AbortHandle>,
}

impl Reliable {
    pub(crate) fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.outstanding = None;
        self.deferred = None;
    }
}

/// Whether the 180 of this INVITE goes out reliably under `mode`.
pub(crate) fn wants_reliable(mode: PrackMode, headers: &rsipstack::sip::Headers) -> bool {
    let requires = util::has_token(headers, "require", "100rel");
    let supports = requires || util::has_token(headers, "supported", "100rel");
    match mode {
        PrackMode::Disabled => false,
        PrackMode::Supported => requires,
        PrackMode::Required => supports,
    }
}

/// `RAck: <rseq> <cseq> <method>`.
fn parse_rack(v: &str) -> Option<(u32, u32, String)> {
    let mut it = v.split_whitespace();
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.to_ascii_uppercase(),
    ))
}

impl Call {
    /// Sends the 180 reliably and starts its retransmission timer.
    pub(crate) fn send_reliable_ringing(self: &Arc<Self>, dialog: &InviteDialog) {
        let rseq: u32 = rand::random_range(1..=0x7fff_ffff);
        let headers = vec![
            Header::Allow(ALLOW.into()),
            Header::Supported(self.cfg.supported_tokens().into()),
            Header::Other("Require".into(), "100rel".into()),
            Header::Other("RSeq".into(), rseq.to_string()),
        ];
        // Not sent reliably any more once the call is over.
        {
            let mut g = self.lock();
            if g.info.state != CallState::Ringing {
                return;
            }
            g.reliable.outstanding = Some(rseq);
        }
        if let Err(e) = dialog.ringing(Some(headers.clone()), None) {
            tracing::warn!(call = %self.id, error = %e, "cannot send reliable 180");
            self.lock().reliable.outstanding = None;
            return;
        }
        let call = self.clone();
        let d = dialog.clone();
        let task = tokio::spawn(async move { call.retransmit_reliable(d, rseq, headers).await });
        self.lock().reliable.task = Some(task.abort_handle());
    }

    /// RFC 3262 §3: retransmit at T1, 2*T1, 4*T1 ... until 64*T1 in total, then give up with a
    /// 504 (the caller never PRACKed).
    async fn retransmit_reliable(self: Arc<Self>, dialog: InviteDialog, rseq: u32, h: Vec<Header>) {
        let t1 = Duration::from_millis(PRACK_T1_MS.load(Ordering::Relaxed));
        let total = t1 * 64;
        let mut waited = Duration::ZERO;
        let mut step = t1;
        loop {
            tokio::time::sleep(step).await;
            waited += step;
            {
                let g = self.lock();
                if g.reliable.outstanding != Some(rseq) || g.info.state != CallState::Ringing {
                    return;
                }
            }
            if waited >= total {
                tracing::warn!(call = %self.id, "no PRACK for the reliable 180; rejecting with 504");
                {
                    let mut g = self.lock();
                    g.reliable.outstanding = None;
                    g.reliable.deferred = None;
                    g.reliable.task = None;
                }
                let _ = self.reject(Some(504), Some("No PRACK received".into()), false);
                return;
            }
            tracing::debug!(call = %self.id, rseq, "retransmitting the reliable 180");
            if dialog.ringing(Some(h.clone()), None).is_err() {
                return;
            }
            step = (step * 2).min(total - waited);
        }
    }

    /// An in-dialog PRACK of an inbound call: matches `RAck` against the outstanding reliable
    /// 18x, answers it and releases a 200 that waited for it.
    pub(crate) async fn on_prack(self: &Arc<Self>, tx: &mut Transaction) {
        let rack = util::header_first(&tx.original.headers, "rack").and_then(|v| parse_rack(&v));
        let (status, deferred) = {
            let mut g = self.lock();
            let invite_cseq = g.invite.as_ref().and_then(util::req_cseq);
            match rack {
                Some((rseq, cseq, m))
                    if m == "INVITE"
                        && Some(cseq) == invite_cseq
                        && g.reliable.outstanding == Some(rseq) =>
                {
                    g.reliable.acked = Some(rseq);
                    g.reliable.outstanding = None;
                    if let Some(t) = g.reliable.task.take() {
                        t.abort();
                    }
                    (StatusCode::OK, g.reliable.deferred.take())
                }
                // Retransmission of a PRACK we already took.
                Some((rseq, _, _)) if g.reliable.acked == Some(rseq) => (StatusCode::OK, None),
                Some(_) => (StatusCode::CallTransactionDoesNotExist, None),
                None => (StatusCode::BadRequest, None),
            }
        };
        tracing::debug!(call = %self.id, code = status.code(), "PRACK");
        let _ = tx
            .reply_with(status, vec![Header::Allow(ALLOW.into())], None)
            .await;
        if let Some(opts) = deferred
            && let Err(e) = self.answer(opts)
        {
            tracing::warn!(call = %self.id, error = %e, "deferred answer failed after PRACK");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::sip::Headers;

    fn h(items: &[(&str, &str)]) -> Headers {
        items
            .iter()
            .map(|(k, v)| Header::Other((*k).into(), (*v).into()))
            .collect::<Vec<_>>()
            .into()
    }

    #[test]
    fn reliable_decision_matrix() {
        let none = Headers::default();
        let sup = h(&[("Supported", "replaces, 100rel")]);
        let req = h(&[("Require", "100rel")]);
        for (mode, a, b, c) in [
            (PrackMode::Disabled, false, false, false),
            (PrackMode::Supported, false, false, true),
            (PrackMode::Required, false, true, true),
        ] {
            assert_eq!(wants_reliable(mode, &none), a, "{mode:?} none");
            assert_eq!(wants_reliable(mode, &sup), b, "{mode:?} supported");
            assert_eq!(wants_reliable(mode, &req), c, "{mode:?} required");
        }
    }

    #[test]
    fn rack_parsing() {
        assert_eq!(
            parse_rack("776655 1 invite"),
            Some((776655, 1, "INVITE".into()))
        );
        assert_eq!(parse_rack("x 1 INVITE"), None);
        assert_eq!(parse_rack("1 2"), None);
    }
}
