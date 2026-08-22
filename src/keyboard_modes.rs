const KITTY_STACK_DEPTH: usize = 8;
const KITTY_FLAGS_MASK: u16 = 0b1_1111;
const KITTY_DISAMBIGUATE: u16 = 1;
const KITTY_REPORT_EVENTS: u16 = 1 << 1;
const KITTY_REPORT_ALTERNATES: u16 = 1 << 2;
const KITTY_REPORT_ALL: u16 = 1 << 3;

pub const MOD_SHIFT: u16 = 1 << 0;
pub const MOD_ALT: u16 = 1 << 1;
pub const MOD_CTRL: u16 = 1 << 2;
pub const MOD_SUPER: u16 = 1 << 3;
pub const MOD_HYPER: u16 = 1 << 4;
pub const MOD_META: u16 = 1 << 5;
pub const MOD_CAPS_LOCK: u16 = 1 << 6;
pub const MOD_NUM_LOCK: u16 = 1 << 7;
pub const MOD_MASK: u16 = (1 << 8) - 1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyboardModeState {
    kitty_stack: Vec<u16>,
    pub modify_other_keys: u8,
    pub application_cursor: bool,
}

impl KeyboardModeState {
    pub fn kitty_flags(&self) -> u16 {
        self.kitty_stack.last().copied().unwrap_or(0)
    }
    pub fn restore_sequences(&self) -> Vec<u8> {
        let mut output = Vec::new();
        if let Some((&bottom, rest)) = self.kitty_stack.split_first() {
            output.extend_from_slice(format!("\x1b[={bottom}u").as_bytes());
            for &flags in rest {
                output.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
            }
        }
        if self.modify_other_keys != 0 {
            output.extend_from_slice(format!("\x1b[>4;{}m", self.modify_other_keys).as_bytes());
        }
        if self.application_cursor {
            output.extend_from_slice(b"\x1b[?1h");
        }
        output
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KittySetMode {
    Set,
    Or,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeSequence {
    PushKitty(u16),
    PopKitty(u16),
    SetKitty { flags: u16, mode: KittySetMode },
    ModifyOtherKeys(u8),
    ApplicationCursor(bool),
    AlternateScreen(bool),
    Reset,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScanPhase {
    #[default]
    Normal,
    Esc,
    Csi,
}

#[derive(Clone, Debug, Default)]
pub struct KeyboardModeTracker {
    main: KeyboardModeState,
    alternate: KeyboardModeState,
    alternate_screen: bool,
    phase: ScanPhase,
    csi: Vec<u8>,
}

impl KeyboardModeTracker {
    pub fn active(&self) -> &KeyboardModeState {
        if self.alternate_screen {
            &self.alternate
        } else {
            &self.main
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) -> bool {
        let before = (self.active().clone(), self.alternate_screen);
        for &byte in bytes {
            match self.phase {
                ScanPhase::Normal if byte == 0x1b => self.phase = ScanPhase::Esc,
                ScanPhase::Normal => {}
                ScanPhase::Esc if byte == b'[' => {
                    self.csi.clear();
                    self.phase = ScanPhase::Csi;
                }
                ScanPhase::Esc if byte == b'c' => {
                    self.apply(ModeSequence::Reset);
                    self.phase = ScanPhase::Normal;
                }
                ScanPhase::Esc if byte == 0x1b => {}
                ScanPhase::Esc => self.phase = ScanPhase::Normal,
                ScanPhase::Csi if (0x40..=0x7e).contains(&byte) => {
                    if let Some(sequence) = detect_sequence(&self.csi, byte) {
                        self.apply(sequence);
                    }
                    self.csi.clear();
                    self.phase = ScanPhase::Normal;
                }
                ScanPhase::Csi if (0x20..=0x3f).contains(&byte) && self.csi.len() < 64 => {
                    self.csi.push(byte);
                }
                ScanPhase::Csi if byte == 0x1b => {
                    self.csi.clear();
                    self.phase = ScanPhase::Esc;
                }
                ScanPhase::Csi => {
                    self.csi.clear();
                    self.phase = ScanPhase::Normal;
                }
            }
        }
        before != (self.active().clone(), self.alternate_screen)
    }

    fn active_mut(&mut self) -> &mut KeyboardModeState {
        if self.alternate_screen {
            &mut self.alternate
        } else {
            &mut self.main
        }
    }

    fn apply(&mut self, sequence: ModeSequence) {
        match sequence {
            ModeSequence::AlternateScreen(enabled) => self.alternate_screen = enabled,
            ModeSequence::Reset => *self = Self::default(),
            ModeSequence::PushKitty(flags) => {
                let state = self.active_mut();
                state.kitty_stack.push(flags);
                if state.kitty_stack.len() > KITTY_STACK_DEPTH {
                    state.kitty_stack.remove(0);
                }
            }
            ModeSequence::PopKitty(count) => {
                let state = self.active_mut();
                if count as usize >= KITTY_STACK_DEPTH {
                    state.kitty_stack.clear();
                } else {
                    let keep = state.kitty_stack.len().saturating_sub(count as usize);
                    state.kitty_stack.truncate(keep);
                }
            }
            ModeSequence::SetKitty { flags, mode } => {
                let state = self.active_mut();
                let current = state.kitty_flags();
                let merged = match mode {
                    KittySetMode::Set => flags,
                    KittySetMode::Or => current | flags,
                    KittySetMode::Not => current & !flags,
                };
                if let Some(top) = state.kitty_stack.last_mut() {
                    *top = merged;
                } else if merged != 0 {
                    state.kitty_stack.push(merged);
                }
            }
            ModeSequence::ModifyOtherKeys(value) => {
                self.active_mut().modify_other_keys = value;
            }
            ModeSequence::ApplicationCursor(enabled) => {
                self.active_mut().application_cursor = enabled;
            }
        }
    }
}

fn detect_sequence(params: &[u8], final_byte: u8) -> Option<ModeSequence> {
    match final_byte {
        b'u' => detect_kitty(params),
        b'm' => detect_modify_other_keys(params),
        b'h' | b'l' => detect_private_mode(params, final_byte == b'h'),
        _ => None,
    }
}

fn detect_kitty(params: &[u8]) -> Option<ModeSequence> {
    let (&prefix, rest) = params.split_first()?;
    match prefix {
        b'>' => parse_flags(rest).map(ModeSequence::PushKitty),
        b'<' => {
            let count = if rest.is_empty() { 1 } else { parse_u16(rest)? };
            Some(ModeSequence::PopKitty(count))
        }
        b'=' => {
            let (flags_raw, mode) = match rest.iter().position(|&byte| byte == b';') {
                None => (rest, KittySetMode::Set),
                Some(separator) => {
                    let mode_raw = &rest[separator + 1..];
                    let mode = match if mode_raw.is_empty() {
                        1
                    } else {
                        parse_u16(mode_raw)?
                    } {
                        1 => KittySetMode::Set,
                        2 => KittySetMode::Or,
                        3 => KittySetMode::Not,
                        _ => return None,
                    };
                    (&rest[..separator], mode)
                }
            };
            Some(ModeSequence::SetKitty {
                flags: parse_flags(flags_raw)?,
                mode,
            })
        }
        _ => None,
    }
}

fn detect_modify_other_keys(params: &[u8]) -> Option<ModeSequence> {
    let rest = params.strip_prefix(b">4")?;
    let value = if rest.is_empty() {
        0
    } else {
        parse_u16(rest.strip_prefix(b";")?)?
    };
    (value <= 2).then_some(ModeSequence::ModifyOtherKeys(value as u8))
}

fn detect_private_mode(params: &[u8], enabled: bool) -> Option<ModeSequence> {
    match params.strip_prefix(b"?")? {
        b"1" => Some(ModeSequence::ApplicationCursor(enabled)),
        b"47" | b"1047" | b"1049" => Some(ModeSequence::AlternateScreen(enabled)),
        _ => None,
    }
}

fn parse_flags(bytes: &[u8]) -> Option<u16> {
    let flags = if bytes.is_empty() {
        0
    } else {
        parse_u16(bytes)?
    };
    (flags & !KITTY_FLAGS_MASK == 0).then_some(flags)
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || bytes.len() > 5 {
        return None;
    }
    let mut value = 0u32;
    for &byte in bytes {
        value = value
            .checked_mul(10)?
            .checked_add((byte as char).to_digit(10)?)?;
    }
    u16::try_from(value).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticKey {
    Unicode(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Repeat(u16),
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticKeyEvent {
    pub key: SemanticKey,
    pub modifiers: u16,
    pub action: KeyAction,
}

pub fn parse_send_keys_token(token: &str) -> Option<SemanticKeyEvent> {
    let mut rest = token;
    let mut modifiers = 0u16;
    loop {
        let Some((prefix, tail)) = rest.split_once('-') else {
            break;
        };
        let bit = match prefix.to_ascii_lowercase().as_str() {
            "s" | "shift" => MOD_SHIFT,
            "m" | "alt" => MOD_ALT,
            "c" | "ctrl" | "control" => MOD_CTRL,
            "super" => MOD_SUPER,
            "hyper" => MOD_HYPER,
            "meta" => MOD_META,
            _ => break,
        };
        if modifiers & bit != 0 {
            return None;
        }
        modifiers |= bit;
        rest = tail;
    }

    let lower = rest.to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" | "cr" => SemanticKey::Enter,
        "tab" => SemanticKey::Tab,
        "btab" | "backtab" => {
            modifiers |= MOD_SHIFT;
            SemanticKey::Tab
        }
        "bspace" | "backspace" => SemanticKey::Backspace,
        "escape" | "esc" => SemanticKey::Escape,
        "up" => SemanticKey::ArrowUp,
        "down" => SemanticKey::ArrowDown,
        "left" => SemanticKey::ArrowLeft,
        "right" => SemanticKey::ArrowRight,
        "home" => SemanticKey::Home,
        "end" => SemanticKey::End,
        "pageup" | "ppage" => SemanticKey::PageUp,
        "pagedown" | "npage" => SemanticKey::PageDown,
        "insert" | "ic" => SemanticKey::Insert,
        "delete" | "dc" => SemanticKey::Delete,
        "space" => SemanticKey::Unicode(' '),
        value if value.starts_with('f') => {
            let number = value[1..].parse::<u8>().ok()?;
            if !(1..=12).contains(&number) {
                return None;
            }
            SemanticKey::Function(number)
        }
        _ => {
            let mut chars = rest.chars();
            let character = chars.next()?;
            if chars.next().is_some() || character.is_control() {
                return None;
            }

            SemanticKey::Unicode(character)
        }
    };

    Some(SemanticKeyEvent {
        key,
        modifiers,
        action: KeyAction::Press,
    })
}
pub fn is_semantic_send_keys_token(token: &str) -> bool {
    parse_send_keys_token(token).is_some_and(|event| {
        event.modifiers != 0
            || !matches!(event.key, SemanticKey::Unicode(_))
            || token.eq_ignore_ascii_case("space")
    })
}

pub fn encode_key_event(event: SemanticKeyEvent, mode: &KeyboardModeState) -> Option<Vec<u8>> {
    if event.modifiers & !MOD_MASK != 0 {
        return None;
    }
    let repetitions = match event.action {
        KeyAction::Repeat(count) if count == 0 => return None,
        KeyAction::Repeat(count) => count,
        _ => 1,
    };
    let flags = mode.kitty_flags();
    let event_kind = match event.action {
        KeyAction::Press => 1,
        KeyAction::Repeat(_) => 2,
        KeyAction::Release => 3,
    };
    let one = if flags != 0 {
        encode_kitty(event.key, event.modifiers, event_kind, flags)
    } else if mode.modify_other_keys != 0 {
        encode_modify_other_keys(event.key, event.modifiers, event_kind, mode)
    } else {
        encode_legacy(event.key, event.modifiers, event_kind, mode)
    }?;
    let mut output = Vec::with_capacity(one.len().saturating_mul(repetitions as usize));
    for _ in 0..repetitions {
        output.extend_from_slice(&one);
    }
    Some(output)
}

fn encode_kitty(key: SemanticKey, modifiers: u16, event_kind: u8, flags: u16) -> Option<Vec<u8>> {
    let report_all = flags & KITTY_REPORT_ALL != 0;
    let disambiguate = flags & KITTY_DISAMBIGUATE != 0 || report_all;
    let report_events = flags & KITTY_REPORT_EVENTS != 0;
    if event_kind == 3 && !report_events {
        return None;
    }
    let event_suffix = (report_events && event_kind != 1).then_some(event_kind);

    match key {
        SemanticKey::Unicode(character) => {
            let shortcut_modifiers =
                modifiers & (MOD_ALT | MOD_CTRL | MOD_SUPER | MOD_HYPER | MOD_META);
            if !report_all && (!disambiguate || shortcut_modifiers == 0) {
                if event_kind == 3 {
                    return None;
                }
                return encode_text(character, modifiers);
            }
            let mut key_field = (character.to_ascii_lowercase() as u32).to_string();
            if flags & KITTY_REPORT_ALTERNATES != 0
                && modifiers & MOD_SHIFT != 0
                && character.is_ascii_alphabetic()
            {
                key_field.push(':');
                key_field.push_str(&(character.to_ascii_uppercase() as u32).to_string());
            }
            Some(csi_u(&key_field, modifiers, event_suffix))
        }
        SemanticKey::Enter | SemanticKey::Tab | SemanticKey::Backspace => {
            if !report_all && modifiers & !(MOD_CAPS_LOCK | MOD_NUM_LOCK) == 0 {
                if event_kind == 3 {
                    return None;
                }
                return legacy_control_key(key, modifiers);
            }
            if !report_all && !disambiguate {
                return encode_legacy(key, modifiers, event_kind, &KeyboardModeState::default());
            }
            Some(csi_u(
                &kitty_c0_code(key)?.to_string(),
                modifiers,
                event_suffix,
            ))
        }
        SemanticKey::Escape if disambiguate => Some(csi_u("27", modifiers, event_suffix)),
        SemanticKey::Escape => {
            encode_legacy(key, modifiers, event_kind, &KeyboardModeState::default())
        }
        _ => encode_functional(key, modifiers, event_suffix, false),
    }
}

fn encode_modify_other_keys(
    key: SemanticKey,
    modifiers: u16,
    event_kind: u8,
    mode: &KeyboardModeState,
) -> Option<Vec<u8>> {
    if event_kind == 3 {
        return None;
    }
    let effective = modifiers & !(MOD_CAPS_LOCK | MOD_NUM_LOCK);
    match key {
        SemanticKey::Unicode(character) if effective != 0 => Some(
            format!(
                "\x1b[27;{};{}~",
                modifiers + 1,
                character.to_ascii_lowercase() as u32
            )
            .into_bytes(),
        ),
        SemanticKey::Enter | SemanticKey::Tab | SemanticKey::Backspace | SemanticKey::Escape
            if mode.modify_other_keys == 2 && effective != 0 =>
        {
            Some(format!("\x1b[27;{};{}~", modifiers + 1, kitty_c0_code(key)?).into_bytes())
        }
        _ => encode_legacy(key, modifiers, event_kind, mode),
    }
}

fn encode_legacy(
    key: SemanticKey,
    modifiers: u16,
    event_kind: u8,
    mode: &KeyboardModeState,
) -> Option<Vec<u8>> {
    if event_kind == 3 || modifiers & (MOD_SUPER | MOD_HYPER | MOD_META) != 0 {
        return None;
    }
    match key {
        SemanticKey::Unicode(character) => encode_text(character, modifiers),
        SemanticKey::Enter | SemanticKey::Tab | SemanticKey::Backspace => {
            legacy_control_key(key, modifiers)
        }
        SemanticKey::Escape => {
            let mut bytes = vec![0x1b];
            if modifiers & MOD_ALT != 0 {
                bytes.insert(0, 0x1b);
            }
            Some(bytes)
        }
        _ => encode_functional(key, modifiers, None, mode.application_cursor),
    }
}

fn encode_text(character: char, modifiers: u16) -> Option<Vec<u8>> {
    let mut bytes = if modifiers & MOD_CTRL != 0 {
        vec![ctrl_byte(character)?]
    } else {
        let output = if modifiers & MOD_SHIFT != 0 && character.is_ascii_alphabetic() {
            character.to_ascii_uppercase()
        } else {
            character
        };
        let mut buffer = [0; 4];
        output.encode_utf8(&mut buffer).as_bytes().to_vec()
    };
    if modifiers & MOD_ALT != 0 {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn legacy_control_key(key: SemanticKey, modifiers: u16) -> Option<Vec<u8>> {
    let mut bytes = match key {
        SemanticKey::Enter if modifiers & MOD_CTRL != 0 => vec![b'\n'],
        SemanticKey::Enter => vec![b'\r'],
        SemanticKey::Tab if modifiers & MOD_SHIFT != 0 => b"\x1b[Z".to_vec(),
        SemanticKey::Tab => vec![b'\t'],
        SemanticKey::Backspace if modifiers & MOD_CTRL != 0 => vec![0x08],
        SemanticKey::Backspace => vec![0x7f],
        _ => return None,
    };
    if modifiers & MOD_ALT != 0 {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn encode_functional(
    key: SemanticKey,
    modifiers: u16,
    event_kind: Option<u8>,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    let modifier_value = modifiers + 1;
    let event = event_kind.filter(|kind| *kind != 1);
    let parameter = event
        .map(|kind| format!("{modifier_value}:{kind}"))
        .unwrap_or_else(|| modifier_value.to_string());

    let final_byte = match key {
        SemanticKey::ArrowUp => Some('A'),
        SemanticKey::ArrowDown => Some('B'),
        SemanticKey::ArrowRight => Some('C'),
        SemanticKey::ArrowLeft => Some('D'),
        SemanticKey::Home => Some('H'),
        SemanticKey::End => Some('F'),
        SemanticKey::Function(1) => Some('P'),
        SemanticKey::Function(2) => Some('Q'),
        SemanticKey::Function(4) => Some('S'),
        _ => None,
    };
    if let Some(final_byte) = final_byte {
        if modifiers == 0 && event.is_none() {
            let prefix = if application_cursor { "\x1bO" } else { "\x1b[" };
            return Some(format!("{prefix}{final_byte}").into_bytes());
        }
        return Some(format!("\x1b[1;{parameter}{final_byte}").into_bytes());
    }

    let number = match key {
        SemanticKey::Insert => 2,
        SemanticKey::Delete => 3,
        SemanticKey::PageUp => 5,
        SemanticKey::PageDown => 6,
        SemanticKey::Function(3) => 13,
        SemanticKey::Function(5) => 15,
        SemanticKey::Function(6) => 17,
        SemanticKey::Function(7) => 18,
        SemanticKey::Function(8) => 19,
        SemanticKey::Function(9) => 20,
        SemanticKey::Function(10) => 21,
        SemanticKey::Function(11) => 23,
        SemanticKey::Function(12) => 24,
        _ => return None,
    };
    if modifiers == 0 && event.is_none() {
        Some(format!("\x1b[{number}~").into_bytes())
    } else {
        Some(format!("\x1b[{number};{parameter}~").into_bytes())
    }
}

fn csi_u(key_field: &str, modifiers: u16, event_kind: Option<u8>) -> Vec<u8> {
    match event_kind {
        Some(kind) => format!("\x1b[{key_field};{}:{kind}u", modifiers + 1).into_bytes(),
        None if modifiers == 0 => format!("\x1b[{key_field}u").into_bytes(),
        None => format!("\x1b[{key_field};{}u", modifiers + 1).into_bytes(),
    }
}

fn kitty_c0_code(key: SemanticKey) -> Option<u32> {
    match key {
        SemanticKey::Escape => Some(27),
        SemanticKey::Enter => Some(13),
        SemanticKey::Tab => Some(9),
        SemanticKey::Backspace => Some(127),
        _ => None,
    }
}

fn ctrl_byte(character: char) -> Option<u8> {
    let byte = u8::try_from(character as u32).ok()?;
    match byte {
        b'1' | b'!' => Some(b'1'),
        b'9' | b'(' => Some(b'9'),
        b'0' | b')' => Some(b'0'),
        b'=' | b'+' => Some(b'='),
        b';' | b':' => Some(b';'),
        b'\'' | b'"' => Some(b'\''),
        b',' | b'<' => Some(b','),
        b'.' | b'>' => Some(b'.'),
        b'/' | b'-' => Some(0x1f),
        b'8' | b'?' => Some(0x7f),
        b' ' | b'2' => Some(0),
        b'3'..=b'7' => Some(byte - 0x18),
        b'@'..=b'~' => Some(byte.to_ascii_lowercase() & 0x1f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_handles_split_sequences_and_screen_local_modes() {
        let mut tracker = KeyboardModeTracker::default();
        assert!(!tracker.advance(b"\x1b[>"));
        assert!(tracker.advance(b"7u\x1b[?1h"));
        assert_eq!(tracker.active().kitty_flags(), 7);
        assert!(tracker.active().application_cursor);

        assert!(tracker.advance(b"\x1b[?1049h\x1b[>4;2m"));
        assert_eq!(tracker.active().kitty_flags(), 0);
        assert_eq!(tracker.active().modify_other_keys, 2);
        assert!(tracker.advance(b"\x1b[?1049l"));
        assert_eq!(tracker.active().kitty_flags(), 7);
        assert!(tracker.active().application_cursor);
        assert_eq!(tracker.active().restore_sequences(), b"\x1b[=7u\x1b[?1h");
    }

    #[test]
    fn parses_tmux_tokens_without_collapsing_modifier_unions() {
        assert_eq!(
            parse_send_keys_token("C-S-M-Super-Hyper-Meta-Enter"),
            Some(SemanticKeyEvent {
                key: SemanticKey::Enter,
                modifiers: MOD_CTRL | MOD_SHIFT | MOD_ALT | MOD_SUPER | MOD_HYPER | MOD_META,
                action: KeyAction::Press,
            })
        );
        assert_eq!(
            parse_send_keys_token("C-S--").map(|event| event.key),
            Some(SemanticKey::Unicode('-'))
        );
    }

    #[test]
    fn encodes_kitty_mok_and_legacy_vectors() {
        let mut tracker = KeyboardModeTracker::default();
        tracker.advance(b"\x1b[>7u");
        let kitty = tracker.active();
        let shifted_enter = SemanticKeyEvent {
            key: SemanticKey::Enter,
            modifiers: MOD_SHIFT,
            action: KeyAction::Press,
        };
        assert_eq!(
            encode_key_event(shifted_enter, kitty).unwrap(),
            b"\x1b[13;2u"
        );
        assert_eq!(
            encode_key_event(
                SemanticKeyEvent {
                    key: SemanticKey::ArrowUp,
                    modifiers: MOD_CTRL | MOD_SHIFT | MOD_ALT,
                    action: KeyAction::Repeat(2),
                },
                kitty,
            )
            .unwrap(),
            b"\x1b[1;8:2A\x1b[1;8:2A"
        );

        let mode = KeyboardModeState {
            modify_other_keys: 2,
            ..KeyboardModeState::default()
        };
        assert_eq!(
            encode_key_event(shifted_enter, &mode).unwrap(),
            b"\x1b[27;2;13~"
        );

        let legacy = KeyboardModeState {
            application_cursor: true,
            ..KeyboardModeState::default()
        };
        assert_eq!(
            encode_key_event(
                SemanticKeyEvent {
                    key: SemanticKey::ArrowLeft,
                    modifiers: 0,
                    action: KeyAction::Press,
                },
                &legacy,
            )
            .unwrap(),
            b"\x1bOD"
        );
    }

    #[test]
    fn release_requires_kitty_event_reporting() {
        let event = SemanticKeyEvent {
            key: SemanticKey::ArrowDown,
            modifiers: MOD_CTRL | MOD_SHIFT,
            action: KeyAction::Release,
        };
        let mut tracker = KeyboardModeTracker::default();
        tracker.advance(b"\x1b[>1u");
        assert_eq!(encode_key_event(event, tracker.active()), None);
        tracker.advance(b"\x1b[=3u");
        assert_eq!(
            encode_key_event(event, tracker.active()).unwrap(),
            b"\x1b[1;6:3B"
        );
    }
}
