//! Wejście z urządzeń `evdev` (`/dev/input/event*`) — rodzime wejście dla trybu DRM.
//!
//! W trybie nested wejście przychodzi od nadrzędnego kompozytora. Gdy Selene działa
//! bezpośrednio na DRM/KMS, nie ma rodzica — wtedy czytamy zdarzenia wprost z urządzeń
//! wejściowych i tłumaczymy je na [`libluna_core::Event`] (ten sam strumień, który
//! trafia do `seat::handle_input`).
//!
//! Zakres: wskaźnik (względny), przyciski myszy, kółko i klawiatura. Śledzimy wirtualny
//! kursor w kompozytorze (evdev daje przemieszczenia, nie pozycję bezwzględną).

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use libluna_core::{
    ButtonState, Event, KeyEvent, Modifiers, Point, PointerButton, PointerEvent, ScrollEvent,
};
use rustix::fs::{Mode, OFlags};

// Typy zdarzeń evdev.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;

/// `struct input_event` (64-bit): `timeval` (2×i64) + typ + kod + wartość.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawEvent {
    time_sec: i64,
    time_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

/// Jedno urządzenie wejściowe.
struct Device {
    fd: OwnedFd,
    #[allow(dead_code)]
    path: String,
}

/// `EVIOCGLED` — `_IOR('E', 0x19, unsigned long)` (odczyt stanu diod).
const EVIOCGLED: u32 = 0x8008_4519;

/// Odczyt `unsigned long` przez ioctl (bez dodatkowych zależności).
struct LedQuery(MaybeUninit<u64>);

unsafe impl rustix::ioctl::Ioctl for LedQuery {
    type Output = u64;
    const OPCODE: rustix::ioctl::Opcode =
        rustix::ioctl::Opcode::old(EVIOCGLED as rustix::ioctl::RawOpcode);
    const IS_MUTATING: bool = true;

    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        self.0.as_mut_ptr().cast()
    }

    unsafe fn output_from_ptr(
        _: rustix::ioctl::IoctlOutput,
        extract: *mut core::ffi::c_void,
    ) -> rustix::io::Result<u64> {
        Ok(*(extract as *const u64))
    }
}

/// Stan diod klawiatury: bit 0 = NumLock, bit 1 = CapsLock.
fn led_bits(fd: impl AsFd) -> Option<u64> {
    unsafe { rustix::ioctl::ioctl(fd, LedQuery(MaybeUninit::new(0))) }.ok()
}

/// Zwraca `(num_lock, caps_lock)` z pierwszego urządzenia, które umie podać LED-y.
fn initial_locks(devices: &[Device]) -> (bool, bool) {
    for dev in devices {
        if let Some(bits) = led_bits(&dev.fd) {
            return (bits & 1 != 0, bits & 2 != 0);
        }
    }
    (false, false)
}

/// Wejście evdev: zestaw urządzeń + wirtualny kursor.
pub struct EvdevInput {
    devices: Vec<Device>,
    cursor: Point,
    acc_dx: f64,
    acc_dy: f64,
    acc_scroll_x: f64,
    acc_scroll_y: f64,
    /// Mapa klawiatury użytkownika (ten sam układ, który dostają klienci) i jej stan.
    /// Bez tego launcher tłumaczył kody fizyczne na litery **amerykańskiego** układu —
    /// na polskim klawisze `y`/`z` (i wszystkie znaki z AltGr) wpisywały coś innego.
    /// `Keymap` musi żyć razem z `State` — stan trzyma wskaźnik do mapy.
    #[allow(dead_code)]
    keymap: Option<xkbcommon::xkb::Keymap>,
    xkb: Option<xkbcommon::xkb::State>,
}

