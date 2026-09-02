//! Persistent app configuration.
//!
//! The relay endpoint is **user-changeable** (the app can point at any relay,
//! including a self-hosted or local one), and the network mode controls the
//! P2P/relay strategy — matching the design's Ayarlar → Ağ section.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Default relay endpoint — the official public Pulsar relay, so the app works out of
/// the box. Written WITHOUT a port: [`crate::proto::DEFAULT_RELAY_PORT`] is implied and
/// filled in when the address is resolved, so the common case shows a clean hostname.
/// A port is only ever written when the operator actually runs on a different one
/// (`my-relay.example.com:9000`). Users override it in Settings → Ağ to point at a
/// self-hosted or local relay.
pub const DEFAULT_RELAY: &str = "relay.pulsardesk.com";

/// How Pulsar establishes a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
	/// Try direct P2P first, fall back to the relay automatically. (Recommended.)
	#[default]
	Auto,
	/// Only ever connect directly (no relay fallback).
	P2pOnly,
	/// Always go through the relay (skip hole punching).
	RelayOnly,
}

/// UI language. The app ships Turkish (default) and English; the core stays
/// language-agnostic and just stores the choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
	#[default]
	Tr,
	En,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
	/// `host:port` of the relay / rendezvous server. Changeable by the user.
	pub relay: String,
	/// Durable ACCESS KEYS for relays that require authentication (v5), keyed by the relay
	/// address they were issued by. The relay hands one back the first time a device
	/// satisfies its password/2FA prompt; every later registration presents the key
	/// instead, so the user is asked once per device and not again — until the operator
	/// changes the relay's credentials, which invalidates it and re-triggers the prompt.
	///
	/// Deliberately NOT the password: the password is entered in a prompt and never
	/// persisted, and a key is useless on any other device (the relay binds it to this
	/// device's public key). `#[serde(default)]` so older configs still load.
	#[serde(default)]
	pub relay_keys: std::collections::HashMap<String, String>,
	/// Run a relay INSIDE this app and register against it instead of a remote one
	/// (Settings → Ağ toggle). While set, `relay` is ignored: the address is by definition
	/// local, so there is nothing for the user to type. `#[serde(default)]` = off.
	#[serde(default)]
	pub use_local_relay: bool,
	/// Connection strategy.
	pub network_mode: NetworkMode,
	/// Friendly name advertised to peers.
	pub device_name: String,
	/// UI language.
	pub language: Language,
	/// Allow unattended (gözetimsiz) access to this host.
	pub unattended_access: bool,
	/// Optional PERSISTENT connect password (empty = none). Accepted alongside the
	/// rotating one-time password — a client presenting either is let in without
	/// the Allow/Deny prompt. Wrong attempts are rate-limited host-side (the
	/// password is a standing secret, so it must not be brute-forceable).
	#[serde(default)]
	pub connect_password: String,
	/// Stream this host's audio to the client (host → client). When off, the
	/// session is video-only. See [`crate::audio`] for how game mode overrides this.
	/// `#[serde(default)]` so configs written before this field still load.
	#[serde(default = "default_true")]
	pub transmit_audio: bool,
	/// Silence this host's *local* speakers while streaming (the sound then plays
	/// only on the client). Independent of [`Self::transmit_audio`]; game mode
	/// forces both on so audio moves entirely to the player.
	#[serde(default)]
	pub mute_host_audio: bool,
	/// Audio capture source override (empty = platform default). Windows: a
	/// DirectShow device name (a loopback / "Stereo Mix" / virtual cable); Linux: a
	/// PulseAudio/PipeWire source (typically a sink `.monitor`); macOS: an
	/// AVFoundation device index. Configurable because the right loopback device is
	/// machine-specific.
	#[serde(default)]
	pub audio_input: String,
	/// Local node listen port for direct/P2P connections (`0` = pick automatically).
	/// Set a fixed port to make port-forwarding to this host predictable.
	#[serde(default)]
	pub node_port: u16,
	/// What identity to present to a peer when connecting: the OS account photo
	/// (`user`), the desktop wallpaper (`wallpaper`), or nothing (`anonymous`).
	/// The display name shown alongside is [`Self::device_name`].
	#[serde(default = "default_avatar_mode")]
	pub avatar_mode: String,
	/// Use the **native renderer** (a bundled ffplay window, hardware-decoded) for
	/// incoming video instead of the in-webview WebCodecs canvas. Far lighter on
	/// CPU/GPU; Windows-only, opt-in, falls back to the webview if ffplay won't run.
	#[serde(default)]
	pub native_player: bool,
	/// Host audio channel layout to capture + encode (stereo / 5.1 / 7.1). Threads
	/// into [`crate::audio::AudioSettings::layout`]. `#[serde(default)]` (stereo) so
	/// configs written before surround support still load and stay stereo.
	#[serde(default)]
	pub audio_layout: crate::audio::ChannelLayout,
	/// Hardware acceleration for the APP'S OWN UI (the WebKitGTK/WebView2 webview that draws
	/// the menus/settings) — NOT the video stream's encode/decode (those are separate per-session
	/// codec/encoder settings). `None` = platform default: ON everywhere EXCEPT the Orange Pi 5
	/// (RK3588/Mali), where WebKitGTK's accelerated compositing has an unrecoverable "stops
	/// presenting" freeze, so it defaults OFF there. `Some(true/false)` overrides. Read once at
	/// process startup (sets the WebKitGTK env), so a change needs an app restart to apply.
	#[serde(default)]
	pub ui_hardware_accel: Option<bool>,
	/// (Windows) Re-launch elevated (UAC) on startup so this machine can act as a full host:
	/// injecting keyboard/mouse into ELEVATED app windows is blocked for a non-elevated process
	/// (UIPI), so without admin the remote user can't control e.g. Task Manager / an installer /
	/// any "Run as administrator" window. Default ON; turn it off in Settings → Güvenlik to launch
	/// without the prompt. Read once at process startup, so a change needs an app restart.
	#[serde(default = "default_true")]
	pub request_admin: bool,
}

