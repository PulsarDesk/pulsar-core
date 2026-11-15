//! Host keyboard tables that are pure data, kept out of the per-OS injectors so they compile
//! and are unit-tested on every target (the injectors themselves only build on their OS).
//!
//! * [`CLIENT_KEYS`]: every evdev code a Pulsar client may send as `InputEvent::Key`. Each
//!   host table is tested against it, so a key one client can send is a key every host types.
//! * [`build_char_table`]: the Linux host's character → key lookup over the host's OWN
//!   layout (`uinput.rs` feeds it from libxkbcommon). A `Char` the host layout can type is
//!   typed with the key and level that layout uses, instead of a fixed US table.
//! * [`mac_vk`]: evdev → macOS virtual key code for the macOS host.

use std::collections::HashMap;

/// Every evdev keycode a client can send (`KEY_*` from `linux/input-event-codes.h`): the
/// webview `keymap.ts`, the Windows hook / Interception tables, the macOS tap table, and the
/// Linux evdev grab (which forwards raw codes, so it is the union of the others in practice).
pub const CLIENT_KEYS: &[u16] = &[
	1, // Esc
	2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, // 1..0 - = Backspace Tab
	16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, // Q..] Enter LCtrl
	30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, // A..` LShift \
	44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, // Z../ RShift KP* LAlt Space Caps
	59, 60, 61, 62, 63, 64, 65, 66, 67, 68, // F1..F10
	69, 70, // NumLock ScrollLock
	71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, // keypad
	86, // KEY_102ND (ISO <>| next to Left Shift)
	87, 88, // F11 F12
	89, // KEY_RO (JIS \_)
	92, 93, 94, // KEY_HENKAN, KEY_KATAKANAHIRAGANA, KEY_MUHENKAN (JIS)
	95, // KEY_KPJPCOMMA
	96, 97, 98, 99, 100, // KPEnter RCtrl KP/ SysRq(PrintScreen) RAlt
	102, 103, 104, 105, 106, 107, 108, 109, 110, 111, // Home..Delete
	117, // KEY_KPEQUAL
	119, // KEY_PAUSE
	121, // KEY_KPCOMMA
	122, 123, // KEY_HANGEUL (Lang1 / Kana), KEY_HANJA (Lang2 / Eisu)
	124, // KEY_YEN (JIS)
	125, 126, 127, // LMeta RMeta Compose(Menu)
	183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, // F13..F24
];

/// evdev Left Shift / Right Alt (AltGr, the key that selects level 3 on most layouts).
pub const EVDEV_LSHIFT: u16 = 42;
pub const EVDEV_RSHIFT: u16 = 54;
pub const EVDEV_ALTGR: u16 = 100;

/// One key press that types a character on the host layout: the evdev code plus the levels
/// it needs (Shift = level 2, AltGr = level 3, both = level 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
	pub code: u16,
	pub shift: bool,
	pub altgr: bool,
}

/// Keys the char lookup never uses: modifiers and locks (pressing one changes state instead
/// of typing), and the keypad, whose output depends on Num Lock — the main block types the
/// same characters without that dependency.
fn skip_for_chars(code: u16) -> bool {
	matches!(
		code,
		29 | 42 | 54 | 56 | 58 | 69 | 70 | 97 | 100 | 125 | 126 | 127
			| 55 | 71..=83 | 96 | 98 | 117 | 118 | 121
	)
}

/// Build the character → key table of a layout. `resolve(code, shift, altgr)` returns the
/// character the host layout produces for evdev `code` with those levels held (no Caps /
/// Num Lock), or `None`. Lower levels win (a char on the base level never needs Shift), then
/// lower codes; control characters are left out (the client sends those keys as `Key`).
pub fn build_char_table(mut resolve: impl FnMut(u16, bool, bool) -> Option<char>) -> HashMap<char, KeyChord> {
	let mut table = HashMap::new();
	for (shift, altgr) in [(false, false), (true, false), (false, true), (true, true)] {
		for code in 1u16..=248 {
			if skip_for_chars(code) {
				continue;
			}
			let Some(c) = resolve(code, shift, altgr) else { continue };
			if c.is_control() {
				continue;
			}
			table.entry(c).or_insert(KeyChord { code, shift, altgr });
		}
	}
	table
}

