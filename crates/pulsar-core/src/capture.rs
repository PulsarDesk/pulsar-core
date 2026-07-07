//! Wayland screen capture via the XDG **ScreenCast** desktop portal + GStreamer.
//!
//! On a Wayland session (KDE/GNOME) there is no global X root window to grab:
//! `x11grab` of the rootless Xwayland display only ever captures black. The portal
//! hands back a **PipeWire** video node we feed to GStreamer, encode to RTP/H.264,
//! and send to the client's WebCodecs viewer. (Input injection for remote control
//! is handled separately by uinput — see [`crate::input::DesktopInput`] — because
//! KDE's RemoteDesktop portal `Start` hangs without showing a dialog here.)
//!
//! Linux-only; the rest of the app calls [`is_wayland`] to decide between this and
//! the ffmpeg capture path in [`crate::pipeline`].
#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Child;

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use ashpd::WindowIdentifier;

/// True when running under Wayland, where `x11grab` would capture a black
/// (rootless Xwayland) screen and we must use the portal instead.
pub fn is_wayland() -> bool {
	std::env::var("XDG_SESSION_TYPE")
		.map(|v| v.eq_ignore_ascii_case("wayland"))
		.unwrap_or(false)
		|| std::env::var("WAYLAND_DISPLAY")
			.map(|v| !v.is_empty())
			.unwrap_or(false)
}

/// A running portal capture: the GStreamer child streaming to the client, the
/// PipeWire remote fd kept open for its lifetime, and the **portal ScreenCast
/// session** — which must be explicitly closed (ashpd does *not* close it on drop)
/// or the compositor keeps showing "your screen is being shared" forever.
pub struct WaylandCapture {
	child: Child,
	// `Option` so both `stop()` and the `Drop` safety-net can move the session out to
	// `close()` it (Drop only has `&mut self`).
	session: Option<Session<'static, Screencast<'static>>>,
	_pw_fd: OwnedFd,
}

impl WaylandCapture {
	/// Stop the capture: kill GStreamer and **close the portal session** so the
	/// compositor's screen-sharing indicator (KDE/GNOME) actually goes away. Just
	/// killing gst / dropping the fd is not enough — the portal session lingers.
	pub async fn stop(mut self) {
		let _ = self.child.kill();
		// Reap the child so the SIGKILLed gst-launch does not linger as a
		// <defunct> zombie until the whole app exits.  wait() on an already-dead
		// process returns immediately (the kernel already holds the exit status).
		let _ = self.child.wait();
		if let Some(session) = self.session.take() {
			let _ = session.close().await;
		}
	}
}

impl Drop for WaylandCapture {
	/// Safety net for the callers that REPLACE a capture without awaiting `stop()`
	/// (a restream — codec/bitrate/monitor change — or a reconnect spawns a fresh
	/// capture and drops the old `WaylandCapture`). `std::process::Child`'s own drop
	/// does NOT kill the process, so without this the old `gst-launch` lingers; worse,
	/// once its ScreenCast node dies PipeWire re-links the (autoconnect) `pipewiresrc`
	/// to the webcam, lighting the camera indicator. Kill + reap the child (sync) and
	/// fire-and-forget the portal-session close so neither the process nor the
	/// "screen is being shared" indicator leaks. `stop()` already took the session on
	/// the graceful path, so this only closes it when `stop()` was skipped.
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
		if let Some(session) = self.session.take() {
			if let Ok(handle) = tokio::runtime::Handle::try_current() {
				handle.spawn(async move {
					let _ = session.close().await;
				});
			}
		}
	}
}

/// Clear `FD_CLOEXEC` so a spawned child inherits the PipeWire fd.
fn clear_cloexec(fd: i32) -> std::io::Result<()> {
	// SAFETY: `fd` is a valid borrowed descriptor for the duration of the call.
	let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
	if flags < 0 {
		return Err(std::io::Error::last_os_error());
	}
	if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
		return Err(std::io::Error::last_os_error());
	}
	Ok(())
}

