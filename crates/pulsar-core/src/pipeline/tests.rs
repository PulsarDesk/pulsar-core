use super::*;

fn plan(enc: HwEncoder) -> StreamPlan {
	StreamPlan {
		encoder: enc,
		codec: VCodec::H264,
		width: 1920,
		height: 1080,
		fps: 60,
		bitrate_kbps: 30_000,
		capture: CaptureMethod::X11grab,
		display: ":0.0".into(),
		vaapi_device: "/dev/dri/renderD128".into(),
		dest: "rtp://10.0.0.5:9000".into(),
		low_latency: true,
		gpu_zerocopy: false,
		output_idx: 0,
		capture_size: None,
		hdr: false,
		yuv444: false,
		loss_recovery: crate::service::LossRecovery::Normal,
		window: None,
	}
}

/// Extract the trailing `format=<fmt>` token from a ddagrab filter string.
/// Returns `None` if there is no `format=` suffix in the filter value.
fn ddagrab_filter_fmt(args: &[String]) -> Option<String> {
	// ddagrab passes its filter via -filter_complex <value>
	let fc_pos = args.iter().position(|a| a == "-filter_complex")?;
	let filter = args.get(fc_pos + 1)?;
	// The trailing format step looks like "...,format=nv12" or "...:format=nv12"
	// For scale_cuda and scale_qsv the format is a colon-separated key=value.
	// For the CPU-bounce path and AMF it is a comma-separated filter.
	// We extract the last `format=<token>` occurrence.
	filter
		.split([',', ':'])
		.filter_map(|seg| seg.strip_prefix("format="))
		.last()
		.map(|s| s.to_string())
}

/// Extract `-pix_fmt <value>` from an arg list; returns `None` when absent.
fn cmd_pix_fmt(args: &[String]) -> Option<String> {
	let pos = args.iter().position(|a| a == "-pix_fmt")?;
	args.get(pos + 1).cloned()
}

/// For ddagrab GPU paths (NVENC/QSV) the filter format and -pix_fmt must agree
/// (or -pix_fmt must be absent, meaning the encoder receives whatever the filter
/// outputs and ffmpeg does not try to convert).
///
/// The core contract: if command.rs emits `-pix_fmt X`, the ddagrab filter MUST
/// end in `format=X` (or a compatible alias).  If no `-pix_fmt` is emitted, the
/// filter format is the encoder's native input and no conversion is needed.
#[test]
fn ddagrab_filter_format_matches_command_pixfmt_for_all_encoders() {
	// Helper: build a ddagrab plan for a given encoder + hdr/yuv444 combo.
	let ddagrab_plan = |enc: HwEncoder, hdr: bool, yuv444: bool, zerocopy: bool| -> StreamPlan {
		StreamPlan {
			encoder: enc,
			codec: VCodec::H265, // H265 exercises the most profile/pix_fmt branches
			capture: CaptureMethod::Ddagrab,
			hdr,
			yuv444,
			gpu_zerocopy: zerocopy,
			..plan(enc)
		}
	};

	// Cases: (encoder, hdr, yuv444, zerocopy)
	let cases: &[(HwEncoder, bool, bool, bool)] = &[
		// NVENC zero-copy: GPU path, must degrade to SDR 4:2:0 (no -pix_fmt emitted,
		// filter ends with format=nv12 — nv12 is the native NVENC input).
		(HwEncoder::Nvenc, true, false, true),
		(HwEncoder::Nvenc, false, true, true),
		(HwEncoder::Nvenc, true, true, true),
		(HwEncoder::Nvenc, false, false, true),
		// NVENC hybrid: BGRA readback → scale_cuda format=nv12 (SDR 4:2:0), no -pix_fmt.
		(HwEncoder::Nvenc, true, false, false),
		(HwEncoder::Nvenc, false, true, false),
		(HwEncoder::Nvenc, true, true, false),
		(HwEncoder::Nvenc, false, false, false),
		// QSV: GPU path, must degrade to SDR 4:2:0 (scale_qsv format=nv12, no -pix_fmt).
		(HwEncoder::Qsv, true, false, false),
		(HwEncoder::Qsv, false, true, false),
		(HwEncoder::Qsv, true, true, false),
		(HwEncoder::Qsv, false, false, false),
		// AMF: CPU-bounce path, honors hdr/yuv444 — filter fmt must equal -pix_fmt.
		(HwEncoder::Amf, true, false, false),
		(HwEncoder::Amf, false, true, false),
		(HwEncoder::Amf, true, true, false),
		(HwEncoder::Amf, false, false, false),
		// Media Foundation: CPU-bounce but NV12-only, must degrade to SDR 4:2:0.
		(HwEncoder::MediaFoundation, true, false, false),
		(HwEncoder::MediaFoundation, false, true, false),
		(HwEncoder::MediaFoundation, false, false, false),
		// Software: CPU-bounce, honors hdr/yuv444.
		(HwEncoder::Software, true, false, false),
		(HwEncoder::Software, false, false, false),
	];

	for &(enc, hdr, yuv444, zerocopy) in cases {
		let p = ddagrab_plan(enc, hdr, yuv444, zerocopy);
		let (_, args) = encode_command(&p);
		let filter_fmt = ddagrab_filter_fmt(&args);
		let pix_fmt = cmd_pix_fmt(&args);

		let label = format!("{enc:?} hdr={hdr} yuv444={yuv444} zerocopy={zerocopy}");

		match pix_fmt {
			Some(ref pf) => {
				// -pix_fmt is present: the filter MUST end in a matching format=.
				let ff = filter_fmt.as_deref().unwrap_or("<none>");
				assert_eq!(
					ff, pf.as_str(),
					"[{label}] filter format `{ff}` != -pix_fmt `{pf}`"
				);
			}
			None => {
				// No -pix_fmt: the filter format is the encoder's native input; that
				// is fine as long as the encoder can accept it (nv12 for NVENC/QSV).
				// No further assertion needed — the absence of -pix_fmt means ffmpeg
				// will accept whatever the filter emits.
			}
		}
	}
}

/// NVENC/QSV ddagrab: even when plan.hdr=true or plan.yuv444=true, the effective
/// values must be clamped and no HDR signaling or 10-bit -pix_fmt may appear.
#[test]
fn ddagrab_nvenc_qsv_degrade_hdr_yuv444_to_sdr_420() {
	for enc in [HwEncoder::Nvenc, HwEncoder::Qsv, HwEncoder::MediaFoundation] {
		for (hdr, yuv444) in [(true, false), (false, true), (true, true)] {
			let p = StreamPlan {
				encoder: enc,
				codec: VCodec::H265,
				capture: CaptureMethod::Ddagrab,
				hdr,
				yuv444,
				gpu_zerocopy: matches!(enc, HwEncoder::Nvenc), // exercise zerocopy too
				..plan(enc)
			};
			let (_, args) = encode_command(&p);
			let joined = args.join(" ");
			assert!(
				!joined.contains("p010le"),
				"[{enc:?} hdr={hdr} yuv444={yuv444}] must not emit p010le (degrades to SDR)"
			);
			assert!(
				!joined.contains("yuv444"),
				"[{enc:?} hdr={hdr} yuv444={yuv444}] must not emit yuv444 (degrades to 4:2:0)"
			);
			assert!(
				!joined.contains("smpte2084"),
				"[{enc:?} hdr={hdr} yuv444={yuv444}] must not emit HDR colorspace signaling"
			);
			assert!(
				!joined.contains("bt2020"),
				"[{enc:?} hdr={hdr} yuv444={yuv444}] must not emit BT2020 primaries"
			);
			// Check effective_hdr_yuv444 directly.
			assert_eq!(
				p.effective_hdr_yuv444(),
				(false, false),
				"[{enc:?} hdr={hdr} yuv444={yuv444}] effective_hdr_yuv444 must be (false,false)"
			);
		}
	}
}