/// The US layout as a [`build_char_table`] resolver: the Linux host's last resort when no
/// keymap compiles at all (no xkeyboard-config data on a bare server), so ASCII still types
/// as it did before the host layout was read.
pub fn us_layout(code: u16, shift: bool, altgr: bool) -> Option<char> {
	const ROWS: &[(u16, &str, &str)] = &[
		(2, "1234567890-=", "!@#$%^&*()_+"),
		(16, "qwertyuiop[]", "QWERTYUIOP{}"),
		(30, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
		(43, "\\zxcvbnm,./", "|ZXCVBNM<>?"),
		(57, " ", " "),
	];
	if altgr {
		return None;
	}
	ROWS.iter().find_map(|&(first, base, shifted)| {
		let i = code.checked_sub(first)? as usize;
		(if shift { shifted } else { base }).chars().nth(i)
	})
}

/// The key a control character is typed with on every layout (Enter / Tab / Backspace / Esc).
pub fn control_char_key(c: char) -> Option<u16> {
	Some(match c {
		'\n' | '\r' => 28,
		'\t' => 15,
		'\u{8}' => 14,
		'\u{1b}' => 1,
		'\u{7f}' => 111,
		_ => return None,
	})
}

/// The hex digits of `c`'s code point, lowercase, for the GTK / IBus Unicode entry
/// (Ctrl+Shift+U, the digits, Space).
pub fn unicode_hex(c: char) -> String {
	format!("{:x}", c as u32)
}

/// With Caps Lock on, a letter's case comes out flipped: type the other level instead.
/// Only for characters that have a case (digits and symbols ignore Caps Lock).
pub fn caps_adjust(c: char, chord: KeyChord, caps_on: bool) -> KeyChord {
	let cased = c.is_lowercase() || c.is_uppercase();
	if caps_on && cased {
		KeyChord { shift: !chord.shift, ..chord }
	} else {
		chord
	}
}

/// Key edges `(evdev code, 1 = down / 0 = up)` that tap `chord` given which keys the host
/// already holds (`held`, the client's own held modifiers): a held Shift / AltGr the chord does
/// not want is lifted for the tap and pressed again after it, a level the chord needs is
/// pressed for the tap only. A held Shift is reused, never pressed twice.
pub fn chord_edges(chord: KeyChord, held: impl Fn(u16) -> bool) -> Vec<(u16, i32)> {
	let mut before: Vec<(u16, i32)> = Vec::new();
	let shifts: Vec<u16> = [EVDEV_LSHIFT, EVDEV_RSHIFT].into_iter().filter(|&k| held(k)).collect();
	if chord.shift && shifts.is_empty() {
		before.push((EVDEV_LSHIFT, 1));
	} else if !chord.shift {
		before.extend(shifts.iter().map(|&k| (k, 0)));
	}
	let altgr_held = held(EVDEV_ALTGR);
	if chord.altgr && !altgr_held {
		before.push((EVDEV_ALTGR, 1));
	} else if !chord.altgr && altgr_held {
		before.push((EVDEV_ALTGR, 0));
	}
	let mut edges = before.clone();
	edges.push((chord.code, 1));
	edges.push((chord.code, 0));
	// Undo the modifier changes in reverse order.
	edges.extend(before.iter().rev().map(|&(k, v)| (k, 1 - v)));
	edges
}

/// Key edges of the GTK / IBus Unicode entry for `c`: Ctrl+Shift+U, the code point's hex
/// digits, Space — every key looked up in the host layout's `table` (a TR-F host has `u` and
/// the digits elsewhere than US). `None` when the layout lacks one of them.
pub fn unicode_entry_edges(
	c: char,
	table: &HashMap<char, KeyChord>,
	held: impl Fn(u16) -> bool,
) -> Option<Vec<(u16, i32)>> {
	const CTRL: u16 = 29;
	const SPACE: u16 = 57;
	let u = table.get(&'u').or_else(|| table.get(&'U'))?.code;
	let digits: Vec<KeyChord> = unicode_hex(c).chars().map(|d| table.get(&d).copied()).collect::<Option<_>>()?;
	// Lift what the client holds (Shift / AltGr) for the whole sequence, restore it after.
	let lifted: Vec<u16> = [EVDEV_LSHIFT, EVDEV_RSHIFT, EVDEV_ALTGR].into_iter().filter(|&k| held(k)).collect();
	let mut edges: Vec<(u16, i32)> = lifted.iter().map(|&k| (k, 0)).collect();
	edges.extend([(CTRL, 1), (EVDEV_LSHIFT, 1), (u, 1), (u, 0), (EVDEV_LSHIFT, 0), (CTRL, 0)]);
	for d in digits {
		edges.extend(chord_edges(d, |_| false));
	}
	edges.extend([(SPACE, 1), (SPACE, 0)]);
	edges.extend(lifted.iter().rev().map(|&k| (k, 1)));
	Some(edges)
}

/// XKB RMLVO names (rules / model / layout / variant / options) of a host keyboard, read
/// from wherever the host session publishes them. Empty fields mean "libxkbcommon default".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XkbNames {
	pub rules: String,
	pub model: String,
	pub layout: String,
	pub variant: String,
	pub options: String,
}

