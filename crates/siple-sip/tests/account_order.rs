//! Account status and BLF listings follow the configured account order, not the id order.

mod common;

use common::{add, peer, rig};
use siple_config::BlfConfig;

#[tokio::test]
async fn status_and_blf_follow_the_set_order() {
    let r = rig();
    for (id, ext) in [("alpha", "1001"), ("bravo", "1002"), ("charlie", "1003")] {
        let mut c = peer(id);
        c.blf = vec![BlfConfig {
            uri: ext.into(),
            label: id.into(),
            ..Default::default()
        }];
        add(&r, c).await;
    }
    let ids = |r: &common::Rig| -> Vec<String> {
        r.stack
            .accounts_status()
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    let blf_accounts = |r: &common::Rig| -> Vec<String> {
        r.stack
            .blf_status(None)
            .into_iter()
            .map(|b| b.label)
            .collect()
    };
    // No order set: by id.
    assert_eq!(ids(&r), ["alpha", "bravo", "charlie"]);
    r.stack.set_account_order(&[
        "charlie".to_string(),
        "alpha".to_string(),
        "bravo".to_string(),
    ]);
    assert_eq!(ids(&r), ["charlie", "alpha", "bravo"]);
    assert_eq!(blf_accounts(&r), ["charlie", "alpha", "bravo"]);
}