/// AMF/Software ddagrab: yuv444 is NOT degraded — it flows through to both the filter
/// format and -pix_fmt, and they must agree. HDR is dropped: ddagrab hands over 8-bit BGRA,
/// which is SDR (no P010, no PQ tags).
#[test]
fn ddagrab_cpu_bounce_encoders_honor_yuv444_but_not_fake_hdr() {
	// AMF + HDR request: the 8-bit BGRA capture stays SDR NV12, untagged.
	let p = StreamPlan {
		encoder: HwEncoder::Amf,
		codec: VCodec::H265,
		capture: CaptureMethod::Ddagrab,
		hdr: true,
		yuv444: false,
		..plan(HwEncoder::Amf)
	};
	assert_eq!(p.effective_hdr_yuv444(), (false, false));
	let (_, args) = encode_command(&p);
	let joined = args.join(" ");
	assert!(joined.contains("format=nv12"), "AMF ddagrab: SDR nv12 filter, got {joined}");
	assert!(!joined.contains("p010le"), "AMF ddagrab: no 10-bit re-quantisation of SDR");
	assert!(!joined.contains("smpte2084") && !joined.contains("bt2020"), "AMF ddagrab: no fake PQ");

	// Software + yuv444: filter must end in format=yuv444p, command -pix_fmt yuv444p.
	let q = StreamPlan {
		encoder: HwEncoder::Software,
		codec: VCodec::H264,
		capture: CaptureMethod::Ddagrab,
		hdr: false,
		yuv444: true,
		..plan(HwEncoder::Software)
	};
	assert_eq!(q.effective_hdr_yuv444(), (false, true));
	let (_, args) = encode_command(&q);
	let joined = args.join(" ");
	assert!(
		joined.contains("format=yuv444p"),
		"Software ddagrab yuv444: filter must end in format=yuv444p"
	);
	assert!(
		joined.contains("-pix_fmt yuv444p"),
		"Software ddagrab yuv444: command must have -pix_fmt yuv444p"
	);
}

/// Media Foundation on ddagrab: the Qualcomm MFT rejects D3D11 frames and yuv420p, so
/// the filter must download and end in nv12 (the old default branch sent yuv420p and
/// every Snapdragon host fell back to libx264).
#[test]
fn ddagrab_media_foundation_feeds_nv12() {
	for codec in [VCodec::H264, VCodec::H265] {
		let p = StreamPlan {
			encoder: HwEncoder::MediaFoundation,
			codec,
			capture: CaptureMethod::Ddagrab,
			..plan(HwEncoder::MediaFoundation)
		};
		let (_, args) = encode_command(&p);
		assert_eq!(ddagrab_filter_fmt(&args).as_deref(), Some("nv12"), "{codec:?}");
		let joined = args.join(" ");
		assert!(joined.contains("hwdownload"), "{codec:?}: MF needs a system-memory frame");
		assert!(!joined.contains("yuv420p"), "{codec:?}: yuv420p fails MF format negotiation");
	}
}

#[test]
fn fit_window_size_keeps_aspect_even_and_never_upscales() {
	use super::fit_window_size;
	assert_eq!(fit_window_size(1280, 720, 1920, 1080), (1280, 720));
	assert_eq!(fit_window_size(2560, 1440, 1920, 1080), (1920, 1080));
	assert_eq!(fit_window_size(801, 601, 1920, 1080), (800, 600));
	// A tall window into a wide box: the height binds.
	assert_eq!(fit_window_size(1000, 2000, 1920, 1080), (540, 1080));
	assert_eq!(fit_window_size(0, 0, 1920, 1080), (2, 2));
}

#[test]
fn capture_backends_emit_the_right_input() {
	let mut p = plan(HwEncoder::Software);
	p.capture = CaptureMethod::X11grab;
	assert!(encode_command(&p).1.iter().any(|a| a == "x11grab"));
	p.capture = CaptureMethod::Gdigrab;
	let (_, args) = encode_command(&p);
	assert!(args.iter().any(|a| a == "gdigrab"));
	assert!(args.iter().any(|a| a == "desktop"));
	// Never a fixed capture size (fails when the desktop is smaller); scaled instead.
	assert!(!args.iter().any(|a| a == "-video_size"));
	assert!(args.iter().any(|a| a.starts_with("scale=")));
	p.capture = CaptureMethod::AvFoundation;
	assert!(encode_command(&p).1.iter().any(|a| a == "avfoundation"));
	p.capture = CaptureMethod::Kmsgrab;
	assert!(encode_command(&p).1.iter().any(|a| a == "kmsgrab"));
	// DXGI Desktop Duplication is a filter source, not a `-f`/`-i` input.
	p.capture = CaptureMethod::Ddagrab;
	let (_, args) = encode_command(&p);
	assert!(args.iter().any(|a| a == "-filter_complex"));
	assert!(args.iter().any(|a| a.contains("ddagrab")));
	assert!(!args.iter().any(|a| a == "-i")); // no input file/device for ddagrab
}

#[test]
fn windows_default_capture_is_dxgi() {
	// Windows hosts default to DXGI Desktop Duplication, not legacy gdigrab.
	if cfg!(target_os = "windows") {
		assert_eq!(CaptureMethod::default_for_os(), CaptureMethod::Ddagrab);
	}
}

#[test]
fn encoder_names_per_codec() {
	assert_eq!(
		HwEncoder::Nvenc.ffmpeg_name(VCodec::H265),
		Some("hevc_nvenc")
	);
	assert_eq!(
		HwEncoder::Vaapi.ffmpeg_name(VCodec::H264),
		Some("h264_vaapi")
	);
	assert_eq!(
		HwEncoder::Software.ffmpeg_name(VCodec::Av1),
		Some("libsvtav1")
	);
	assert_eq!(HwEncoder::Auto.ffmpeg_name(VCodec::H264), None);
}

#[test]
fn detect_parses_encoder_list() {
	let out = " V..... h264_nvenc\n V..... h264_vaapi\n V..... libx264\n";
	let got = detect(out);
	assert!(got.contains(&HwEncoder::Nvenc));
	assert!(got.contains(&HwEncoder::Vaapi));
	assert!(got.contains(&HwEncoder::Software));
	assert!(!got.contains(&HwEncoder::Qsv));
}

#[test]
fn detect_registers_vendor_on_any_codec_not_just_h264() {
	// An HEVC/AV1-only build (no h264_amf) must still register AMF.
	let out = " V..... hevc_amf\n V..... av1_qsv\n V..... libx265\n";
	let got = detect(out);
	assert!(
		got.contains(&HwEncoder::Amf),
		"hevc_amf alone must register AMF"
	);
	assert!(
		got.contains(&HwEncoder::Qsv),
		"av1_qsv alone must register QSV"
	);
	assert!(
		got.contains(&HwEncoder::Software),
		"libx265 alone must register Software"
	);
}

#[test]
fn available_codecs_intersects_with_ffmpeg() {
	let out = " V..... h264_nvenc\n V..... av1_nvenc\n"; // no hevc_nvenc
	let c = HwEncoder::Nvenc.available_codecs(out);
	assert!(c.contains(&VCodec::H264) && c.contains(&VCodec::Av1));
	assert!(
		!c.contains(&VCodec::H265),
		"missing hevc_nvenc must not be advertised"
	);
	// VideoToolbox never advertises AV1 even if the name somehow appears.
	assert!(!HwEncoder::VideoToolbox.all_codecs().contains(&VCodec::Av1));
}