fn default_avatar_mode() -> String {
	"user".to_string()
}

fn default_true() -> bool {
	true
}

impl Default for Config {
	fn default() -> Self {
		Self {
			relay: DEFAULT_RELAY.to_string(),
			relay_keys: std::collections::HashMap::new(),
			use_local_relay: false,
			network_mode: NetworkMode::Auto,
			device_name: default_device_name(),
			language: Language::Tr,
			unattended_access: false,
			connect_password: String::new(),
			transmit_audio: true,
			mute_host_audio: false,
			audio_input: String::new(),
			node_port: 0,
			avatar_mode: default_avatar_mode(),
			native_player: false,
			audio_layout: crate::audio::ChannelLayout::Stereo,
			ui_hardware_accel: None,
			request_admin: true,
		}
	}
}

impl Config {
	/// Load from a JSON file, or return defaults if it doesn't exist / is invalid.
	pub fn load(path: impl AsRef<Path>) -> Self {
		let mut cfg: Self = std::fs::read_to_string(path)
			.ok()
			.and_then(|s| serde_json::from_str(&s).ok())
			.unwrap_or_default();
		cfg.normalise_relay();
		cfg
	}

	/// Drop a redundant `:21116` from the stored relay address.
	///
	/// The default port is implied everywhere it matters (resolution fills it in), so
	/// carrying it in the value only shows the user a port they never chose — including on
	/// installs written before the address became port-optional. A NON-default port is left
	/// exactly as typed, because there it is the whole point.
	fn normalise_relay(&mut self) {
		let trimmed = self.relay.trim();
		if let Some((host, port)) = split_relay_port(trimmed) {
			if !host.is_empty() && port == crate::proto::DEFAULT_RELAY_PORT.to_string() {
				self.relay = host.to_string();
				return;
			}
		}
		if trimmed.len() != self.relay.len() {
			self.relay = trimmed.to_string();
		}
	}