impl EvdevInput {
    /// Otwiera wszystkie czytelne urządzenia `/dev/input/event*`.
    /// Zwraca `None`, gdy nie ma dostępnych urządzeń (np. brak uprawnień).
    pub fn open() -> Option<Self> {
        let dir = std::fs::read_dir("/dev/input").ok()?;
        let mut paths: Vec<String> = dir
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("event"))
                    .unwrap_or(false)
            })
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        paths.sort();

        let mut devices = Vec::new();
        for path in paths {
            let Ok(fd) = rustix::fs::open(
                path.as_str(),
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) else {
                continue;
            };
            devices.push(Device { fd, path });
        }
        if devices.is_empty() {
            return None;
        }
        let locks = initial_locks(&devices);
        // Ten sam układ klawiatury, który Selene wysyła klientom (patrz `seat::keymap_text`).
        // Dzięki temu powłoka (wyszukiwarka launchera, skróty) rozumie te same znaki.
        let (keymap, xkb) = match xkbcommon::xkb::Keymap::new_from_string(
            &xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS),
            crate::seat::keymap_text(),
            xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) {
            Some(km) => {
                let mut state = xkbcommon::xkb::State::new(&km);
                // Zsynchronizuj blokady z diodami — kernel mógł mieć NumLock/CapsLock
                // włączone **przed** startem sesji, a świeży stan xkb zaczyna od zera.
                if locks.0 {
                    tap_lock(&mut state, 69); // KEY_NUMLOCK
                }
                if locks.1 {
                    tap_lock(&mut state, 58); // KEY_CAPSLOCK
                }
                (Some(km), Some(state))
            }
            None => (None, None),
        };
        Some(Self {
            devices,
            cursor: Point::new(0.0, 0.0),
            acc_dx: 0.0,
            acc_dy: 0.0,
            acc_scroll_x: 0.0,
            acc_scroll_y: 0.0,
            keymap,
            xkb,
        })
    }

    /// Surowe deskryptory do `poll` (do wykrywania gotowości).
    pub fn fds(&self) -> Vec<std::os::fd::RawFd> {
        self.devices.iter().map(|d| d.fd.as_raw_fd()).collect()
    }

    /// Bieżąca pozycja wirtualnego kursora (współrzędne logiczne).
    pub fn cursor(&self) -> Point {
        self.cursor
    }

    /// Początkowy stan blokad `(num_lock, caps_lock)` z diod klawiatury.
    pub fn locks(&self) -> (bool, bool) {
        initial_locks(&self.devices)
    }

    /// Czyta i tłumaczy wszystkie oczekujące zdarzenia.
    ///
    /// `bounds` to rozmiar monitora — kursor jest do niego ograniczany.
    pub fn read_events(&mut self, bounds: (f64, f64)) -> Vec<Event> {
        // Najpierw odczyt (pożyczka współdzielona urządzeń), potem tłumaczenie (mutacja).
        let mut decoded = Vec::new();
        for dev in &self.devices {
            let mut buf = [0u8; std::mem::size_of::<RawEvent>() * 64];
            loop {
                match rustix::io::read(&dev.fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for chunk in buf[..n].chunks_exact(std::mem::size_of::<RawEvent>()) {
                            if let Some(ev) = decode(chunk) {
                                decoded.push(ev);
                            }
                        }
                    }
                    Err(_) => break, // EAGAIN lub błąd — kończymy na tym urządzeniu
                }
            }
        }
        let mut out = Vec::new();
        for ev in decoded {
            self.apply(ev, bounds, &mut out);
        }
        out
    }

    fn apply(&mut self, ev: Decoded, bounds: (f64, f64), out: &mut Vec<Event>) {
        match ev {
            Decoded::Motion(dx, dy) => {
                self.acc_dx += dx;
                self.acc_dy += dy;
            }
            Decoded::Scroll(dx, dy) => {
                self.acc_scroll_x += dx;
                self.acc_scroll_y += dy;
            }
            Decoded::Button(button, pressed) => {
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                out.push(Event::PointerButton(PointerEvent {
                    position: self.cursor,
                    button,
                    state,
                    modifiers: Modifiers::NONE,
                }));
            }
            Decoded::Key(code, fallback_keysym, pressed) => {
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                let (keysym, text) = self.translate_key(code, fallback_keysym, pressed);
                out.push(Event::Key(KeyEvent {
                    keysym,
                    keycode: code as u32,
                    text,
                    state,
                    modifiers: Modifiers::NONE,
                    repeat: false,
                }));
            }
            Decoded::Syn => {
                if self.acc_dx != 0.0 || self.acc_dy != 0.0 {
                    self.cursor = Point::new(
                        (self.cursor.x + self.acc_dx).clamp(0.0, (bounds.0 - 1.0).max(0.0)),
                        (self.cursor.y + self.acc_dy).clamp(0.0, (bounds.1 - 1.0).max(0.0)),
                    );
                    self.acc_dx = 0.0;
                    self.acc_dy = 0.0;
                    out.push(Event::PointerMotion {
                        position: self.cursor,
                        modifiers: Modifiers::NONE,
                    });
                }
                if self.acc_scroll_x != 0.0 || self.acc_scroll_y != 0.0 {
                    out.push(Event::Scroll(ScrollEvent {
                        position: self.cursor,
                        dx: self.acc_scroll_x,
                        dy: -self.acc_scroll_y,
                    }));
                    self.acc_scroll_x = 0.0;
                    self.acc_scroll_y = 0.0;
                }
            }
        }
    }

    /// Tłumaczy fizyczny kod `evdev` na keysym (i tekst) według układu użytkownika.
    ///
    /// `fallback_keysym` pochodzi z wbudowanej tabeli (dla klawiszy specjalnych
    /// i jako awaryjna ścieżka, gdy xkbcommon nie ma mapy).
    fn translate_key(
        &mut self,
        code: u16,
        fallback_keysym: u32,
        pressed: bool,
    ) -> (u32, Option<String>) {
        use xkbcommon::xkb;
        let Some(state) = self.xkb.as_mut() else {
            return (fallback_keysym, None);
        };
        let kc = xkb::Keycode::new(u32::from(code) + 8);
        let dir = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        state.update_key(kc, dir);
        let sym = state.key_get_one_sym(kc).raw();
        let keysym = if sym != 0 { sym } else { fallback_keysym };
        let text = if pressed {
            let s = state.key_get_utf8(kc);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        } else {
            None
        };
        (keysym, text)
    }
}