impl XkbNames {
	pub fn is_empty(&self) -> bool {
		self.layout.is_empty()
	}
}

/// The X root window's `_XKB_RULES_NAMES` property (what `setxkbmap -query` prints): five
/// NUL-separated strings, rules / model / layout / variant / options.
pub fn parse_xkb_rules_names(bytes: &[u8]) -> Option<XkbNames> {
	let mut it = bytes.split(|&b| b == 0).map(|s| String::from_utf8_lossy(s).trim().to_string());
	let names = XkbNames {
		rules: it.next().unwrap_or_default(),
		model: it.next().unwrap_or_default(),
		layout: it.next().unwrap_or_default(),
		variant: it.next().unwrap_or_default(),
		options: it.next().unwrap_or_default(),
	};
	(!names.is_empty()).then_some(names)
}

/// Debian / Ubuntu `/etc/default/keyboard` (`XKBLAYOUT="tr"` shell assignments).
pub fn parse_default_keyboard(text: &str) -> Option<XkbNames> {
	let get = |key: &str| {
		text.lines()
			.filter_map(|l| l.trim().strip_prefix(key)?.trim_start().strip_prefix('='))
			.last()
			.map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
			.unwrap_or_default()
	};
	let names = XkbNames {
		rules: String::new(),
		model: get("XKBMODEL"),
		layout: get("XKBLAYOUT"),
		variant: get("XKBVARIANT"),
		options: get("XKBOPTIONS"),
	};
	(!names.is_empty()).then_some(names)
}

/// An xorg.conf(.d) keyboard section, as `localectl set-x11-keymap` writes it to
/// `/etc/X11/xorg.conf.d/00-keyboard.conf` (`Option "XkbLayout" "tr"`).
pub fn parse_xorg_keyboard_conf(text: &str) -> Option<XkbNames> {
	let get = |key: &str| {
		text.lines()
			.filter_map(|l| {
				let l = l.trim();
				if l.starts_with('#') {
					return None;
				}
				let mut parts = l.split('"').map(str::trim).filter(|p| !p.is_empty());
				(parts.next()? == "Option" && parts.next()?.eq_ignore_ascii_case(key))
					.then(|| parts.next().unwrap_or_default().to_string())
			})
			.last()
			.unwrap_or_default()
	};
	let names = XkbNames {
		rules: get("XkbRules"),
		model: get("XkbModel"),
		layout: get("XkbLayout"),
		variant: get("XkbVariant"),
		options: get("XkbOptions"),
	};
	(!names.is_empty()).then_some(names)
}