#[test]
fn resolve_codec_honors_then_falls_back() {
	let full = " h264_nvenc hevc_nvenc av1_nvenc ";
	// requested available → honored
	assert_eq!(
		resolve_codec(HwEncoder::Nvenc, VCodec::Av1, full),
		VCodec::Av1
	);
	// requested missing → prefer H.264 (webview-decodable)
	let only_h264 = " h264_nvenc ";
	assert_eq!(
		resolve_codec(HwEncoder::Nvenc, VCodec::H265, only_h264),
		VCodec::H264
	);
	// no H.264 but HEVC present → first available
	let only_hevc = " hevc_nvenc ";
	assert_eq!(
		resolve_codec(HwEncoder::Nvenc, VCodec::Av1, only_hevc),
		VCodec::H265
	);
}

/// Every ffmpeg capture input is 8-bit SDR: an HDR request must never produce PQ / BT.2020
/// tags or a 10-bit re-quantisation, on any OS's capture or any encoder.
#[test]
fn hdr_request_never_tags_an_8bit_capture_as_pq() {
	let captures = [
		CaptureMethod::X11grab,
		CaptureMethod::Kmsgrab,
		CaptureMethod::Gdigrab,
		CaptureMethod::Ddagrab,
		CaptureMethod::AvFoundation,
	];
	let encoders = [
		HwEncoder::Nvenc,
		HwEncoder::Amf,
		HwEncoder::Qsv,
		HwEncoder::Vaapi,
		HwEncoder::Vulkan,
		HwEncoder::VideoToolbox,
		HwEncoder::MediaFoundation,
		HwEncoder::Software,
	];
	for capture in captures {
		for enc in encoders {
			for codec in [VCodec::H264, VCodec::H265, VCodec::Av1] {
				let mut p = plan(enc);
				p.capture = capture;
				p.codec = codec;
				p.hdr = true;
				assert!(!p.capture_delivers_hdr());
				assert!(!p.effective_hdr_yuv444().0, "{capture:?} {enc:?} {codec:?}");
				let j = encode_command(&p).1.join(" ");
				for bad in ["smpte2084", "bt2020", "p010le", "main10", "high10", "yuv444p10le"] {
					assert!(!j.contains(bad), "{capture:?} {enc:?} {codec:?} emitted {bad}: {j}");
				}
			}
		}
	}
}

#[test]
fn hdr_and_yuv444_set_pixfmt_profile_colorspace() {
	// An HDR request on an 8-bit capture: plain SDR (no 10-bit, no PQ).
	let mut p = plan(HwEncoder::Nvenc);
	p.codec = VCodec::H265;
	p.hdr = true;
	let j = encode_command(&p).1.join(" ");
	assert!(!j.contains("-pix_fmt p010le"));
	assert!(!j.contains("-profile:v main10"));
	assert!(!j.contains("smpte2084") && !j.contains("bt2020"));
	// YUV444 H.264: high444p + yuv444p, no HDR colorspace.
	let mut q = plan(HwEncoder::Software);
	q.codec = VCodec::H264;
	q.yuv444 = true;
	let j = encode_command(&q).1.join(" ");
	assert!(j.contains("-profile:v high444p"));
	assert!(j.contains("-pix_fmt yuv444p"));
	assert!(!j.contains("smpte2084"));
	// SDR 4:2:0 path unchanged (no pix_fmt override).
	let j = encode_command(&plan(HwEncoder::Nvenc)).1.join(" ");
	assert!(!j.contains("-pix_fmt"));
}

#[test]
fn probe_command_is_one_frame_to_null() {
	let (prog, args) = probe_command(HwEncoder::Nvenc, VCodec::Av1, "/dev/dri/renderD128").unwrap();
	assert_eq!(prog, "ffmpeg");
	let j = args.join(" ");
	assert!(j.contains("testsrc"), "uses a synthetic source");
	assert!(j.contains("-frames:v 1"), "exactly one frame");
	assert!(j.contains("av1_nvenc"));
	assert!(j.ends_with("null -"), "discards output");
	// VA-API probe declares the device + uploads.
	let (_, va) = probe_command(HwEncoder::Vaapi, VCodec::H265, "/dev/dri/renderD128").unwrap();
	let j = va.join(" ");
	assert!(j.contains("-vaapi_device /dev/dri/renderD128"));
	assert!(j.contains("hwupload"));
	// Unsupported combo → None.
	assert!(probe_command(HwEncoder::VideoToolbox, VCodec::Av1, "").is_none());
	// Media Foundation: NV12 input + the hardware MFT, like the stream itself.
	let (_, mf) = probe_command(HwEncoder::MediaFoundation, VCodec::H265, "").unwrap();
	let j = mf.join(" ");
	assert!(j.contains("-vf format=nv12 -c:v hevc_mf -hw_encoding 1"), "{j}");
}

#[test]
fn new_backends_have_names_and_labels() {
	assert_eq!(
		HwEncoder::Vulkan.ffmpeg_name(VCodec::Av1),
		Some("av1_vulkan")
	);
	assert_eq!(
		HwEncoder::MediaFoundation.ffmpeg_name(VCodec::H265),
		Some("hevc_mf")
	);
	assert_eq!(HwEncoder::Vulkan.label(), "Vulkan");
	// Vulkan declares all three codecs.
	assert_eq!(HwEncoder::Vulkan.all_codecs().len(), 3);
	// detect registers them from their listed names.
	let got = detect(" h264_vulkan hevc_mf libx264 ");
	assert!(got.contains(&HwEncoder::Vulkan));
	assert!(got.contains(&HwEncoder::MediaFoundation));
}

#[test]
fn vaapi_and_videotoolbox_have_tune_args() {
	let mut v = plan(HwEncoder::Vaapi);
	v.low_latency = true;
	let args = encode_command(&v).1.join(" ");
	assert!(args.contains("-rc_mode CBR"), "vaapi game mode = CBR");
	let mut t = plan(HwEncoder::VideoToolbox);
	t.low_latency = true;
	assert!(encode_command(&t).1.iter().any(|a| a == "-realtime"));
}

#[test]
fn resolve_prefers_best_then_falls_back() {
	let avail = [HwEncoder::Vaapi, HwEncoder::Software];
	// manual choice honored when available
	assert_eq!(resolve(HwEncoder::Vaapi, &avail), HwEncoder::Vaapi);
	// unavailable manual choice → best available
	assert_eq!(resolve(HwEncoder::Nvenc, &avail), HwEncoder::Vaapi);
	// auto → best available
	assert_eq!(resolve(HwEncoder::Auto, &avail), HwEncoder::Vaapi);
	// nothing → software
	assert_eq!(resolve(HwEncoder::Auto, &[]), HwEncoder::Software);
}

#[test]
fn nvenc_command_invokes_ffmpeg_directly_with_encoder() {
	let (program, args) = encode_command(&plan(HwEncoder::Nvenc));
	assert_eq!(program, "ffmpeg"); // no prime-run / wrapper — ffmpeg directly
	assert!(args.iter().any(|a| a == "h264_nvenc"));
	assert!(args.iter().any(|a| a == "x11grab"));
	assert!(args.iter().any(|a| a == "ull")); // low-latency tune
	assert!(args.iter().any(|a| a == "rtp")); // RTP output for WebCodecs
	assert!(args.last().unwrap().starts_with("rtp://"));
	// Packet size capped for 1280-byte overlay / relay MTUs (ffmpeg's default is 1472).
	assert!(args.last().unwrap().ends_with(&format!("?pkt_size={}", crate::service::media::RTP_MAX_PACKET)));
}

