use crate::test_utils::{
    create_lnd, create_rld, fund_rld, generate_blocks_and_wait, open_channel_from_lnd,
    open_channel_from_rld,
};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use lnd::tonic_lnd::lnrpc::channel_point::FundingTxid;
use lnd::tonic_lnd::lnrpc::{ChannelPoint, CloseChannelRequest, InvoiceRequest, SendRequest};
use rld::models::payment::PaymentStatus;
use std::str::FromStr;
use std::time::Duration;

mod test_utils;

#[tokio::test]
async fn test_fund_rld_wallet() {
    let node = create_rld().await;

    fund_rld(&node).await;
}

#[tokio::test]
async fn test_open_channel_from_rld() {
    let node = create_rld().await;
    let mut lnd = create_lnd().await;
    open_channel_from_rld(&node, &mut lnd).await;
}

#[tokio::test]
async fn test_open_channel_from_lnd() {
    let node = create_rld().await;
    let mut lnd = create_lnd().await;
    open_channel_from_lnd(&node, &mut lnd).await;
}

#[tokio::test]
async fn test_pay_invoice() {
    let node = create_rld().await;
    let mut lnd = create_lnd().await;
    open_channel_from_rld(&node, &mut lnd).await;

    let lightning = lnd.client.lightning();
    let resp = lightning
        .add_invoice(InvoiceRequest {
            memo: "".to_string(),
            value_msat: 10_000_000, // 10k sats
            private: false,
            is_keysend: false,
            is_amp: false,
        })
        .await
        .unwrap();
    let invoice = Bolt11Invoice::from_str(&resp.into_inner().payment_request).unwrap();

    let inv = node
        .pay_invoice_with_timeout(invoice, None, None)
        .await
        .unwrap();

    assert_eq!(inv.status(), PaymentStatus::Completed);
    assert!(inv.preimage().is_some());
    assert!(inv.fee_msats().is_some());
}

/// Open a channel from rld to lnd and then have rld force close it
#[tokio::test]
async fn force_close_outbound_channel_from_rld() {
    let node = create_rld().await;
    let mut lnd = create_lnd().await;
    open_channel_from_rld(&node, &mut lnd).await;

    let starting_balance = node.get_balance();
    assert_eq!(starting_balance.on_chain(), 98_998_766);
    assert_eq!(starting_balance.lightning, 1_000_000);

    let channel = node.channel_manager.list_channels()[0].clone();
    node.channel_manager
        .force_close_broadcasting_latest_txn(&channel.channel_id, &channel.counterparty.node_id)
        .unwrap();

    let new_balance = node.get_balance();
    assert_eq!(new_balance.on_chain(), starting_balance.on_chain());
    assert_eq!(new_balance.lightning, 0);
    assert_eq!(new_balance.force_close, 999_056);

    // generate some blocks for ldk to handle the force close
    generate_blocks_and_wait(6).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    generate_blocks_and_wait(300).await;

    // wait for rld to sync and sweep the channel
    for _ in 0..10 {
        generate_blocks_and_wait(6).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let balance = node.get_balance();
        if balance.on_chain() > starting_balance.on_chain() && balance.force_close == 0 {
            break;
        }
    }

    // check that we swept the channel
    let final_balance = node.get_balance();
    assert_eq!(final_balance.lightning, 0);
    assert_eq!(final_balance.force_close, 0);
    assert!(final_balance.on_chain() > starting_balance.on_chain());
}