/// Whether the host's input method understands the Unicode entry Ctrl+Shift+U, hex, Space:
/// GTK's built-in input method and IBus do (GNOME uses both by default); KDE without IBus and
/// most wlroots compositors do not, and there the sequence would type "u15f " instead.
/// `get` reads an environment variable.
pub fn unicode_entry_supported(get: impl Fn(&str) -> Option<String>) -> bool {
	let has = |var: &str, needle: &str| get(var).is_some_and(|v| v.to_ascii_lowercase().contains(needle));
	has("PULSAR_UNICODE_ENTRY", "1")
		|| has("XDG_CURRENT_DESKTOP", "gnome")
		|| has("GTK_IM_MODULE", "ibus")
		|| has("QT_IM_MODULE", "ibus")
		|| has("XMODIFIERS", "ibus")
}

/// evdev keycode → macOS virtual key code (`kVK_*`, `<HIToolbox/Events.h>`). Positional (the
/// key at the same place on the Mac). `iso`: the host's keyboard is ISO, where macOS reports
/// the key left of 1 as `kVK_ISO_Section` and the key right of Left Shift as `kVK_ANSI_Grave`
/// (the inverse of the client's `layout_fix` in pulsar-macinput). Keys a Mac has no slot for
/// map to the F-key Apple's own PC-keyboard support uses (Print Screen = F13, Scroll Lock =
/// F14, Pause = F15) or to `None`.
pub fn mac_vk(code: u32, iso: bool) -> Option<u16> {
	Some(match code {
		1 => 0x35,  // Escape       (kVK_Escape)
		2 => 0x12,  // Digit1       (kVK_ANSI_1)
		3 => 0x13,  // Digit2
		4 => 0x14,  // Digit3
		5 => 0x15,  // Digit4
		6 => 0x17,  // Digit5
		7 => 0x16,  // Digit6
		8 => 0x1A,  // Digit7
		9 => 0x1C,  // Digit8
		10 => 0x19, // Digit9
		11 => 0x1D, // Digit0
		12 => 0x1B, // Minus        (kVK_ANSI_Minus)
		13 => 0x18, // Equal        (kVK_ANSI_Equal)
		14 => 0x33, // Backspace    (kVK_Delete)
		15 => 0x30, // Tab          (kVK_Tab)
		16 => 0x0C, // Q
		17 => 0x0D, // W
		18 => 0x0E, // E
		19 => 0x0F, // R
		20 => 0x11, // T
		21 => 0x10, // Y
		22 => 0x20, // U
		23 => 0x22, // I
		24 => 0x1F, // O
		25 => 0x23, // P
		26 => 0x21, // [            (kVK_ANSI_LeftBracket)
		27 => 0x1E, // ]            (kVK_ANSI_RightBracket)
		28 => 0x24, // Enter        (kVK_Return)
		29 => 0x3B, // Left Ctrl    (kVK_Control)
		30 => 0x00, // A
		31 => 0x01, // S
		32 => 0x02, // D
		33 => 0x03, // F
		34 => 0x05, // G
		35 => 0x04, // H
		36 => 0x26, // J
		37 => 0x28, // K
		38 => 0x25, // L
		39 => 0x29, // ;            (kVK_ANSI_Semicolon)
		40 => 0x27, // '            (kVK_ANSI_Quote)
		41 if iso => 0x0A, // key left of 1 on an ISO Mac (kVK_ISO_Section)
		41 => 0x32, // `            (kVK_ANSI_Grave)
		42 => 0x38, // Left Shift   (kVK_Shift)
		43 => 0x2A, // \            (kVK_ANSI_Backslash)
		44 => 0x06, // Z
		45 => 0x07, // X
		46 => 0x08, // C
		47 => 0x09, // V
		48 => 0x0B, // B
		49 => 0x2D, // N
		50 => 0x2E, // M
		51 => 0x2B, // ,            (kVK_ANSI_Comma)
		52 => 0x2F, // .            (kVK_ANSI_Period)
		53 => 0x2C, // /            (kVK_ANSI_Slash)
		54 => 0x3C, // Right Shift  (kVK_RightShift)
		55 => 0x43, // Numpad *     (kVK_ANSI_KeypadMultiply)
		56 => 0x3A, // Left Alt     (kVK_Option)
		57 => 0x31, // Space        (kVK_Space)
		58 => 0x39, // Caps Lock    (kVK_CapsLock)
		59 => 0x7A, // F1
		60 => 0x78, // F2
		61 => 0x63, // F3
		62 => 0x76, // F4
		63 => 0x60, // F5
		64 => 0x61, // F6
		65 => 0x62, // F7
		66 => 0x64, // F8
		67 => 0x65, // F9
		68 => 0x6D, // F10
		69 => 0x47, // Num Lock → Keypad Clear (kVK_ANSI_KeypadClear; no NumLock on Mac)
		70 => 0x6B, // Scroll Lock → F14 (Apple's PC-keyboard mapping)
		71 => 0x59, // Numpad 7
		72 => 0x5B, // Numpad 8
		73 => 0x5C, // Numpad 9
		74 => 0x4E, // Numpad -     (kVK_ANSI_KeypadMinus)
		75 => 0x56, // Numpad 4
		76 => 0x57, // Numpad 5
		77 => 0x58, // Numpad 6
		78 => 0x45, // Numpad +     (kVK_ANSI_KeypadPlus)
		79 => 0x53, // Numpad 1
		80 => 0x54, // Numpad 2
		81 => 0x55, // Numpad 3
		82 => 0x52, // Numpad 0
		83 => 0x41, // Numpad .     (kVK_ANSI_KeypadDecimal)
		86 if iso => 0x32, // ISO key right of Left Shift, as an ISO Mac reports it
		86 => 0x0A, // kVK_ISO_Section
		87 => 0x67, // F11
		88 => 0x6F, // F12
		89 => 0x5E, // JIS Underscore (kVK_JIS_Underscore)
		95 => 0x5F, // JIS Keypad ,  (kVK_JIS_KeypadComma)
		96 => 0x4C, // Numpad Enter (kVK_ANSI_KeypadEnter)
		97 => 0x3E, // Right Ctrl   (kVK_RightControl)
		98 => 0x4B, // Numpad /     (kVK_ANSI_KeypadDivide)
		99 => 0x69, // Print Screen → F13 (Apple's PC-keyboard mapping)
		100 => 0x3D, // Right Alt   (kVK_RightOption)
		102 => 0x73, // Home        (kVK_Home)
		103 => 0x7E, // Arrow Up    (kVK_UpArrow)
		104 => 0x74, // Page Up     (kVK_PageUp)
		105 => 0x7B, // Arrow Left  (kVK_LeftArrow)
		106 => 0x7C, // Arrow Right (kVK_RightArrow)
		107 => 0x77, // End         (kVK_End)
		108 => 0x7D, // Arrow Down  (kVK_DownArrow)
		109 => 0x79, // Page Down   (kVK_PageDown)
		110 => 0x72, // Insert → Help (kVK_Help; closest macOS slot)
		111 => 0x75, // Delete (forward) (kVK_ForwardDelete)
		117 => 0x51, // Keypad =    (kVK_ANSI_KeypadEquals)
		119 => 0x71, // Pause → F15 (Apple's PC-keyboard mapping)
		121 => 0x5F, // Keypad , → JIS Keypad , (the only keypad comma macOS has)
		122 => 0x68, // Lang1 / Kana (kVK_JIS_Kana)
		123 => 0x66, // Lang2 / Eisu (kVK_JIS_Eisu)
		124 => 0x5D, // JIS Yen      (kVK_JIS_Yen)
		125 => 0x37, // Left Win  → Command (kVK_Command)
		126 => 0x36, // Right Win → Right Command (kVK_RightCommand)
		127 => 0x6E, // Context Menu (the PC Application key on macOS)
		183 => 0x69, // F13
		184 => 0x6B, // F14
		185 => 0x71, // F15
		186 => 0x6A, // F16
		187 => 0x40, // F17
		188 => 0x4F, // F18
		189 => 0x50, // F19
		190 => 0x5A, // F20
		// F21..F24, Henkan / Muhenkan / Katakana-Hiragana: macOS has no virtual key for them.
		_ => return None,
	})
}