#[test]
fn vaapi_command_sets_device_and_upload() {
	let (program, args) = encode_command(&plan(HwEncoder::Vaapi));
	assert_eq!(program, "ffmpeg");
	assert!(args.iter().any(|a| a == "-vaapi_device"));
	assert!(args.iter().any(|a| a == "/dev/dri/renderD128"));
	// The upload's NV12 comes from an explicit BT.709 limited conversion, then the SDR tag.
	assert!(
		args.iter().any(|a| a
			== "scale=out_color_matrix=bt709:out_range=tv,format=nv12,hwupload,setparams=color_primaries=bt709:color_trc=bt709"),
		"{args:?}"
	);
	assert!(args.iter().any(|a| a == "h264_vaapi"));
}

#[test]
fn profile_picks_latency_vs_quality_params() {
	// Game (low-latency): ultrafast + short GOP.
	let mut p = plan(HwEncoder::Software);
	p.low_latency = true;
	p.fps = 60;
	let (_, game) = encode_command(&p);
	assert!(game.iter().any(|a| a == "ultrafast"));
	// GOP ~ fps/4 = 15 follows the -g flag.
	let gi = game.iter().position(|a| a == "-g").unwrap();
	assert_eq!(game[gi + 1], "15");

	// Desktop (quality): veryfast + a longer GOP — 0.5 s at this 30 Mbit/s (fps/2 = 30),
	// 2 s on a thin link (fps*2 = 120).
	p.low_latency = false;
	let (_, desk) = encode_command(&p);
	assert!(desk.iter().any(|a| a == "veryfast"));
	let gi = desk.iter().position(|a| a == "-g").unwrap();
	assert_eq!(desk[gi + 1], "30");
	p.bitrate_kbps = 1500;
	let (_, thin) = encode_command(&p);
	let gi = thin.iter().position(|a| a == "-g").unwrap();
	assert_eq!(thin[gi + 1], "120");
	p.bitrate_kbps = 30_000;

	// NVENC: ull tune for games, a higher preset for the desktop.
	let mut n = plan(HwEncoder::Nvenc);
	n.low_latency = true;
	assert!(encode_command(&n).1.iter().any(|a| a == "ull"));
	n.low_latency = false;
	let (_, nq) = encode_command(&n);
	assert!(nq.iter().any(|a| a == "p5"));
	assert!(nq.iter().any(|a| a == "-spatial-aq"));
}

#[test]
fn ddagrab_is_fully_gpu_per_encoder() {
	// NVENC + display on the NVIDIA GPU: zero-copy D3D11→CUDA→NVENC, no hwdownload.
	let mut p = plan(HwEncoder::Nvenc);
	p.capture = CaptureMethod::Ddagrab;
	p.gpu_zerocopy = true;
	let f = encode_command(&p).1.join(" ");
	assert!(f.contains("hwmap=derive_device=cuda"));
	assert!(f.contains("scale_cuda"));
	assert!(
		!f.contains("hwdownload"),
		"zero-copy must not round-trip to CPU"
	);

	// NVENC hybrid (display on iGPU, CUDA map unavailable): NVENC cannot open a session on
	// the iGPU's D3D11 device ("no encode device", measured 2026-11-15), so the frame is read
	// back once as BGRA and scaled + converted on the NVIDIA GPU — never a CPU swscale (the
	// old swscale round trip capped ~51 fps on a 3080 laptop).
	p.gpu_zerocopy = false;
	let f = encode_command(&p).1.join(" ");
	assert!(
		f.contains("hwdownload,format=bgra,hwupload_cuda,scale_cuda=1920:1080:format=nv12,fps=60"),
		"hybrid NVENC goes through CUDA: {f}"
	);
	assert!(!f.contains(",scale=") && !f.contains("-pix_fmt"), "no CPU scaler / pix_fmt: {f}");

	// Hybrid, capture size EQUAL to the request: the capture's own size (scale_cuda only
	// converts to NV12).
	p.capture_size = Some((1920, 1080));
	let f = encode_command(&p).1.join(" ");
	assert!(f.contains("scale_cuda=1920:1080:format=nv12"), "{f}");

	// Hybrid asked for a SMALLER size than the capture's: honoured, scaled on the NVIDIA GPU.
	p.capture_size = Some((2560, 1600));
	let f = encode_command(&p).1.join(" ");
	assert!(
		f.contains("hwdownload,format=bgra,hwupload_cuda,scale_cuda=1920:1080:format=nv12,fps=60"),
		"hybrid must scale to the requested size: {f}"
	);
	assert!(!f.contains(",scale=") && !f.contains("-pix_fmt"), "no CPU scaler / pix_fmt: {f}");

	// Hybrid asked for a LARGER size than the capture (a 1440p client, a 1080p panel): never
	// upscaled — clamped to the capture per axis, as the native DXGI path does.
	p.capture_size = Some((1920, 1080));
	p.width = 2560;
	p.height = 1440;
	let f = encode_command(&p).1.join(" ");
	assert!(f.contains("scale_cuda=1920:1080:format=nv12") && !f.contains("2560"), "{f}");
	// One axis larger, one smaller: each axis clamped on its own (and evened).
	p.capture_size = Some((1366, 768));
	p.width = 1280;
	p.height = 1080;
	let f = encode_command(&p).1.join(" ");
	assert!(f.contains("scale_cuda=1280:768:format=nv12"), "{f}");
	p.capture_size = Some((1365, 767));
	p.width = 1920;
	p.height = 1080;
	let f = encode_command(&p).1.join(" ");
	assert!(f.contains("scale_cuda=1364:766:format=nv12"), "odd capture is evened: {f}");
	p.capture_size = None;

	// AMD AMF: NV12, no yuv420p swscale.
	let mut a = plan(HwEncoder::Amf);
	a.capture = CaptureMethod::Ddagrab;
	let f = encode_command(&a).1.join(" ");
	assert!(f.contains("h264_amf") && f.contains("format=nv12"));

	// Software still uses the CPU yuv420p path (libx264 needs it).
	let mut s = plan(HwEncoder::Software);
	s.capture = CaptureMethod::Ddagrab;
	assert!(encode_command(&s).1.join(" ").contains("format=yuv420p"));
}

#[test]
fn software_command_is_zerolatency() {
	let (program, args) = encode_command(&plan(HwEncoder::Software));
	assert_eq!(program, "ffmpeg");
	assert!(args.iter().any(|a| a == "libx264"));
	assert!(args.iter().any(|a| a == "zerolatency"));
	// SDR software encode pins 4:2:0 — without it libx264 matched the BGR capture to
	// High 4:4:4, which the client's HW/4:2:0 decode paths can't play (black screen).
	assert!(args.join(" ").contains("-pix_fmt yuv420p"));
}

#[test]
fn decode_command_is_low_latency_ffplay() {
	let (program, args) = decode_command("udp://@:9000");
	assert_eq!(program, "ffplay");
	assert!(args.iter().any(|a| a == "nobuffer"));
	assert!(args.iter().any(|a| a == "low_delay"));
	assert_eq!(args.last().unwrap(), "udp://@:9000");
}

#[test]
fn gst_fragment_mpp_h265_matches_verified_pi_pipeline() {
	// Property names verified live on an Orange Pi 5 (gst 1.20 rockchipmpp plugin).
	let f = gst::encoder_fragment(gst::GstEncoder::Mpp, VCodec::H265, 8000, 60).unwrap();
	assert!(f.contains("mpph265enc name=venc bps=8000000 gop=60 header-mode=each-idr"));
	assert!(f.contains("h265parse"));
	assert!(f.contains(&format!(
		"rtph265pay config-interval=1 pt=96 mtu={}",
		crate::service::media::RTP_MAX_PACKET
	)));
}