	/// Persist to a JSON file (creating parent dirs).
	///
	/// The write is **atomic**: the JSON is written to a sibling temp file and
	/// then renamed over the target. A crash/power-loss mid-write therefore
	/// leaves the previous config intact instead of a truncated file that
	/// [`Config::load`] would silently discard (resetting every setting).
	pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
		let path = path.as_ref();
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		let json = serde_json::to_string_pretty(self).expect("config serializes");
		// Write to a sibling temp file (same dir → same filesystem, so the rename
		// is atomic on Windows/macOS/Linux), then rename over the target. The pid
		// keeps concurrent writers (e.g. separate ASTER seats) from clobbering one
		// another's temp file.
		let tmp = match path.file_name() {
			Some(name) => {
				let mut fname = name.to_os_string();
				fname.push(format!(".{}.tmp", std::process::id()));
				path.with_file_name(fname)
			}
			None => path.with_extension("tmp"),
		};
		std::fs::write(&tmp, json)?;
		match std::fs::rename(&tmp, path) {
			Ok(()) => Ok(()),
			Err(e) => {
				let _ = std::fs::remove_file(&tmp);
				Err(e)
			}
		}
	}

	/// The audio toggles as an [`crate::audio::AudioSettings`] for policy resolution.
	pub fn audio_settings(&self) -> crate::audio::AudioSettings {
		crate::audio::AudioSettings {
			transmit: self.transmit_audio,
			mute_host: self.mute_host_audio,
			layout: self.audio_layout,
		}
	}

	/// The configured capture source as an [`crate::audio::AudioInput`]; an empty
	/// override resolves to the platform default.
	pub fn audio_input(&self) -> crate::audio::AudioInput {
		let dev = self.audio_input.trim();
		if dev.is_empty() {
			crate::audio::AudioInput::default_for_os()
		} else if cfg!(windows) {
			crate::audio::AudioInput::Dshow(dev.to_string())
		} else if cfg!(target_os = "macos") {
			crate::audio::AudioInput::AvFoundation(dev.parse().unwrap_or(0))
		} else {
			crate::audio::AudioInput::Pulse(dev.to_string())
		}
	}

	/// Windows only: capture system audio via **WASAPI loopback** (the default render
	/// endpoint) rather than an ffmpeg dshow device. True when no explicit device is set
	/// or it's the `loopback`/`wasapi` sentinel — so audio streams out of the box without
	/// a `virtual-audio-capturer` / Stereo Mix device installed. A named device opts back
	/// into the dshow path ([`Self::audio_input`]).
	pub fn audio_loopback(&self) -> bool {
		if !cfg!(windows) {
			return false;
		}
		let d = self.audio_input.trim();
		d.is_empty() || d.eq_ignore_ascii_case("loopback") || d.eq_ignore_ascii_case("wasapi")
	}

	/// Returns true if the relay endpoint looks like a usable `host:port`.
	/// The stored access key for a relay address (normalised the same way the pins are:
	/// trimmed + lowercased), if this device has already authenticated there.
	pub fn relay_key_for(&self, relay: &str) -> Option<&str> {
		self.relay_keys
			.get(&relay.trim().to_ascii_lowercase())
			.map(String::as_str)
	}

	/// Remember the access key a relay just issued (or, with `None`, forget it — e.g. the
	/// operator rotated the credentials and the key stopped working).
	pub fn set_relay_key(&mut self, relay: &str, key: Option<String>) {
		let k = relay.trim().to_ascii_lowercase();
		match key {
			Some(v) => {
				self.relay_keys.insert(k, v);
			}
			None => {
				self.relay_keys.remove(&k);
			}
		}
	}

	/// Is the configured relay address usable? A bare host (`relay.pulsardesk.com`,
	/// `192.168.1.5`) is valid on its own — the default relay port is implied. An explicit
	/// port is validated when one is given. IPv6 literals must be bracketed to carry a port
	/// (`[::1]:21116`); a bare `::1` is treated as a host, which is what a user means.
	pub fn relay_is_valid(&self) -> bool {
		let s = self.relay.trim();
		if s.is_empty() {
			return false;
		}
		match split_relay_port(s) {
			Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
			None => true, // bare host — the default port applies
		}
	}
}

/// Split `host:port` when the address really carries a port. Returns `None` for a bare
/// host, including an unbracketed IPv6 literal (whose colons are part of the address).
/// `[v6]:port` is split at the bracket.
pub fn split_relay_port(s: &str) -> Option<(&str, &str)> {
	let s = s.trim();
	if let Some(rest) = s.strip_prefix('[') {
		// Bracketed IPv6: `[::1]:21116` → ("[::1]", "21116"); `[::1]` → None.
		let (host, tail) = rest.split_once(']')?;
		let port = tail.strip_prefix(':')?;
		let _ = host;
		return Some((&s[..host.len() + 2], port));
	}
	let (host, port) = s.rsplit_once(':')?;
	// More than one colon and no brackets → a bare IPv6 literal, not host:port.
	if host.contains(':') {
		return None;
	}
	Some((host, port))
}