/// Client keys the macOS host cannot type (no `kVK_*` exists for them).
pub const MAC_UNMAPPED: &[u16] = &[92, 93, 94, 191, 192, 193, 194];

#[cfg(test)]
mod tests {
	use super::*;

	/// A TR-Q host layout (xkb `tr`), the characters the tests need: base / Shift / AltGr.
	fn tr_q(code: u16, shift: bool, altgr: bool) -> Option<char> {
		let (base, sh, ag) = match code {
			2 => ('1', '!', '>'),
			3 => ('2', '\'', '£'),
			4 => ('3', '^', '#'),
			8 => ('7', '/', '{'),
			9 => ('8', '(', '['),
			11 => ('0', '=', '}'),
			12 => ('*', '?', '\\'),
			16 => ('q', 'Q', '@'),
			18 => ('e', 'E', '€'),
			23 => ('ı', 'I', 'i'),
			26 => ('ğ', 'Ğ', '¨'),
			27 => ('ü', 'Ü', '~'),
			30 => ('a', 'A', 'æ'),
			31 => ('s', 'S', 'ß'),
			39 => ('ş', 'Ş', '´'),
			40 => ('i', 'İ', 'i'),
			43 => (',', ';', '`'),
			51 => ('ö', 'Ö', '×'),
			52 => ('ç', 'Ç', '÷'),
			53 => ('.', ':', '·'),
			57 => (' ', ' ', ' '),
			86 => ('<', '>', '|'),
			// keypad: must never be picked
			79 => ('1', '1', '1'),
			_ => return None,
		};
		Some(match (shift, altgr) {
			(false, false) => base,
			(true, false) => sh,
			_ => ag,
		})
	}