#[test]
fn gst_fragment_va_matches_verified_pipeline() {
	// Property names verified live on an AMD Cezanne iGPU (gst 1.28 `va` plugin, Mesa).
	let f = gst::encoder_fragment(gst::GstEncoder::Va, VCodec::H265, 8000, 60).unwrap();
	assert!(f.contains("vah265enc name=venc rate-control=cbr bitrate=8000 key-int-max=60 b-frames=0"));
	assert!(f.contains("h265parse"));
	// One UI/wire entry for both VA-API plugins.
	assert_eq!(gst::GstEncoder::Va.wire_id(), gst::GstEncoder::Vaapi.wire_id());
}

#[test]
fn gst_fragment_rejects_impossible_pairs() {
	// Software HEVC stays off (same policy as the ffmpeg path); no AV1 elements modeled.
	assert!(gst::encoder_fragment(gst::GstEncoder::X264, VCodec::H265, 8000, 60).is_none());
	assert!(gst::encoder_fragment(gst::GstEncoder::Mpp, VCodec::Av1, 8000, 60).is_none());
}

#[test]
fn gst_pipelines_embed_fragment_and_low_latency_queue() {
	let f = gst::encoder_fragment(gst::GstEncoder::X264, VCodec::H264, 6000, 30).unwrap();
	let w = gst::wayland_pipeline(7, 42, &f, "10.0.0.2", 9000);
	assert!(w.contains("pipewiresrc fd=7 path=42"));
	assert!(w.contains("queue leaky=downstream"));
	assert!(w.contains("x264enc name=venc tune=zerolatency"));
	assert!(w.contains("udpsink host=10.0.0.2 port=9000 sync=false"));
	let x = gst::x11_pipeline(":0", 60, &f, "10.0.0.2", 9000, false, None);
	assert!(x.contains("ximagesrc display-name=:0 use-damage=0"));
	assert!(x.contains("framerate=60/1"));
	assert!(x.contains("videoconvert"), "non-MPP keeps the CPU convert");
	assert!(x.contains(&f));
	// MPP path: capture's BGRx goes STRAIGHT to the encoder (RGA converts inside) —
	// the CPU videoconvert that capped the Pi at ~49 fps must be gone.
	let xd = gst::x11_pipeline(":0", 120, &f, "10.0.0.2", 9000, true, None);
	assert!(xd.contains("format=BGRx,framerate=120/1"));
	assert!(!xd.contains("videoconvert"));
	assert!(xd.contains("queue leaky=downstream"));
	// Probe wraps the SAME fragment between a testsrc and a fakesink.
	let p = gst::probe_pipeline(&f);
	assert!(p.starts_with("videotestsrc num-buffers=2"));
	assert!(p.contains(&f) && p.ends_with("fakesink sync=false"));
}

#[test]
fn gst_kms_pipeline_is_zero_copy_and_vblank_paced() {
	let f = gst::encoder_fragment(gst::GstEncoder::Mpp, VCodec::H265, 12000, 120).unwrap();
	let k = gst::kms_pipeline(120, &f, "10.0.0.2", 9000);
	// The scanout DMABuf rides untouched into MPP: DMABuf caps feature, no X
	// capture, no CPU convert anywhere.
	assert!(k.starts_with("kmssrc driver-name=rockchip dma-feature=true sync-fb=false"));
	assert!(k.contains("video/x-raw(memory:DMABuf),format=BGRx,framerate=120/1"));
	assert!(!k.contains("videoconvert") && !k.contains("ximagesrc"));
	// sync-fb=false is load-bearing: the default waits for NEW pageflips and an
	// idle desktop then throttles to ~58 fps instead of the 120 Hz vblank.
	assert!(k.contains("sync-fb=false"));
	assert!(k.contains("queue leaky=downstream"));
	assert!(k.contains(&f));
	assert!(k.ends_with("udpsink host=10.0.0.2 port=9000 sync=false"));
	// fps=0 guards the division the same way the other builders do.
	assert!(gst::kms_pipeline(0, &f, "1.2.3.4", 1).contains("framerate=1/1"));
	// The KMS probe validates the REAL capture path (2 scanout frames), not a
	// videotestsrc stand-in — it must fail where GETFB2 privileges are missing.
	let p = gst::kms_probe_pipeline(&f);
	assert!(
		p.starts_with("kmssrc driver-name=rockchip dma-feature=true sync-fb=false num-buffers=2")
	);
	assert!(p.contains("(memory:DMABuf)"));
	assert!(p.contains(&f) && p.ends_with("fakesink sync=false"));
}

#[test]
fn rkmpp_encoder_is_detected_and_resolves_before_software() {
	// An ffmpeg-rockchip build lists the rkmpp encoders → the vendor registers and
	// auto-resolution prefers it over software on a GPU-less SBC.
	let out = " V..... h264_rkmpp\n V..... hevc_rkmpp\n V..... libx264\n";
	let avail = detect(out);
	assert!(avail.contains(&HwEncoder::Rkmpp));
	assert_eq!(resolve(HwEncoder::Auto, &avail), HwEncoder::Rkmpp);
	assert_eq!(
		HwEncoder::Rkmpp.available_codecs(out),
		vec![VCodec::H264, VCodec::H265]
	);
}

/// Phase 0 (loss recovery) on the gst fragments: `ShortGop` halves the key interval,
/// `IntraRefresh` enables x264enc's rolling refresh (other elements → short GOP), and the
/// plain `encoder_fragment` is byte-identical to the `Normal` mode (no regression).
#[test]
fn gst_fragment_loss_recovery_modes() {
	use crate::service::LossRecovery;
	use gst::{encoder_fragment, encoder_fragment_with, GstEncoder};
	let normal = encoder_fragment(GstEncoder::X264, VCodec::H264, 4000, 60).unwrap();
	assert_eq!(
		normal,
		encoder_fragment_with(GstEncoder::X264, VCodec::H264, 4000, 60, LossRecovery::Normal).unwrap()
	);
	assert!(normal.contains("key-int-max=60") && !normal.contains("intra-refresh"), "{normal}");
	let short = encoder_fragment_with(GstEncoder::X264, VCodec::H264, 4000, 60, LossRecovery::ShortGop).unwrap();
	assert!(short.contains("key-int-max=30") && !short.contains("intra-refresh"), "{short}");
	let ir = encoder_fragment_with(GstEncoder::X264, VCodec::H264, 4000, 60, LossRecovery::IntraRefresh).unwrap();
	assert!(ir.contains("key-int-max=30") && ir.contains("intra-refresh=true"), "{ir}");
	// Elements without a refresh knob: IntraRefresh degrades to the short key interval.
	let nv = encoder_fragment_with(GstEncoder::Nv, VCodec::H265, 4000, 60, LossRecovery::IntraRefresh).unwrap();
	assert!(nv.contains("gop-size=30") && !nv.contains("intra-refresh"), "{nv}");
	let mpp = encoder_fragment_with(GstEncoder::Mpp, VCodec::H264, 4000, 30, LossRecovery::IntraRefresh).unwrap();
	assert!(mpp.contains("gop=15") && !mpp.contains("intra-refresh"), "{mpp}");
}