/// Closes a portal ScreenCast session when dropped. ashpd does NOT close the
/// session on drop, so if [`start`] is cancelled WHILE THE PICKER DIALOG IS STILL
/// OPEN — the realistic case being the client connection dropping before the user
/// has picked a screen — the future is dropped mid-`.await` and the session (and its
/// on-screen picker) would otherwise linger forever. Held across the fallible body
/// of `start`; on success it's defused (the session is moved into [`WaylandCapture`],
/// whose own `stop()`/`Drop` then owns the close).
struct SessionCloseGuard(Option<Session<'static, Screencast<'static>>>);

impl Drop for SessionCloseGuard {
	fn drop(&mut self) {
		if let Some(session) = self.0.take() {
			// `close()` is async (D-Bus); fire-and-forget on the current runtime so
			// the picker dialog is dismissed without blocking the drop.
			if let Ok(handle) = tokio::runtime::Handle::try_current() {
				handle.spawn(async move {
					let _ = session.close().await;
				});
			}
		}
	}
}

/// Start a portal screencast and pipe the screen to `udp://ip:port` as RTP.
/// `encoder_fragment` is a prebuilt gst encode→parse→rtp-payload fragment from
/// [`crate::pipeline::gst::encoder_fragment`] — the codec/encoder choice (and thus
/// what the client's SDP must declare) is the CALLER's, made against its validated
/// gst caps. Shows the compositor's share dialog the first time; pass a stored
/// `restore_token` to skip it on later calls. Returns the running capture and a
/// (possibly new) restore token to persist.
///
/// Retries a few times on startup failure: a re-stream can race the compositor's
/// teardown of a just-closed cast, so the first `pipewiresrc` fails READY→PAUSED;
/// a short backoff lets KWin settle and the token-restored retry succeeds without a
/// dialog. Callers therefore get a live capture or a hard error — never a dead child.
pub async fn start(
	ip: &str,
	port: u16,
	encoder_fragment: &str,
	restore_token: Option<String>,
) -> anyhow::Result<(WaylandCapture, Option<String>)> {
	// Open the portal ONCE. `select_sources`/`start` is what shows the compositor's
	// picker, so it must happen a single time — the previous design retried the WHOLE
	// attempt (portal + gst) and, because the fresh restore-token from a gst-failed
	// attempt was discarded, the retry re-prompted: the user saw TWO pickers. The only
	// thing that actually needs retrying is the gst startup race (a re-stream racing the
	// compositor's teardown of a just-closed cast → first `pipewiresrc` fails
	// READY→PAUSED), which we now retry on the SAME portal node below.
	let (mut guard, node_id, pw_fd, token) = open_portal(restore_token).await?;

	let mut last_err: Option<anyhow::Error> = None;
	for attempt in 0u32..3 {
		if attempt > 0 {
			// Backoff lets KWin finish tearing down the prior cast before we re-arm gst
			// on the same (already-picked) node — no second dialog.
			tokio::time::sleep(std::time::Duration::from_millis(300 * attempt as u64)).await;
		}
		match spawn_gst_verified(pw_fd.as_raw_fd(), node_id, encoder_fragment, ip, port).await {
			Ok(child) => {
				// Success: defuse the guard — ownership of the session passes to
				// WaylandCapture (its stop()/Drop closes it on teardown).
				let session = guard.0.take().expect("session present on success");
				return Ok((
					WaylandCapture {
						child,
						session: Some(session),
						_pw_fd: pw_fd,
					},
					token,
				));
			}
			Err(e) => {
				tracing::warn!(attempt, "gst pipeline start failed, retrying: {e}");
				last_err = Some(e);
			}
		}
	}
	// Every gst attempt failed — the guard closes the portal session on drop.
	Err(last_err.unwrap_or_else(|| anyhow::anyhow!("wayland capture start failed")))
}