fn default_device_name() -> String {
	// Use the real OS hostname cross-platform (whoami handles Windows/Linux/macOS).
	// `$HOSTNAME` is normally unset on Windows (the name lives in COMPUTERNAME) and
	// often unexported to GUI sessions on Linux, so reading it gave the generic
	// placeholder on most fresh installs.
	whoami::fallible::hostname()
		.ok()
		.filter(|h| !h.trim().is_empty())
		.unwrap_or_else(|| "Pulsar Cihazı".to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn defaults_are_auto_mode_and_turkish() {
		let c = Config::default();
		assert_eq!(c.network_mode, NetworkMode::Auto);
		assert_eq!(c.language, Language::Tr);
		assert!(
			c.relay_is_valid(),
			"default relay should be valid host:port"
		);
	}

	#[test]
	fn network_mode_serializes_kebab_case() {
		assert_eq!(
			serde_json::to_string(&NetworkMode::P2pOnly).unwrap(),
			"\"p2p-only\""
		);
		assert_eq!(
			serde_json::from_str::<NetworkMode>("\"relay-only\"").unwrap(),
			NetworkMode::RelayOnly
		);
	}

	#[test]
	fn relay_validation_catches_garbage() {
		let mut c = Config::default();
		// A bare host is VALID: the default relay port is implied, so the address a user
		// sees and types is just the hostname.
		c.relay = "no-port".into();
		assert!(c.relay_is_valid());
		c.relay = DEFAULT_RELAY.into();
		assert!(c.relay_is_valid(), "the shipped default carries no port");
		c.relay = "192.168.1.5".into();
		assert!(c.relay_is_valid());
		// An explicit port is still validated.
		c.relay = "host:notaport".into();
		assert!(!c.relay_is_valid());
		c.relay = "host:99999".into();
		assert!(!c.relay_is_valid());
		c.relay = "127.0.0.1:21116".into();
		assert!(c.relay_is_valid());
		// Empty is never valid.
		c.relay = "  ".into();
		assert!(!c.relay_is_valid());
	}

	#[test]
	fn loading_drops_a_redundant_default_port_but_keeps_a_real_one() {
		let dir = std::env::temp_dir().join(format!("pulsar-relay-norm-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("config.json");

		// An install written before the address became port-optional.
		let mut c = Config::default();
		c.relay = "relay.pulsardesk.com:21116".into();
		c.save(&path).unwrap();
		assert_eq!(
			Config::load(&path).relay,
			"relay.pulsardesk.com",
			"the implied default port should not be shown back to the user"
		);

		// A deliberately non-default port is preserved verbatim.
		c.relay = "my-relay.example.com:9000".into();
		c.save(&path).unwrap();
		assert_eq!(Config::load(&path).relay, "my-relay.example.com:9000");

		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn relay_port_split_handles_ipv6_and_bare_hosts() {
		// Bare hosts (v4, v6, dns) carry no port.
		assert_eq!(split_relay_port("relay.pulsardesk.com"), None);
		assert_eq!(split_relay_port("192.168.1.5"), None);
		assert_eq!(
			split_relay_port("::1"),
			None,
			"unbracketed IPv6 is a host, not host:port"
		);
		// Explicit ports split.
		assert_eq!(split_relay_port("host:9000"), Some(("host", "9000")));
		assert_eq!(split_relay_port("[::1]:21116"), Some(("[::1]", "21116")));
		assert_eq!(split_relay_port("[::1]"), None);
	}

	#[test]
	fn audio_defaults_transmit_without_muting() {
		let c = Config::default();
		assert!(c.transmit_audio);
		assert!(!c.mute_host_audio);
		let s = c.audio_settings();
		assert!(s.transmit && !s.mute_host);
	}

	#[cfg(windows)]
	#[test]
	fn audio_loopback_is_windows_default_until_a_device_is_named() {
		let mut c = Config::default();
		// Empty (the default) → WASAPI loopback, so audio works with no capture device installed.
		assert!(c.audio_loopback());
		c.audio_input = "loopback".into();
		assert!(c.audio_loopback());
		// A named dshow device opts back out of loopback.
		c.audio_input = "Stereo Mix".into();
		assert!(!c.audio_loopback());
	}

	#[test]
	fn old_config_without_audio_fields_still_loads() {
		// A config written before the audio fields existed must still deserialize
		// (serde defaults fill them) rather than resetting every other setting.
		let json = r#"{"relay":"1.2.3.4:21116","network_mode":"relay-only",
			"device_name":"Eski PC","language":"en","unattended_access":true}"#;
		let c: Config = serde_json::from_str(json).expect("loads with serde defaults");
		assert_eq!(c.device_name, "Eski PC");
		assert!(c.unattended_access);
		assert!(c.transmit_audio); // default-true
		assert!(!c.mute_host_audio); // default-false
	}

	#[test]
	fn load_missing_file_yields_defaults() {
		let c = Config::load("/nonexistent/pulsar/config.json");
		assert_eq!(c, Config::default());
	}

	#[test]
	fn save_then_load_round_trips() {
		let dir = std::env::temp_dir().join(format!("pulsar-cfg-test-{}", std::process::id()));
		let path = dir.join("config.json");
		let mut cfg = Config::default();
		// A NON-default port, so the value survives load unchanged — `:21116` is stripped on
		// load by design (see `loading_drops_a_redundant_default_port_but_keeps_a_real_one`).
		cfg.relay = "127.0.0.1:9000".into();
		cfg.network_mode = NetworkMode::RelayOnly;
		cfg.device_name = "Ev PC’si".into();
		cfg.save(&path).unwrap();

		let loaded = Config::load(&path);
		assert_eq!(loaded, cfg);
		let _ = std::fs::remove_dir_all(&dir);
	}
}