/// One X11 window: `-window_id` instead of `-video_size`, the bare display, and ONE `-vf`
/// chain that scales the grab and (VA-API) uploads it — never two `-vf` flags.
#[test]
fn x11_window_capture_args() {
	let p = StreamPlan {
		window: Some(WindowInput::X11 { id: 0x3a0_0007 }),
		width: 1280,
		height: 720,
		..plan(HwEncoder::Software)
	};
	let args = encode_command(&p).1;
	let at = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].clone());
	assert_eq!(at("-window_id").as_deref(), Some("60817415"));
	assert_eq!(at("-i").as_deref(), Some(":0.0"));
	assert!(!args.iter().any(|a| a == "-video_size"), "{args:?}");
	assert_eq!(
		at("-vf").as_deref(),
		Some("scale=1280:720:out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709")
	);

	let v = StreamPlan { encoder: HwEncoder::Vaapi, ..p.clone() };
	let args = encode_command(&v).1;
	assert_eq!(args.iter().filter(|a| *a == "-vf").count(), 1, "{args:?}");
	assert!(
		args.iter().any(|a| a
			== "scale=1280:720:out_color_matrix=bt709:out_range=tv,format=nv12,hwupload,setparams=color_primaries=bt709:color_trc=bt709"),
		"{args:?}"
	);
}

/// The screen path is untouched by the window field: no `-window_id`, `-video_size` kept,
/// and a window id on a non-x11grab capture is ignored.
#[test]
fn window_none_keeps_screen_args() {
	let args = encode_command(&plan(HwEncoder::Software)).1;
	assert!(!args.iter().any(|a| a == "-window_id"));
	assert!(args.iter().any(|a| a == "1920x1080"));
	// The only filters of a plain x11grab screen are the SDR conversion + colour tag (no resize).
	let vf = args.iter().position(|a| a == "-vf").map(|i| args[i + 1].as_str());
	assert_eq!(
		vf,
		Some("scale=out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709"),
		"{args:?}"
	);
	let k = StreamPlan {
		capture: CaptureMethod::Kmsgrab,
		window: Some(WindowInput::X11 { id: 5 }),
		..plan(HwEncoder::Software)
	};
	let args = encode_command(&k).1;
	assert!(!args.iter().any(|a| a == "-window_id"), "{args:?}");
}

/// Raw NV12 frames from an in-process source (macOS ScreenCaptureKit) replace the capture
/// method's input entirely and carry the exact frame size.
#[test]
fn raw_nv12_window_input() {
	let p = StreamPlan {
		capture: CaptureMethod::AvFoundation,
		encoder: HwEncoder::VideoToolbox,
		width: 1440,
		height: 900,
		window: Some(WindowInput::RawNv12 { url: "tcp://127.0.0.1:40123".into() }),
		..plan(HwEncoder::Software)
	};
	let args = encode_command(&p).1;
	let joined = args.join(" ");
	assert!(
		joined.contains("-f rawvideo -pix_fmt nv12 -video_size 1440x900 -framerate 60 -i tcp://127.0.0.1:40123"),
		"{joined}"
	);
	assert!(!joined.contains("avfoundation"), "{joined}");
	assert!(joined.contains("h264_videotoolbox"), "{joined}");
}

#[test]
fn fit_within_keeps_aspect_even_and_never_upscales() {
	// Smaller than the box: own size, rounded down to even.
	assert_eq!(fit_within((801, 601), (1920, 1080)), (800, 600));
	// Wider than the box: width-bound.
	assert_eq!(fit_within((3840, 1600), (1920, 1080)), (1920, 800));
	// Taller than the box: height-bound.
	assert_eq!(fit_within((1000, 2000), (1920, 1080)), (540, 1080));
	// Exactly the box.
	assert_eq!(fit_within((1920, 1080), (1920, 1080)), (1920, 1080));
	// Odd box and odd result stay even and inside the box.
	let (w, h) = fit_within((2561, 1441), (1279, 719));
	assert!(w % 2 == 0 && h % 2 == 0 && w <= 1279 && h <= 719, "{w}x{h}");
	// Degenerate inputs never yield 0.
	assert_eq!(fit_within((0, 0), (1280, 720)), (1280, 720));
	assert_eq!(fit_within((1, 1), (1280, 720)), (2, 2));
	assert_eq!(fit_within((640, 480), (0, 0)), (2, 2));
}

#[test]
fn gst_x11_window_pipeline_targets_the_window_at_a_fixed_size() {
	let p = gst::x11_window_pipeline(":0", 0x3a0_0007, 0, (1281, 721), "x264enc name=venc", "10.0.0.5", 9000);
	assert!(p.starts_with("ximagesrc display-name=:0 xid=60817415 "), "{p}");
	assert!(p.contains("framerate=1/1"), "fps floored to 1: {p}");
	assert!(p.contains("width=1280,height=720"), "even fixed size: {p}");
	assert!(p.contains("! x264enc name=venc ! udpsink host=10.0.0.5 port=9000"), "{p}");
}

#[test]
fn gst_wayland_window_cast_is_scaled_to_a_fixed_box() {
	let screen = gst::wayland_pipeline(7, 42, "x264enc name=venc", "10.0.0.5", 9000);
	assert!(!screen.contains("videoscale"), "{screen}");
	assert_eq!(screen, gst::wayland_pipeline_sized(7, 42, "x264enc name=venc", "10.0.0.5", 9000, None));
	let win = gst::wayland_pipeline_sized(7, 42, "x264enc name=venc", "10.0.0.5", 9000, Some((1281, 719)));
	assert!(
		win.contains("format=NV12,colorimetry=bt709 ! videoscale n-threads=0 add-borders=true ! video/x-raw,width=1280,height=718,pixel-aspect-ratio=1/1 ! x264enc name=venc"),
		"{win}"
	);
	assert!(win.starts_with("pipewiresrc fd=7 path=42 "), "{win}");
}

#[test]
fn gst_encode_timer_measures_time_inside_the_encoder() {
	use std::time::{Duration, Instant};
	let mut t = gst::EncodeTimer::default();
	let t0 = Instant::now();
	// 60 fps frames, each 4 ms inside the encoder: the meter reads ~4 ms (not the 16.7 ms
	// frame gap), and reports about once a second.
	let mut reports = Vec::new();
	for i in 0..130u64 {
		let at = t0 + Duration::from_micros(i * 16_667);
		t.frame_in(i * 16_666_667, at);
		if let Some(ms) = t.frame_out(i * 16_666_667, at + Duration::from_millis(4)) {
			reports.push(ms);
		}
	}
	assert_eq!(reports.len(), 2, "one report per second: {reports:?}");
	assert!(reports.iter().all(|ms| (ms - 4.0).abs() < 0.2), "{reports:?}");
	assert_eq!(t.inflight(), 0);
}

#[test]
fn gst_encode_timer_forgets_dropped_frames_and_ignores_unknown_pts() {
	use std::time::{Duration, Instant};
	let mut t = gst::EncodeTimer::default();
	let t0 = Instant::now();
	t.frame_in(1, t0);
	t.frame_in(2, t0 + Duration::from_millis(16));
	t.frame_in(3, t0 + Duration::from_millis(33));
	// A header buffer / re-stamped output: not a frame we saw going in.
	assert!(t.frame_out(99, t0 + Duration::from_millis(40)).is_none());
	assert_eq!(t.inflight(), 3);
	// Frame 1 was dropped by the encoder; frame 2 comes out → frame 1 is forgotten too.
	t.frame_out(2, t0 + Duration::from_millis(20));
	assert_eq!(t.inflight(), 1);
	assert!((t.ema_ms() - 4.0).abs() < 0.01);
	// Bounded: a source whose PTS never come back can't grow the queue.
	for i in 0..1000 {
		t.frame_in(1000 + i, t0);
	}
	assert_eq!(t.inflight(), gst::EncodeTimer::MAX_INFLIGHT);
}

