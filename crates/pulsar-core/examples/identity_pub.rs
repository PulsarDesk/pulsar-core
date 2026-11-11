//! Print the X25519 public key of a Pulsar `identity.key` (lowercase hex, the form
//! `paired_devices.json` stores). The secret never leaves the machine; only the
//! public half is printed. Developer tool for provisioning trusted devices by hand.
//!
//! `cargo run -p pulsar-core --example identity_pub -- <path to identity.key>`

fn main() {
	let Some(path) = std::env::args().nth(1) else {
		eprintln!("usage: identity_pub <identity.key>");
		std::process::exit(2);
	};
	let bytes = match std::fs::read(&path) {
		Ok(b) => b,
		Err(e) => {
			eprintln!("cannot read {path}: {e}");
			std::process::exit(1);
		}
	};
	let Ok(secret) = <[u8; 32]>::try_from(bytes.as_slice()) else {
		eprintln!("{path}: not a 32-byte identity");
		std::process::exit(1);
	};
	let id = pulsar_core::crypto::Identity::from_secret_bytes(secret);
	let hex: String = id.public_bytes().iter().map(|b| format!("{b:02x}")).collect();
	println!("{hex}");
}
