use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use fasttran_core::discovery::Peer;
use fasttran_core::model::{TransferHub, TransferState};
use fasttran_core::network::AppEvent;
use fasttran_core::receiver::ReceiverServer;
use fasttran_core::sender::TransferService;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transfers_a_file_and_verifies_its_contents() {
    let source_directory = tempfile::tempdir().unwrap();
    let receive_directory = tempfile::tempdir().unwrap();
    let source = source_directory.path().join("payload.bin");
    let payload: Vec<u8> = (0..512 * 1024).map(|index| (index % 251) as u8).collect();
    std::fs::write(&source, &payload).unwrap();

    let receiver_hub = Arc::new(TransferHub::default());
    let sender_hub = Arc::new(TransferHub::default());
    let download_dir = Arc::new(RwLock::new(receive_directory.path().to_path_buf()));
    let (events, mut event_receiver) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let server = ReceiverServer::start(0, download_dir, receiver_hub.clone(), events)
        .await
        .unwrap();
    let peer = Peer::manual(Ipv4Addr::LOCALHOST, server.local_port());
    let service = TransferService::new(tokio::runtime::Handle::current(), sender_hub.clone());

    let transfer_id = service
        .enqueue(&peer, &source, "Test Sender".into())
        .unwrap();
    let receiver_snapshot = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(snapshot) = receiver_hub.get(transfer_id) {
                let snapshot = snapshot.snapshot();
                if snapshot.state == TransferState::AwaitingConfirmation {
                    break snapshot;
                }
                if snapshot.state.is_finished() {
                    panic!("receiver failed before confirmation: {snapshot:?}");
                }
            }
            if let Some(snapshot) = sender_hub.get(transfer_id) {
                let snapshot = snapshot.snapshot();
                if snapshot.state.is_finished() {
                    panic!("sender failed before receiver confirmation: {snapshot:?}");
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("receiver did not request confirmation");
    assert!(receiver_hub.accept_incoming(receiver_snapshot.id));

    let _receiver_snapshot = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = receiver_hub.get(transfer_id).unwrap().snapshot();
            if snapshot.state == TransferState::Completed {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("receiver did not finish automatically");

    let received_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(AppEvent::Received { transfer_id, .. }) = event_receiver.recv().await
                && transfer_id == receiver_snapshot.id
            {
                break true;
            }
        }
    })
    .await
    .expect("receiver did not emit a completion notice");
    assert!(received_event);

    let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = sender_hub.get(transfer_id).unwrap().snapshot();
            if snapshot.state.is_finished() {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("transfer timed out");

    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert_eq!(
        receiver_hub.get(transfer_id).unwrap().snapshot().state,
        TransferState::Completed
    );
    let destination: PathBuf = receive_directory.path().join("payload.bin");
    assert_eq!(snapshot.file_path, source);
    assert_eq!(std::fs::read(destination).unwrap(), payload);
    assert_eq!(snapshot.total_bytes, payload.len() as u64);
    assert_eq!(snapshot.transferred_bytes, payload.len() as u64);
}