/// An odd requested size (screen adaptation, a client window of any size) is evened on every
/// ffmpeg capture path, as the native paths do — an odd 4:2:0 frame kills the encoder.
#[test]
fn odd_sizes_are_evened_on_the_ffmpeg_paths() {
	let odd = |enc: HwEncoder, cap: CaptureMethod| StreamPlan {
		width: 1921,
		height: 1081,
		capture: cap,
		..plan(enc)
	};
	// Windows full screen: ddagrab CPU-bounce / MF / AMF / QSV / NVENC zero-copy, and gdigrab.
	for enc in [HwEncoder::Software, HwEncoder::Amf, HwEncoder::MediaFoundation] {
		let f = encode_command(&odd(enc, CaptureMethod::Ddagrab)).1.join(" ");
		assert!(f.contains("scale=1920:1080") && !f.contains("1921"), "[{enc:?}] {f}");
	}
	let f = encode_command(&odd(HwEncoder::Qsv, CaptureMethod::Ddagrab)).1.join(" ");
	assert!(f.contains("vpp_qsv=w=1920:h=1080") && !f.contains("1921"), "{f}");
	let mut z = odd(HwEncoder::Nvenc, CaptureMethod::Ddagrab);
	z.gpu_zerocopy = true;
	let f = encode_command(&z).1.join(" ");
	assert!(f.contains("scale_cuda=1920:1080") && !f.contains("1921"), "{f}");
	// Hybrid NVENC: the evened request equals the capture → the capture's own size.
	let mut h = odd(HwEncoder::Nvenc, CaptureMethod::Ddagrab);
	h.capture_size = Some((1920, 1080));
	let f = encode_command(&h).1.join(" ");
	assert!(f.contains("scale_cuda=1920:1080:") && !f.contains("1921"), "{f}");
	let f = encode_command(&odd(HwEncoder::Software, CaptureMethod::Gdigrab)).1.join(" ");
	assert!(f.contains("scale=1920:1080") && !f.contains("1921"), "{f}");
	// Linux x11grab region.
	let f = encode_command(&odd(HwEncoder::Software, CaptureMethod::X11grab)).1.join(" ");
	assert!(f.contains("1920x1080") && !f.contains("1921"), "{f}");
	// A 1-pixel request floors at 2×2.
	let tiny = StreamPlan { width: 1, height: 1, ..plan(HwEncoder::Software) };
	assert!(encode_command(&tiny).1.join(" ").contains("2x2"));
	// Even sizes pass unchanged.
	assert!(encode_command(&plan(HwEncoder::Software)).1.join(" ").contains("1920x1080"));
	// Raw NV12 window frames are produced at exactly the plan size: never resized here.
	let raw = StreamPlan {
		width: 1281,
		height: 721,
		window: Some(WindowInput::RawNv12 { url: "tcp://127.0.0.1:1".into() }),
		..plan(HwEncoder::Software)
	};
	assert!(encode_command(&raw).1.join(" ").contains("1281x721"));
}

#[test]
fn gst_wayland_convert_uses_every_core_or_the_gpu() {
	// CPU path (every encoder by default): the convert is sliced over all cores.
	let cpu = gst::wayland_pipeline(7, 42, "x264enc name=venc", "10.0.0.5", 9000);
	assert!(cpu.contains("! videoconvert n-threads=0 ! video/x-raw,format=NV12,colorimetry=bt709 ! x264enc name=venc"), "{cpu}");
	// Only the `va` plugin's encoders get the GPU conversion.
	let va = gst::encoder_fragment(gst::GstEncoder::Va, VCodec::H264, 8000, 60).unwrap();
	assert_eq!(gst::wayland_convert_for(&va), gst::WaylandConvert::Va);
	for enc in [gst::GstEncoder::Vaapi, gst::GstEncoder::Nv, gst::GstEncoder::X264, gst::GstEncoder::Mpp] {
		let f = gst::encoder_fragment(enc, VCodec::H264, 8000, 60).unwrap();
		assert_eq!(gst::wayland_convert_for(&f), gst::WaylandConvert::Cpu, "{f}");
	}
	let gpu = gst::wayland_pipeline_conv(7, 42, &va, "10.0.0.5", 9000, None, gst::WaylandConvert::Va);
	assert!(gpu.contains("! vapostproc ! video/x-raw(memory:VAMemory),format=NV12,colorimetry=bt709 ! vah264enc name=venc"), "{gpu}");
	assert!(!gpu.contains("videoconvert"), "{gpu}");
	// The caps in front of vapostproc stay open: DMA-BUF or SHM, whatever PipeWire offers.
	assert!(!gpu.contains("memory:DMABuf"), "{gpu}");
	// A window cast scales on the GPU too, into the same fixed even box.
	let win = gst::wayland_pipeline_conv(7, 42, &va, "10.0.0.5", 9000, Some((1281, 719)), gst::WaylandConvert::Va);
	assert!(
		win.contains("vapostproc add-borders=true ! video/x-raw(memory:VAMemory),format=NV12,colorimetry=bt709,width=1280,height=718,pixel-aspect-ratio=1/1 ! vah264enc"),
		"{win}"
	);
	// The X11 converts are threaded too.
	let x = gst::x11_pipeline(":0", 60, "x264enc name=venc", "10.0.0.2", 9000, false, None);
	assert!(x.contains("videoconvert n-threads=0"), "{x}");
	let xw = gst::x11_window_pipeline(":0", 5, 60, (1280, 720), "x264enc name=venc", "10.0.0.2", 9000);
	assert!(xw.contains("videoconvert n-threads=0 ! videoscale n-threads=0"), "{xw}");
}

