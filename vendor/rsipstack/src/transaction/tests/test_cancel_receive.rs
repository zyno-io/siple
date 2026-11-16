//! siple: `Transaction::receive` ends once the endpoint is cancelled.
use crate::sip::{headers::*, Uri};
use crate::transaction::endpoint::EndpointBuilder;
use crate::transaction::key::{TransactionKey, TransactionRole};
use crate::transaction::transaction::Transaction;
use crate::transport::TransportLayer;
use std::convert::TryFrom;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_receive_returns_none_after_endpoint_cancel() {
    let token = CancellationToken::new();
    let endpoint = EndpointBuilder::new()
        .with_cancel_token(token.clone())
        .with_transport_layer(TransportLayer::new(token.child_token()))
        .build();
    let request = crate::sip::Request {
        method: crate::sip::Method::Options,
        uri: Uri::try_from("sip:bob@example.com").unwrap(),
        headers: vec![
            Via::new("SIP/2.0/UDP uac.example.com:5060;branch=z9hG4bK1").into(),
            CSeq::new("1 OPTIONS").into(),
            From::new("<sip:alice@example.com>;tag=from-tag").into(),
            To::new("<sip:bob@example.com>").into(),
            CallId::new("callid@example.com").into(),
            MaxForwards::new("70").into(),
        ]
        .into(),
        version: crate::sip::Version::V2,
        body: vec![],
    };
    let key = TransactionKey::from_request(&request, TransactionRole::Client).unwrap();
    let mut tx = Transaction::new_client(key, request, endpoint.inner.clone(), None);
    // Nothing arrives and no timer runs (endpoint not served): would wait forever.
    let waiter = tokio::spawn(async move { tx.receive().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    token.cancel();
    let got = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("receive must end after cancel")
        .unwrap();
    assert!(got.is_none());
}