/// Open a channel from lnd to rld and then have rld force close it
#[tokio::test]
#[ignore = "need to figure out sweeper"]
async fn force_close_inbound_channel_from_rld() {
    let node = create_rld().await;
    let mut lnd = create_lnd().await;
    open_channel_from_lnd(&node, &mut lnd).await;

    let d = Description::new(String::new()).unwrap();
    let invoice = node
        .create_invoice(
            Bolt11InvoiceDescription::Direct(&d),
            Some(100_000_000),
            None,
        )
        .unwrap();

    let lightning = lnd.client.lightning();
    let resp = lightning
        .send_payment_sync(SendRequest {
            payment_request: invoice.bolt11.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let resp = resp.into_inner();
    assert_eq!(resp.payment_error, "");

    // wait for payment to complete
    tokio::time::sleep(Duration::from_secs(1)).await;

    let starting_balance = node.get_balance();
    assert_eq!(starting_balance.on_chain(), 0);
    assert_eq!(starting_balance.lightning, 100_000);

    let channel = node.channel_manager.list_channels()[0].clone();
    node.channel_manager
        .force_close_broadcasting_latest_txn(&channel.channel_id, &channel.counterparty.node_id)
        .unwrap();

    let new_balance = node.get_balance();
    assert_eq!(new_balance.on_chain(), starting_balance.on_chain());
    assert_eq!(new_balance.lightning, 0);
    assert_eq!(new_balance.force_close, 100_000);

    // generate some blocks for ldk to handle the force close
    generate_blocks_and_wait(6).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    generate_blocks_and_wait(300).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    generate_blocks_and_wait(2016).await;

    // need to sleep for ldk to handle the sweep
    // fixme figure out how to lower this
    tokio::time::sleep(Duration::from_secs(40)).await;

    // wait for rld to sync and sweep the channel
    for _ in 0..100 {
        generate_blocks_and_wait(6).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let balance = node.get_balance();
        if balance.on_chain() > starting_balance.on_chain() && balance.force_close == 0 {
            break;
        }
    }

    // check that we swept the channel
    let final_balance = node.get_balance();
    assert_eq!(final_balance.lightning, 0);
    assert_eq!(final_balance.force_close, 0);
    assert_eq!(final_balance.on_chain(), 97_550 + starting_balance.on_chain());
}

/// Open a channel from rld to lnd and then have lnd force close it
#[tokio::test]
async fn force_close_outbound_channel_from_lnd() {
    let node = create_rld().await;
    let mut lnd = create_lnd().await;
    open_channel_from_rld(&node, &mut lnd).await;

    let starting_balance = node.get_balance();
    assert_eq!(starting_balance.on_chain(), 98998766);
    assert_eq!(starting_balance.lightning, 1_000_000);

    // force close the channel
    let channel = node.channel_manager.list_channels()[0]
        .funding_txo
        .clone()
        .unwrap();
    let lightning = lnd.client.lightning();
    lightning
        .close_channel(CloseChannelRequest {
            channel_point: Some(ChannelPoint {
                output_index: channel.index as u32,
                funding_txid: Some(FundingTxid::FundingTxidStr(channel.txid.to_string())),
            }),
            force: true,
            ..Default::default()
        })
        .await
        .unwrap();

    // mine the close transaction
    generate_blocks_and_wait(1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let new_balance = node.get_balance();
    assert_eq!(new_balance.on_chain(), starting_balance.on_chain());
    assert_eq!(new_balance.lightning, 0);
    assert_eq!(new_balance.force_close, 999_056);

    // generate some blocks for ldk to handle the force close
    generate_blocks_and_wait(6).await;

    // wait for rld to sync and sweep the channel
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let balance = node.get_balance();
        if balance.on_chain() > starting_balance.on_chain() && balance.force_close == 0 {
            break;
        }
    }

    // check that we swept the channel
    let final_balance = node.get_balance();
    assert_eq!(final_balance.lightning, 0);
    assert_eq!(final_balance.force_close, 0);
    assert!(final_balance.on_chain() > starting_balance.on_chain());
}

/// Open a channel from lnd to rld and then have lnd force close it
#[tokio::test]
async fn force_close_inbound_channel_from_lnd() {
    let node = create_rld().await;
    let mut lnd = create_lnd().await;
    open_channel_from_lnd(&node, &mut lnd).await;

    let d = Description::new(String::new()).unwrap();
    let invoice = node
        .create_invoice(
            Bolt11InvoiceDescription::Direct(&d),
            Some(100_000_000),
            None,
        )
        .unwrap();

    let lightning = lnd.client.lightning();
    let resp = lightning
        .send_payment_sync(SendRequest {
            payment_request: invoice.bolt11.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let resp = resp.into_inner();
    assert_eq!(resp.payment_error, "");

    // wait for payment to complete
    tokio::time::sleep(Duration::from_secs(1)).await;

    let starting_balance = node.get_balance();
    assert_eq!(starting_balance.on_chain(), 0);
    assert_eq!(starting_balance.lightning, 100_000);

    // force close the channel
    let channel = node.channel_manager.list_channels()[0]
        .funding_txo
        .clone()
        .unwrap();
    let lightning = lnd.client.lightning();
    lightning
        .close_channel(CloseChannelRequest {
            channel_point: Some(ChannelPoint {
                output_index: channel.index as u32,
                funding_txid: Some(FundingTxid::FundingTxidStr(channel.txid.to_string())),
            }),
            force: true,
            ..Default::default()
        })
        .await
        .unwrap();

    // mine the close transaction and wait rld to sync
    generate_blocks_and_wait(3).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let new_balance = node.get_balance();
    assert_eq!(new_balance.on_chain(), starting_balance.on_chain());
    assert_eq!(new_balance.lightning, 0);
    assert_eq!(new_balance.force_close, 100_000);

    // wait for rld to sync and sweep the channel
    for _ in 0..10 {
        generate_blocks_and_wait(6).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let balance = node.get_balance();
        if balance.on_chain() > starting_balance.on_chain() && balance.force_close == 0 {
            break;
        }
    }

    // check that we swept the channel
    let final_balance = node.get_balance();
    assert_eq!(final_balance.lightning, 0);
    assert_eq!(final_balance.force_close, 0);
    assert_eq!(final_balance.on_chain(), 99_020);
}