/// Open the portal ScreenCast session: pick sources (shows the compositor's picker the
/// first time; a stored `restore_token` skips it), start the cast, and open the
/// PipeWire remote fd. Returns the still-open session — wrapped in a [`SessionCloseGuard`]
/// so it's closed on any early return — plus the node id, the fd, and a restore token to
/// persist. The caller defuses the guard once it owns a live capture.
async fn open_portal(
	restore_token: Option<String>,
) -> anyhow::Result<(SessionCloseGuard, u32, OwnedFd, Option<String>)> {
	let proxy: Screencast<'static> = Screencast::new().await?;
	let session: Session<'static, Screencast<'static>> = proxy.create_session().await?;
	// Everything past `create_session` can fail with the portal cast already live
	// (e.g. gstreamer not installed) OR be CANCELLED mid-picker (the connection
	// dropped before the user chose a screen). ashpd does NOT close the session on
	// drop, so either case would leave the compositor's picker / "you're sharing"
	// state up with no stream behind it. The guard closes the session on every exit
	// except success, where it's defused and ownership passes to `WaylandCapture`.
	let mut guard = SessionCloseGuard(Some(session));
	let (node_id, pw_fd, token) = async {
		let session = guard.0.as_ref().expect("session present");
		proxy
			.select_sources(
				session,
				CursorMode::Embedded,
				SourceType::Monitor | SourceType::Window,
				false,
				restore_token.as_deref(),
				PersistMode::Application,
			)
			.await?;
		let response = proxy
			.start(session, &WindowIdentifier::default())
			.await?
			.response()?;
		let stream = response
			.streams()
			.first()
			.ok_or_else(|| anyhow::anyhow!("portal returned no screencast stream"))?;
		let node_id = stream.pipe_wire_node_id();
		let token = response.restore_token().map(|s| s.to_string());

		let pw_fd: OwnedFd = proxy.open_pipe_wire_remote(session).await?;
		clear_cloexec(pw_fd.as_raw_fd())?;
		Ok::<_, anyhow::Error>((node_id, pw_fd, token))
	}
	.await?;
	Ok((guard, node_id, pw_fd, token))
}

/// Spawn `gst-launch` for a portal node + fd and confirm the pipeline reached PAUSED.
/// Returns the live child, or an Err if gst died at startup — the caller retries on the
/// SAME node (the failure is the compositor still tearing down a prior cast, not a bad
/// pick), so retrying never re-opens the picker.
async fn spawn_gst_verified(
	fd: i32,
	node_id: u32,
	encoder_fragment: &str,
	ip: &str,
	port: u16,
) -> anyhow::Result<Child> {
	// Latency: the builder's `leaky=downstream` queue drops stale frames if the encoder
	// can't keep up with the monitor's refresh, so end-to-end lag stays bounded
	// (effective fps drops instead of latency growing).
	let pipeline = crate::pipeline::gst::wayland_pipeline(fd, node_id, encoder_fragment, ip, port);
	let mut cmd = std::process::Command::new("gst-launch-1.0");
	cmd.arg("-q").args(pipeline.split_whitespace());
	// Die if our process dies, so an orphaned gst-launch never keeps the screen
	// "being shared" (KDE tray) after the app/session goes away.
	unsafe {
		cmd.pre_exec(|| {
			// SAFETY: async-signal-safe libc calls only.
			libc::prctl(
				libc::PR_SET_PDEATHSIG,
				libc::SIGKILL as libc::c_ulong,
				0,
				0,
				0,
			);
			if libc::getppid() == 1 {
				libc::_exit(0); // parent already gone between fork and here
			}
			Ok(())
		});
	}
	let mut child = cmd.spawn().map_err(|e| {
		anyhow::anyhow!("gst-launch-1.0 başlatılamadı (gstreamer kurulu mu?): {e}")
	})?;
	// Spawn success is NOT stream success: gst-launch prints "Failed to set pipeline to
	// PAUSED." and exits immediately when `pipewiresrc` can't open the portal node — e.g. a
	// restart racing the compositor's teardown of the previous cast. Poll briefly so an
	// early death surfaces as an Err (the caller retries / the guard closes the session)
	// instead of storing a dead child + reporting success (permanently black video). gst
	// streams during this window, so it adds no first-frame latency.
	for _ in 0..14 {
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
		match child.try_wait() {
			Ok(Some(status)) => {
				return Err(anyhow::anyhow!(
					"gst-launch exited at startup (status {status}) — pipeline failed to reach PAUSED (pipewire/portal node not ready?)"
				));
			}
			Ok(None) => {}
			Err(e) => return Err(anyhow::anyhow!("gst-launch wait failed: {e}")),
		}
	}
	Ok(child)
}
