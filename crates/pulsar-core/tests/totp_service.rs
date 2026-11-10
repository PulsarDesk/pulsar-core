//! Two-factor (TOTP) step of the session auth handshake: after the password, a host with
//! 2FA on asks for the authenticator code; a wrong code is asked again with the retry flag,
//! the right one is accepted. (The code check itself is `pulsar_core::totp`, unit-tested
//! there; this pins the wire messages and the capability bit between two real nodes.)

use std::net::SocketAddr;
use std::time::Duration;

use pulsar_core::service::{
	accept, authenticate, need_totp, recv_auth, recv_host_auth, recv_totp, reject, send_totp,
	AuthOutcome, Credential, HostAuth, TotpReply,
};
use pulsar_core::{NetworkMode, Node};
use pulsar_relay::Relay;
use tokio::time::timeout;

const LOCAL: &str = "127.0.0.1:0";
const GOOD: &str = "123456";

#[tokio::test]
async fn password_then_totp_code_with_one_retry() {
	let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
	let relay_addr: SocketAddr = relay.local_addr().unwrap();
	tokio::spawn(relay.run());

	let host = Node::bind(LOCAL.parse().unwrap(), relay_addr, NetworkMode::Auto).await.unwrap();
	let client = Node::bind(LOCAL.parse().unwrap(), relay_addr, NetworkMode::Auto).await.unwrap();
	host.register().await.unwrap();
	client.register().await.unwrap();
	let host_id = host.self_id().await.unwrap();

	// Host: password "sezam", then the 2FA code — wrong → ask again (retry), GOOD → accept.
	let (cap_tx, cap_rx) = tokio::sync::oneshot::channel::<bool>();
	{
		let host = host.clone();
		tokio::spawn(async move {
			let mut session = host.next_incoming().await.unwrap();
			let attempt = recv_auth(&mut session).await.unwrap();
			if attempt.verify(&mut session, "", "sezam").await != Credential::Fixed {
				let _ = reject(&mut session).await;
				return;
			}
			let _ = cap_tx.send(session.peer_supports_totp().await);
			let _ = need_totp(&mut session, false).await;
			loop {
				match recv_totp(&mut session).await {
					TotpReply::Code(c) if c == GOOD => {
						let _ = accept(&mut session).await;
						break;
					}
					TotpReply::Code(_) => {
						let _ = need_totp(&mut session, true).await;
					}
					TotpReply::Probe | TotpReply::Other => {}
					TotpReply::Gone => break,
				}
			}
			tokio::time::sleep(Duration::from_millis(200)).await;
		});
	}

	let mut s = client.connect(host_id).await.unwrap();
	let v = timeout(Duration::from_secs(3), authenticate(&mut s, "sezam")).await.unwrap().unwrap();
	assert_eq!(v, AuthOutcome::NeedTotp(false), "password passed → the host asks for the code");
	assert!(cap_rx.await.unwrap(), "this build advertises CAP_TOTP");

	send_totp(&mut s, "000000").await.unwrap();
	let v = timeout(Duration::from_secs(3), recv_host_auth(&mut s)).await.unwrap();
	assert!(matches!(v, HostAuth::NeedTotp(true)), "a wrong code is asked again with retry");

	send_totp(&mut s, " 123456 ").await.unwrap();
	let v = timeout(Duration::from_secs(3), recv_host_auth(&mut s)).await.unwrap();
	assert!(matches!(v, HostAuth::Ok | HostAuth::Other), "the right code is accepted");
	if matches!(v, HostAuth::Other) {
		let v = timeout(Duration::from_secs(3), recv_host_auth(&mut s)).await.unwrap();
		assert!(matches!(v, HostAuth::Ok), "the right code is accepted");
	}
}