	#[test]
	fn tr_q_table_types_turkish_letters_and_altgr_symbols() {
		let t = build_char_table(tr_q);
		let k = |c| t.get(&c).copied();
		let chord = |code, shift, altgr| Some(KeyChord { code, shift, altgr });
		assert_eq!(k('ş'), chord(39, false, false));
		assert_eq!(k('ı'), chord(23, false, false));
		assert_eq!(k('İ'), chord(40, true, false));
		assert_eq!(k('ç'), chord(52, false, false));
		assert_eq!(k('ö'), chord(51, false, false));
		assert_eq!(k('Ğ'), chord(26, true, false));
		// 'i' is on the base level of 40 — never the AltGr level of 23.
		assert_eq!(k('i'), chord(40, false, false));
		assert_eq!(k('@'), chord(16, false, true));
		assert_eq!(k('€'), chord(18, false, true));
		assert_eq!(k('\\'), chord(12, false, true));
		assert_eq!(k('<'), chord(86, false, false));
		assert_eq!(k('|'), chord(86, false, true));
		// ',' sits where US has '\': the fixed US table typed ',' with KEY_COMMA = 'ö'.
		assert_eq!(k(','), chord(43, false, false));
		assert_eq!(k(':'), chord(53, true, false));
		// Digits come from the main row, never the Num Lock dependent keypad.
		assert_eq!(k('1'), chord(2, false, false));
		assert_eq!(k('{'), chord(8, false, true));
		assert_eq!(k('/'), chord(8, true, false));
		assert_eq!(k('Q'), chord(16, true, false));
		assert_eq!(k(' '), chord(57, false, false));
		assert_eq!(k('ğ').map(|c| c.code), Some(26));
		assert_eq!(k('ñ'), None);
	}

