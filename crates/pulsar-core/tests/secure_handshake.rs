//! Both connect paths negotiate the forward-secret (v3) key and advertise PAKE, and a
//! password attempt over them authenticates end-to-end without the password travelling.

use std::net::SocketAddr;
use std::time::Duration;

use pulsar_core::service::{accept, authenticate, recv_auth, reject, AuthOutcome, Credential};
use pulsar_core::{NetworkMode, Node, Session};
use pulsar_relay::Relay;
use tokio::time::timeout;

const LOCAL: &str = "127.0.0.1:0";

async fn assert_secure(c: &Session, h: &Session) {
	assert!(
		c.is_forward_secret().await,
		"client side must be forward secret"
	);
	assert!(
		h.is_forward_secret().await,
		"host side must be forward secret"
	);
	assert!(c.peer_supports_pake().await && h.peer_supports_pake().await);
}

/// Host task: accept only the connect password "sezam" (via PAKE).
fn serve_one(mut h: Session) {
	tokio::spawn(async move {
		let Some(a) = recv_auth(&mut h).await else {
			return;
		};
		if a.verify(&mut h, "", "sezam").await == Credential::Fixed {
			let _ = accept(&mut h).await;
		} else {
			let _ = reject(&mut h).await;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	});
}

#[tokio::test]
async fn relay_path_is_forward_secret_and_pake_authenticates() {
	let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
	let addr: SocketAddr = relay.local_addr().unwrap();
	tokio::spawn(relay.run());
	let host = Node::bind(LOCAL.parse().unwrap(), addr, NetworkMode::Auto)
		.await
		.unwrap();
	let client = Node::bind(LOCAL.parse().unwrap(), addr, NetworkMode::Auto)
		.await
		.unwrap();
	host.register().await.unwrap();
	client.register().await.unwrap();
	let host_id = host.self_id().await.unwrap();

	let mut c = client.connect(host_id).await.unwrap();
	let h = timeout(Duration::from_secs(2), host.next_incoming())
		.await
		.unwrap()
		.unwrap();
	assert_secure(&c, &h).await;
	serve_one(h);
	let v = timeout(Duration::from_secs(5), authenticate(&mut c, "sezam"))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(v, AuthOutcome::Accepted);

	let mut c = client.connect(host_id).await.unwrap();
	let h = timeout(Duration::from_secs(2), host.next_incoming())
		.await
		.unwrap()
		.unwrap();
	serve_one(h);
	let v = timeout(Duration::from_secs(5), authenticate(&mut c, "yanlis"))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(v, AuthOutcome::Denied);
}

#[tokio::test]
async fn direct_ip_path_is_forward_secret_and_pake_authenticates() {
	let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
	let host = Node::bind(LOCAL.parse().unwrap(), dead, NetworkMode::P2pOnly)
		.await
		.unwrap();
	let client = Node::bind(LOCAL.parse().unwrap(), dead, NetworkMode::P2pOnly)
		.await
		.unwrap();
	let host_addr = host.local_addr().unwrap();

	let mut c = client.connect_direct(host_addr, None).await.unwrap();
	let h = timeout(Duration::from_secs(2), host.next_incoming())
		.await
		.unwrap()
		.unwrap();
	assert_secure(&c, &h).await;
	serve_one(h);
	let v = timeout(Duration::from_secs(5), authenticate(&mut c, "sezam"))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(v, AuthOutcome::Accepted);
}
