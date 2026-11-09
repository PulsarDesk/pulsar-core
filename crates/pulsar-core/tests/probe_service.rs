//! The session-start bandwidth probe end to end: a client asks a real `serve_with` host (over
//! a local relay + two nodes, the encrypted session) for probe trains and gets an estimate. On
//! loopback nothing limits the rate, so every train keeps up and the estimate is "at least the
//! top rate".

use std::net::SocketAddr;
use std::time::Duration;

use pulsar_core::adapt::probe::{plan_rates, TRAIN_MS};
use pulsar_core::service::{run_probe, serve_with, DataHandlers};
use pulsar_core::{NetworkMode, Node};
use pulsar_relay::Relay;
use tokio::time::timeout;

const LOCAL: &str = "127.0.0.1:0";

#[tokio::test]
async fn a_probe_over_loopback_measures_every_train() {
	let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
	let relay_addr: SocketAddr = relay.local_addr().unwrap();
	tokio::spawn(relay.run());

	let host = Node::bind(LOCAL.parse().unwrap(), relay_addr, NetworkMode::Auto)
		.await
		.unwrap();
	let client = Node::bind(LOCAL.parse().unwrap(), relay_addr, NetworkMode::Auto)
		.await
		.unwrap();
	host.register().await.unwrap();
	client.register().await.unwrap();
	let host_id = host.self_id().await.unwrap();
	{
		let host = host.clone();
		tokio::spawn(async move {
			if let Some(session) = host.next_incoming().await {
				serve_with(session, Vec::new, |_| {}, |_, _| {}, |_| {}, DataHandlers::default()).await;
			}
		});
	}

	let mut sess = client.connect(host_id).await.unwrap();
	// A small plan keeps the test quick (2.5 + 5 Mbit trains).
	let rates = plan_rates(4_000);
	let est = timeout(Duration::from_secs(5), run_probe(&mut sess, rates.clone(), TRAIN_MS, 5.0))
		.await
		.expect("the probe finishes")
		.expect("the session stays up")
		.expect("an estimate comes back");
	assert_eq!(est.trains.len(), rates.len(), "{est:?}");
	assert!(est.trains.iter().all(|t| t.received > 0), "{est:?}");
	assert!(est.at_least, "nothing limits loopback: {est:?}");
	assert_eq!(est.kbps, *rates.last().unwrap(), "{est:?}");
}