	#[test]
	fn chord_edges_respect_the_modifiers_the_client_holds() {
		let none = |_: u16| false;
		let at = KeyChord { code: 16, shift: false, altgr: true };
		assert_eq!(chord_edges(at, none), vec![(100, 1), (16, 1), (16, 0), (100, 0)]);
		// The client holds AltGr itself (TR-Q '@' = AltGr+Q): reuse it, don't release it.
		assert_eq!(chord_edges(at, |k| k == 100), vec![(16, 1), (16, 0)]);
		// Shift held but the char is on the base level ('\'' = Shift+2 on TR-Q, typed on a host
		// where it is unshifted): lift Shift for the tap only.
		let base = KeyChord { code: 40, shift: false, altgr: false };
		assert_eq!(chord_edges(base, |k| k == 54), vec![(54, 0), (40, 1), (40, 0), (54, 1)]);
		// A held AltGr is lifted for a base-level char too (else RAlt = Alt on a US host
		// turns the char into an Alt shortcut).
		assert_eq!(chord_edges(base, |k| k == 100), vec![(100, 0), (40, 1), (40, 0), (100, 1)]);
		let upper = KeyChord { code: 30, shift: true, altgr: false };
		assert_eq!(chord_edges(upper, none), vec![(42, 1), (30, 1), (30, 0), (42, 0)]);
		assert_eq!(chord_edges(upper, |k| k == 42), vec![(30, 1), (30, 0)]);
	}

	#[test]
	fn us_fallback_types_ascii() {
		let t = build_char_table(us_layout);
		let k = |c| t.get(&c).map(|k| (k.code, k.shift));
		assert_eq!(k('a'), Some((30, false)));
		assert_eq!(k('Z'), Some((44, true)));
		assert_eq!(k('@'), Some((3, true)));
		assert_eq!(k('\\'), Some((43, false)));
		assert_eq!(k('`'), Some((41, false)));
		assert_eq!(k('?'), Some((53, true)));
		assert_eq!(k(' '), Some((57, false)));
		assert_eq!(k('0'), Some((11, false)));
		// Every printable ASCII char.
		assert_eq!((' '..='~').filter(|c| t.contains_key(c)).count(), 95);
	}

	#[test]
	fn caps_lock_flips_only_cased_chars() {
		let a = KeyChord { code: 30, shift: false, altgr: false };
		assert!(caps_adjust('a', a, true).shift);
		assert!(!caps_adjust('a', a, false).shift);
		assert!(!caps_adjust('1', KeyChord { code: 2, ..a }, true).shift);
	}

	#[test]
	fn unicode_entry_uses_the_host_layout_keys() {
		let t = build_char_table(tr_q);
		// Not in the fake TR-Q table: 'u' and '5', so no entry possible.
		assert_eq!(unicode_entry_edges('ñ', &t, |_| false), None);
		let mut t = t;
		t.insert('u', KeyChord { code: 22, shift: false, altgr: false });
		t.insert('f', KeyChord { code: 33, shift: false, altgr: false });
		t.insert('5', KeyChord { code: 6, shift: false, altgr: false });
		// 'ş' = U+015F → "15f".
		let e = unicode_entry_edges('ş', &t, |k| k == 42).unwrap();
		assert_eq!(&e[..7], &[(42, 0), (29, 1), (42, 1), (22, 1), (22, 0), (42, 0), (29, 0)]);
		assert_eq!(&e[7..13], &[(2, 1), (2, 0), (6, 1), (6, 0), (33, 1), (33, 0)]);
		assert_eq!(&e[13..], &[(57, 1), (57, 0), (42, 1)]);
	}

	#[test]
	fn table_skips_modifiers_and_control_chars() {
		let t = build_char_table(|code, _, _| match code {
			42 => Some('x'),  // Shift: never a char key
			28 => Some('\r'), // Enter: control char, left to the Key path
			30 => Some('a'),
			_ => None,
		});
		assert_eq!(t.len(), 1);
		assert_eq!(t[&'a'].code, 30);
		assert_eq!(control_char_key('\n'), Some(28));
		assert_eq!(control_char_key('\t'), Some(15));
		assert_eq!(control_char_key('a'), None);
		assert_eq!(unicode_hex('ş'), "15f");
		assert_eq!(unicode_hex('€'), "20ac");
	}