/// SDR = BT.709 limited range on every ffmpeg path: the encoder options (negotiated RGB → YUV
/// conversion + VUI) and the frame tag, at the end of a `-filter_complex` (ddagrab) or of the
/// `-vf` chain, or first for a capture that is YUV already (labelled, not converted).
#[test]
fn sdr_streams_signal_bt709_limited() {
	let tag_rgb = "setparams=color_primaries=bt709:color_trc=bt709";
	let tag_yuv = "setparams=colorspace=bt709:range=tv:color_primaries=bt709:color_trc=bt709";
	let opts = "-colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv";
	for enc in [
		HwEncoder::Software,
		HwEncoder::Nvenc,
		HwEncoder::Amf,
		HwEncoder::Qsv,
		HwEncoder::Vaapi,
		HwEncoder::MediaFoundation,
		HwEncoder::VideoToolbox,
	] {
		for capture in [CaptureMethod::X11grab, CaptureMethod::Gdigrab, CaptureMethod::Ddagrab] {
			let p = StreamPlan { capture, ..plan(enc) };
			let args = encode_command(&p).1;
			let j = args.join(" ");
			assert!(j.contains(opts), "[{enc:?} {capture:?}] {j}");
			assert_eq!(args.iter().filter(|a| a.contains("setparams")).count(), 1, "[{enc:?} {capture:?}] {j}");
			let at = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].clone());
			let chain = at("-filter_complex").or_else(|| at("-vf")).unwrap();
			assert!(chain.ends_with(tag_rgb), "[{enc:?} {capture:?}] {chain}");
			// (VA-API never runs on Windows: its upload `-vf` beside ddagrab is not a real plan.)
			if capture == CaptureMethod::Ddagrab && enc != HwEncoder::Vaapi {
				assert!(at("-vf").is_none(), "[{enc:?}] no -vf beside a complex graph: {j}");
			}
			// Every RGB → YUV conversion names BT.709 limited, ahead of the format constraint
			// (an ffmpeg older than 7.1 would otherwise convert BT.601 under the BT.709 tag).
			// The one exception: ddagrab → NVENC without the CUDA map hands NVENC the BGRA.
			let hands_rgb = capture == CaptureMethod::Ddagrab && enc == HwEncoder::Nvenc;
			let conv = chain.find("out_color_matrix=bt709:out_range=tv");
			assert_eq!(conv.is_some(), !hands_rgb, "[{enc:?} {capture:?}] {chain}");
			if let (Some(c), Some(f)) = (conv, chain.rfind("format=")) {
				let scaler = chain[..c].rfind("scale").or_else(|| chain[..c].rfind("vpp_qsv")).unwrap();
				assert!(scaler < c, "[{enc:?} {capture:?}] {chain}");
				// The format the conversion feeds comes after (or is the scaler's own option).
				assert!(f > scaler, "[{enc:?} {capture:?}] {chain}");
			}
		}
	}
	// The ddagrab GPU scalers convert too: CUDA (zero-copy NVENC) and QSV name the matrix.
	for enc in [HwEncoder::Nvenc, HwEncoder::Qsv] {
		let p = StreamPlan { capture: CaptureMethod::Ddagrab, gpu_zerocopy: true, ..plan(enc) };
		let args = encode_command(&p).1;
		let fc = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
		assert!(fc.contains("format=nv12:out_color_matrix=bt709:out_range=tv"), "[{enc:?}] {fc}");
	}
	// An x11grab with no scale of its own gets an explicit one in front of the encoder's format.
	let p = StreamPlan { capture: CaptureMethod::X11grab, ..plan(HwEncoder::Software) };
	let args = encode_command(&p).1;
	let at = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].clone());
	assert_eq!(at("-vf").as_deref(), Some("scale=out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709"));
	assert!(args.iter().position(|a| a == "-vf") < args.iter().position(|a| a == "-pix_fmt"));
	// Kmsgrab frames are DRM-PRIME (no swscale): no CPU scale for an encoder that takes them.
	let p = StreamPlan { capture: CaptureMethod::Kmsgrab, ..plan(HwEncoder::Nvenc) };
	assert!(!encode_command(&p).1.join(" ").contains("scale="), "{:?}", encode_command(&p).1);
	// macOS frames are NV12 / UYVY already: the tag labels them before anything converts.
	let p = StreamPlan {
		capture: CaptureMethod::AvFoundation,
		window: Some(WindowInput::RawNv12 { url: "tcp://127.0.0.1:5000".into() }),
		..plan(HwEncoder::VideoToolbox)
	};
	let args = encode_command(&p).1;
	let at = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].clone());
	assert_eq!(at("-vf").as_deref(), Some(tag_yuv), "{args:?}");
	assert!(args.join(" ").contains(opts));
	let p = StreamPlan { capture: CaptureMethod::AvFoundation, ..plan(HwEncoder::VideoToolbox) };
	let args = encode_command(&p).1;
	assert!(args.iter().any(|a| a.starts_with(tag_yuv)), "{args:?}");
	// HDR keeps its own colour description (whenever the plan really streams HDR).
	let mut h = plan(HwEncoder::Software);
	h.codec = VCodec::H265;
	h.hdr = true;
	let j = encode_command(&h).1.join(" ");
	assert_eq!(!j.contains("bt709"), h.effective_hdr_yuv444().0, "{j}");
}

/// Colour-bar harness: run the real ffmpeg args of an SDR software plan (gdigrab chain with
/// the capture swapped for solid RGB frames, RTP swapped for a raw H.264 file), decode the
/// result and compare the YUV to the BT.709 limited-range values, plus the VUI tag. BT.601
/// would put red at Y = 81 instead of 63. Runs only with `PULSAR_TEST_FFMPEG=<ffmpeg>`
/// (FFmpeg 7.1+, e.g. the desktop's bundled `src-tauri/resources/ffmpeg`), else it skips.
#[test]
fn sdr_colour_bars_through_ffmpeg() {
	let Ok(ffmpeg) = std::env::var("PULSAR_TEST_FFMPEG") else {
		eprintln!("PULSAR_TEST_FFMPEG not set: colour-bar harness skipped");
		return;
	};
	let dir = std::env::temp_dir().join(format!("pulsar-colour-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	// (R, G, B) → BT.709 limited (Y, Cb, Cr).
	let bars = [
		("red", (63u8, 102u8, 240u8)),
		("lime", (173, 42, 26)),
		("blue", (32, 240, 118)),
		("white", (235, 128, 128)),
		("black", (16, 128, 128)),
	];
	// gdigrab (its own resize carries the matrix) and x11grab (the explicit conversion scale).
	for ((name, (y, cb, cr)), (capture, from, to)) in bars
		.into_iter()
		.flat_map(|b| [(b, (CaptureMethod::Gdigrab, "gdigrab", "desktop")), (b, (CaptureMethod::X11grab, "x11grab", ":0.0"))])
	{
		let p = StreamPlan { capture, width: 64, height: 64, ..plan(HwEncoder::Software) };
		let mut args = encode_command(&p).1;
		let i = args.iter().position(|a| a == from).unwrap() - 1;
		let j = args.iter().position(|a| a == to).unwrap();
		args.splice(i..=j, ["-f", "lavfi", "-i", &format!("color=c={name}:s=64x64:r=60,format=bgr0")].map(String::from));
		let k = args.iter().position(|a| a == "-bsf:v").unwrap();
		let out = dir.join(format!("{name}-{from}.h264"));
		args.truncate(k);
		args.extend(["-frames:v", "1", "-f", "h264", "-y"].map(String::from));
		args.push(out.to_string_lossy().into_owned());
		let st = std::process::Command::new(&ffmpeg).args(&args).status().unwrap();
		assert!(st.success(), "encode {name}: {args:?}");
		let dec = std::process::Command::new(&ffmpeg)
			.args(["-hide_banner", "-loglevel", "error", "-i"])
			.arg(&out)
			.args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
			.output()
			.unwrap();
		assert!(dec.status.success(), "decode {name}");
		let px = &dec.stdout;
		assert_eq!(px.len(), 64 * 64 * 3 / 2, "{name}");
		let got = (px[64 * 32 + 32], px[64 * 64 + 16 * 32 + 16], px[64 * 64 + 32 * 32 + 16 * 32 + 16]);
		let near = |a: u8, b: u8| a.abs_diff(b) <= 3;
		assert!(near(got.0, y) && near(got.1, cb) && near(got.2, cr), "{name} {from}: got {got:?}, BT.709 limited {:?}", (y, cb, cr));
		let info = std::process::Command::new(&ffmpeg).args(["-hide_banner", "-i"]).arg(&out).output().unwrap();
		let info = String::from_utf8_lossy(&info.stderr);
		assert!(info.contains("yuv420p(tv, bt709"), "{name}: VUI not BT.709 limited: {info}");
	}
	let _ = std::fs::remove_dir_all(&dir);
}

/// Every gst `videoconvert` into the encoder names BT.709 (limited range): GStreamer's default
/// for a YUV caps without colorimetry is BT.601 below 720 lines, which the client would then
/// decode as BT.709 (Pulsar's SDR rule).
#[test]
fn gst_conversions_pin_bt709() {
	let frag = "x264enc name=venc";
	for desc in [
		gst::wayland_pipeline(3, 42, frag, "10.0.0.5", 9000),
		gst::probe_pipeline(frag),
	] {
		assert!(desc.contains("video/x-raw,format=NV12,colorimetry=bt709"), "{desc}");
	}
}