/// Symuluje pełne naciśnięcie klawisza blokady (aktualizuje `locked_mods` xkb).
fn tap_lock(state: &mut xkbcommon::xkb::State, code: u16) {
    use xkbcommon::xkb;
    let kc = xkb::Keycode::new(u32::from(code) + 8);
    state.update_key(kc, xkb::KeyDirection::Down);
    state.update_key(kc, xkb::KeyDirection::Up);
}

/// Zdarzenie po przetłumaczeniu z bajtów.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Decoded {
    Motion(f64, f64),
    Scroll(f64, f64),
    Button(PointerButton, bool),
    /// Klawisz: fizyczny kod evdev, jego keysym oraz stan.
    Key(u16, u32, bool),
    Syn,
}

/// Dekoduje jeden `input_event`.
fn decode(bytes: &[u8]) -> Option<Decoded> {
    if bytes.len() < std::mem::size_of::<RawEvent>() {
        return None;
    }
    let ev = RawEvent {
        time_sec: i64::from_ne_bytes(bytes[0..8].try_into().ok()?),
        time_usec: i64::from_ne_bytes(bytes[8..16].try_into().ok()?),
        type_: u16::from_ne_bytes(bytes[16..18].try_into().ok()?),
        code: u16::from_ne_bytes(bytes[18..20].try_into().ok()?),
        value: i32::from_ne_bytes(bytes[20..24].try_into().ok()?),
    };
    match ev.type_ {
        EV_SYN if ev.code == SYN_REPORT => Some(Decoded::Syn),
        EV_REL => match ev.code {
            REL_X => Some(Decoded::Motion(ev.value as f64, 0.0)),
            REL_Y => Some(Decoded::Motion(0.0, ev.value as f64)),
            REL_HWHEEL => Some(Decoded::Scroll(ev.value as f64, 0.0)),
            REL_WHEEL => Some(Decoded::Scroll(0.0, ev.value as f64)),
            _ => None,
        },
        EV_KEY => {
            let pressed = ev.value != 0;
            match ev.code {
                0x110 => Some(Decoded::Button(PointerButton::Left, pressed)),
                0x111 => Some(Decoded::Button(PointerButton::Right, pressed)),
                0x112 => Some(Decoded::Button(PointerButton::Middle, pressed)),
                // „Wstecz/dalej” bywa raportowane jako BTN_SIDE/BTN_EXTRA
                // (większość myszy, X11: przyciski 8/9) albo BTN_BACK/FORWARD.
                0x113 | 0x116 => Some(Decoded::Button(PointerButton::Back, pressed)),
                0x114 | 0x115 => Some(Decoded::Button(PointerButton::Forward, pressed)),
                // Pozostałe `BTN_*` (touchpad, tablet, pióro, joystick) **nie** są
                // klawiszami — bez tego filtr przesyłał dotknięcia panelu jako klawisze.
                0x100..=0x15f => None,
                // **Każdy** inny klawisz przekazujemy dalej. Wcześniej wymagaliśmy,
                // by kod był w ręcznej tabeli `evdev_to_keysym`; nie było tam m.in.
                // `; ' / \ [ ] - = , .` ani klawiatury numerycznej, więc te klawisze
                // były **po cichu wyrzucane** (a z nimi `? : " | { } _ +` i numpad).
                // Keysym i tekst i tak liczy xkbcommon według układu użytkownika;
                // tabela jest tylko awaryjnym fallbackiem.
                code => Some(Decoded::Key(
                    code,
                    evdev_to_keysym(code).unwrap_or(0),
                    pressed,
                )),
            }
        }
        _ => None,
    }
}