	#[test]
	fn parses_host_layout_names() {
		let x = parse_xkb_rules_names(b"evdev\0pc105\0tr,us\0,\0grp:alt_shift_toggle\0").unwrap();
		assert_eq!(x.rules, "evdev");
		assert_eq!(x.model, "pc105");
		assert_eq!(x.layout, "tr,us");
		assert_eq!(x.variant, ",");
		assert_eq!(x.options, "grp:alt_shift_toggle");
		assert_eq!(parse_xkb_rules_names(b"evdev\0pc105\0\0\0\0"), None);

		let d = parse_default_keyboard(
			"# comment\nXKBMODEL=\"pc105\"\nXKBLAYOUT=\"tr\"\nXKBVARIANT=\"f\"\nXKBOPTIONS=\"\"\n",
		)
		.unwrap();
		assert_eq!((d.model.as_str(), d.layout.as_str(), d.variant.as_str()), ("pc105", "tr", "f"));
		assert_eq!(parse_default_keyboard("BACKSPACE=guess\n"), None);

		let c = parse_xorg_keyboard_conf(concat!(
			"Section \"InputClass\"\n",
			"  Identifier \"system-keyboard\"\n",
			"  MatchIsKeyboard \"on\"\n",
			"  Option \"XkbLayout\" \"de\"\n",
			"  Option \"XkbVariant\" \"nodeadkeys\"\n",
			"#  Option \"XkbLayout\" \"us\"\n",
			"EndSection\n",
		))
		.unwrap();
		assert_eq!((c.layout.as_str(), c.variant.as_str()), ("de", "nodeadkeys"));
	}

	#[test]
	fn unicode_entry_only_where_an_input_method_reads_it() {
		let env = |pairs: &'static [(&'static str, &'static str)]| {
			move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
		};
		assert!(unicode_entry_supported(env(&[("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")])));
		assert!(unicode_entry_supported(env(&[("XDG_CURRENT_DESKTOP", "KDE"), ("QT_IM_MODULE", "ibus")])));
		assert!(!unicode_entry_supported(env(&[("XDG_CURRENT_DESKTOP", "KDE")])));
		assert!(!unicode_entry_supported(env(&[("XDG_CURRENT_DESKTOP", "sway")])));
	}

	#[test]
	fn client_keys_are_unique_and_in_the_uinput_range() {
		let mut seen = std::collections::HashSet::new();
		for &k in CLIENT_KEYS {
			assert!(seen.insert(k), "duplicate {k}");
			assert!((1..=248).contains(&k), "{k} outside the uinput keyboard");
		}
	}

	#[test]
	fn mac_table_covers_every_client_key() {
		for &k in CLIENT_KEYS {
			let mapped = mac_vk(k as u32, false).is_some();
			assert_eq!(mapped, !MAC_UNMAPPED.contains(&k), "evdev {k}");
		}
	}

	#[test]
	fn mac_iso_swaps_section_and_grave() {
		assert_eq!(mac_vk(41, false), Some(0x32));
		assert_eq!(mac_vk(86, false), Some(0x0A));
		assert_eq!(mac_vk(41, true), Some(0x0A));
		assert_eq!(mac_vk(86, true), Some(0x32));
	}

	#[test]
	fn mac_table_is_injective_apart_from_documented_aliases() {
		// Print Screen / Scroll Lock / Pause share F13 / F14 / F15, and KP comma shares the JIS
		// keypad comma — every other evdev code has its own virtual key.
		let aliases = [(99u16, 183u16), (70, 184), (119, 185), (121, 95)];
		let mut by_vk: HashMap<u16, u16> = HashMap::new();
		for &k in CLIENT_KEYS {
			let Some(vk) = mac_vk(k as u32, false) else { continue };
			if let Some(&prev) = by_vk.get(&vk) {
				assert!(
					aliases.contains(&(k, prev)) || aliases.contains(&(prev, k)),
					"evdev {prev} and {k} both map to {vk:#x}"
				);
			} else {
				by_vk.insert(vk, k);
			}
		}
	}
}