/// Kod evdev → keysym (podzbiór wspólny z mapą klawiatury Selene).
fn evdev_to_keysym(code: u16) -> Option<u32> {
    // Litery (evdev: a=30 … l=38, m=50 …; użyto standardowego układu evdev).
    let keysym = match code {
        1 => 0xff1b,   // Escape
        14 => 0xff08,  // BackSpace
        15 => 0xff09,  // Tab
        28 => 0xff0d,  // Return
        57 => 0x0020,  // space
        103 => 0xff52, // Up
        105 => 0xff51, // Left
        106 => 0xff53, // Right
        108 => 0xff54, // Down
        102 => 0xff50, // Home (KEY_HOME)
        107 => 0xff57, // End (KEY_END)
        104 => 0xff55, // Prior/Page_Up
        109 => 0xff56, // Next/Page_Down
        110 => 0xff63, // Insert
        111 => 0xffff, // Delete
        // Klawisze funkcyjne F1..F12 (bez nich skróty z F-kami nie docierały do powłoki).
        59 => 0xffbe, // F1
        60 => 0xffbf, // F2
        61 => 0xffc0, // F3
        62 => 0xffc1, // F4
        63 => 0xffc2, // F5
        64 => 0xffc3, // F6
        65 => 0xffc4, // F7
        66 => 0xffc5, // F8
        67 => 0xffc6, // F9
        68 => 0xffc7, // F10
        87 => 0xffc8, // F11
        88 => 0xffc9, // F12 (Super+F12 = zrzut ekranu)
        // Multimedialne (głośność/wyciszenie) — kompozytor obsługuje je sam.
        99 => 0xff61,      // Print (SysRq)
        113 => 0x1008ff12, // XF86AudioMute
        114 => 0x1008ff11, // XF86AudioLowerVolume
        115 => 0x1008ff13, // XF86AudioRaiseVolume
        // cyfry 1..0
        2 => 0x0031,
        3 => 0x0032,
        4 => 0x0033,
        5 => 0x0034,
        6 => 0x0035,
        7 => 0x0036,
        8 => 0x0037,
        9 => 0x0038,
        10 => 0x0039,
        11 => 0x0030,
        // litery
        30 => 0x0061, // a
        48 => 0x0062, // b
        46 => 0x0063, // c
        32 => 0x0064, // d
        18 => 0x0065, // e
        33 => 0x0066, // f
        34 => 0x0067, // g
        35 => 0x0068, // h
        23 => 0x0069, // i
        36 => 0x006a, // j
        37 => 0x006b, // k
        38 => 0x006c, // l
        50 => 0x006d, // m
        49 => 0x006e, // n
        24 => 0x006f, // o
        25 => 0x0070, // p
        16 => 0x0071, // q
        19 => 0x0072, // r
        31 => 0x0073, // s
        20 => 0x0074, // t
        22 => 0x0075, // u
        47 => 0x0076, // v
        17 => 0x0077, // w
        45 => 0x0078, // x
        21 => 0x0079, // y
        44 => 0x007a, // z
        // modyfikatory
        42 | 54 => 0xffe1,   // Shift_L / Shift_R
        29 | 97 => 0xffe3,   // Control_L / Control_R
        56 => 0xffe9,        // Alt_L (Mod1)
        100 => 0xfe03,       // Alt_R → ISO_Level3_Shift (AltGr)
        125 | 126 => 0xffeb, // Super_L / Super_R
        _ => return None,
    };
    Some(keysym)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(type_: u16, code: u16, value: i32) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[16..18].copy_from_slice(&type_.to_ne_bytes());
        b[18..20].copy_from_slice(&code.to_ne_bytes());
        b[20..24].copy_from_slice(&value.to_ne_bytes());
        b
    }

    #[test]
    fn raw_event_layout_matches_kernel() {
        assert_eq!(std::mem::size_of::<RawEvent>(), 24);
    }

    #[test]
    fn evdev_path_handles_shifted_symbols_and_numpad() {
        use std::io::Write;
        use xkbcommon::xkb;
        let (mut writer, reader) = std::os::unix::net::UnixStream::pair().expect("socket");
        reader.set_nonblocking(true).expect("nonblocking");
        let km = xkb::Keymap::new_from_string(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            crate::seat::keymap_text(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("keymap");
        let mut input = EvdevInput {
            devices: vec![Device {
                fd: OwnedFd::from(reader),
                path: "test".to_owned(),
            }],
            cursor: Point::new(0.0, 0.0),
            acc_dx: 0.0,
            acc_dy: 0.0,
            acc_scroll_x: 0.0,
            acc_scroll_y: 0.0,
            keymap: Some(km.clone()),
            xkb: Some(xkb::State::new(&km)),
        };
        let mut send = |t: u16, c: u16, v: i32| {
            writer.write_all(&raw(t, c, v)).expect("write");
        };
        // Shift + `'` → `"` (wcześniej klawisz 40 był wyrzucany w `decode`).
        send(EV_KEY, 42, 1);
        send(EV_KEY, 40, 1);
        send(EV_KEY, 40, 0);
        send(EV_KEY, 42, 0);
        // NumLock + KP1 → „1”.
        send(EV_KEY, 69, 1);
        send(EV_KEY, 69, 0);
        send(EV_KEY, 79, 1);
        send(EV_KEY, 79, 0);
        send(EV_SYN, SYN_REPORT, 0);

        let events = input.read_events((1920.0, 1080.0));
        let texts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                Event::Key(k) => k.text.clone(),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t == "\""),
            "brak cudzysłowu w zdarzeniach: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "1"),
            "brak cyfry z numpada: {texts:?}"
        );
    }

    #[test]
    fn no_devices_means_no_locks() {
        assert_eq!(initial_locks(&[]), (false, false));
    }

    #[test]
    fn decodes_function_and_media_keys() {
        // Bez tych mapowań skróty z F-kami i klawisze głośności były wyrzucane
        // (F12 w ogóle nie docierał do powłoki, więc Super+F12 nic nie robiło).
        assert_eq!(evdev_to_keysym(88), Some(0xffc9)); // F12
        assert_eq!(evdev_to_keysym(59), Some(0xffbe)); // F1
        assert_eq!(evdev_to_keysym(115), Some(0x1008ff13)); // głośniej
        assert_eq!(evdev_to_keysym(114), Some(0x1008ff11)); // ciszej
        assert_eq!(evdev_to_keysym(113), Some(0x1008ff12)); // wycisz
    }

    #[test]
    fn decodes_punctuation_and_keypad_keys() {
        // Klawisze spoza ręcznej tabeli też muszą dojść do klientów — inaczej
        // `? : " | { } _ +` i cała klawiatura numeryczna znikały w sesji evdev.
        let codes: [u16; 18] = [
            39, // ;
            40, // '
            51, // ,
            52, // .
            53, // /
            12, // -
            13, // =
            26, // [
            27, // ]
            43, // \
            86, // < (102nd)
            69, // NumLock
            79, // KP1
            80, // KP2
            82, // KP0
            83, // KP.
            96, // KP Enter
            98, // KP /
        ];
        for code in codes {
            match decode(&raw(EV_KEY, code, 1)) {
                Some(Decoded::Key(c, _, true)) => assert_eq!(c, code),
                other => panic!("klawisz {code} został wyrzucony: {other:?}"),
            }
        }
    }

    #[test]
    fn xkb_gives_text_for_punctuation_and_numpad() {
        use xkbcommon::xkb;
        let km = xkb::Keymap::new_from_string(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            crate::seat::keymap_text(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("keymap");
        let mut st = xkb::State::new(&km);
        fn tap(st: &mut xkb::State, code: u16, down: bool) {
            st.update_key(
                xkb::Keycode::new(u32::from(code) + 8),
                if down {
                    xkb::KeyDirection::Down
                } else {
                    xkb::KeyDirection::Up
                },
            );
        }
        // Shift + `'` → `"` (klawisz 40 nie był w tabeli fallbacku).
        tap(&mut st, 42, true);
        tap(&mut st, 40, true);
        assert_eq!(st.key_get_utf8(xkb::Keycode::new(48)), "\"");
        tap(&mut st, 40, false);
        tap(&mut st, 42, false);
        // Shift + `,` → `<`.
        tap(&mut st, 42, true);
        tap(&mut st, 51, true);
        assert_eq!(st.key_get_utf8(xkb::Keycode::new(59)), "<");
        tap(&mut st, 51, false);
        tap(&mut st, 42, false);
        // NumLock, potem KP1 → „1”.
        tap(&mut st, 69, true);
        tap(&mut st, 69, false);
        tap(&mut st, 79, true);
        assert_eq!(st.key_get_utf8(xkb::Keycode::new(87)), "1");
        tap(&mut st, 79, false);
    }

    #[test]
    fn decodes_button_and_key_and_rel() {
        assert_eq!(
            decode(&raw(EV_KEY, 0x110, 1)),
            Some(Decoded::Button(PointerButton::Left, true))
        );
        // Boki myszy — oba warianty kodów (BTN_SIDE/EXTRA oraz BTN_BACK/FORWARD).
        assert_eq!(
            decode(&raw(EV_KEY, 0x113, 1)),
            Some(Decoded::Button(PointerButton::Back, true))
        );
        assert_eq!(
            decode(&raw(EV_KEY, 0x114, 1)),
            Some(Decoded::Button(PointerButton::Forward, true))
        );
        assert_eq!(
            decode(&raw(EV_KEY, 0x116, 1)),
            Some(Decoded::Button(PointerButton::Back, true))
        );
        assert_eq!(
            decode(&raw(EV_KEY, 0x115, 1)),
            Some(Decoded::Button(PointerButton::Forward, true))
        );
        assert_eq!(
            decode(&raw(EV_KEY, 30, 0)),
            Some(Decoded::Key(30, 0x0061, false))
        );
        assert_eq!(
            decode(&raw(EV_REL, REL_X, 5)),
            Some(Decoded::Motion(5.0, 0.0))
        );
        assert_eq!(decode(&raw(EV_SYN, SYN_REPORT, 0)), Some(Decoded::Syn));
        assert_eq!(
            decode(&raw(EV_KEY, 0x14a, 1)),
            None // BTN_TOUCH z touchpada — nie klawisz
        );
        // Nieznany kod nie jest już wyrzucany: klient dostaje fizyczny klawisz,
        // a tekst i tak liczy z mapy klawiatury (fallback keysym = 0).
        assert_eq!(
            decode(&raw(EV_KEY, 0x2ff, 1)),
            Some(Decoded::Key(0x2ff, 0, true))
        );
    }

    #[test]
    fn accumulates_motion_and_emits_on_syn() {
        let mut input = EvdevInput {
            devices: Vec::new(),
            cursor: Point::new(100.0, 100.0),
            acc_dx: 0.0,
            acc_dy: 0.0,
            acc_scroll_x: 0.0,
            acc_scroll_y: 0.0,
            keymap: None,
            xkb: None,
        };
        let mut out = Vec::new();
        input.apply(Decoded::Motion(10.0, 5.0), (1280.0, 800.0), &mut out);
        assert!(out.is_empty()); // jeszcze bez SYN
        input.apply(Decoded::Syn, (1280.0, 800.0), &mut out);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0], Event::PointerMotion { position, .. } if position.x == 110.0 && position.y == 105.0)
        );
    }

    #[test]
    fn cursor_is_clamped_to_bounds() {
        let mut input = EvdevInput {
            devices: Vec::new(),
            cursor: Point::new(1270.0, 0.0),
            acc_dx: 0.0,
            acc_dy: 0.0,
            acc_scroll_x: 0.0,
            acc_scroll_y: 0.0,
            keymap: None,
            xkb: None,
        };
        let mut out = Vec::new();
        input.apply(Decoded::Motion(1000.0, -50.0), (1280.0, 800.0), &mut out);
        input.apply(Decoded::Syn, (1280.0, 800.0), &mut out);
        assert!(
            matches!(out[0], Event::PointerMotion { position, .. } if position.x == 1279.0 && position.y == 0.0)
        );
    }
}
