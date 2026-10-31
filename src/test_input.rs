//! Skryptowane wejście do testów (`SELENE_INPUT_SCRIPT`).
//!
//! W izolowanej sesji testowej nie ma fizycznej myszy ani nadrzędnego kompozytora,
//! więc nie da się sprawdzić routingu kliknięć „na żywo”. Ten moduł czyta polecenia
//! z pliku (może być zwykły plik, do którego dopisujemy) i podaje je do
//! [`crate::seat::handle_input`] dokładnie tak, jak zrobiłby to evdev.
//!
//! Format — jedna komenda na linię:
//!
//! ```text
//! move X Y
//! down X Y
//! up X Y
//! click X Y
//! rclick X Y      # prawy przycisk (menu kontekstowe taskbara)
//! keydown KOD     # KOD = fizyczny kod evdev (np. 42 = Shift, 3 = "2")
//! keyup KOD
//! ```
//!
//! Włączenie: `SELENE_INPUT_SCRIPT=/ścieżka/do/pliku selene`. Bez zmiennej moduł
//! nic nie robi i nie wpływa na normalną sesję.

use std::path::PathBuf;

use libluna_core::{ButtonState, Event, KeyEvent, Modifiers, Point, PointerButton, PointerEvent};
use wayland_server::DisplayHandle;

use crate::state::State;

/// Czytnik skryptu wejścia (śledzi pozycję w pliku, więc dopisywanie działa).
pub struct ScriptInput {
    path: PathBuf,
    offset: u64,
    /// Ta sama mapa klawiatury co w evdev — pozwala odtworzyć keysym/tekst klawisza.
    #[allow(dead_code)]
    keymap: Option<xkbcommon::xkb::Keymap>,
    xkb: Option<xkbcommon::xkb::State>,
}

impl ScriptInput {
    /// `Some`, gdy włączono `SELENE_INPUT_SCRIPT`.
    pub fn open() -> Option<Self> {
        let path = std::env::var_os("SELENE_INPUT_SCRIPT")?;
        let (keymap, xkb) = match xkbcommon::xkb::Keymap::new_from_string(
            &xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS),
            crate::seat::keymap_text(),
            xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) {
            Some(km) => {
                let state = xkbcommon::xkb::State::new(&km);
                (Some(km), Some(state))
            }
            None => (None, None),
        };
        Some(Self {
            path: PathBuf::from(path),
            offset: 0,
            keymap,
            xkb,
        })
    }

    /// Przetwarza nowe linie dopisane do pliku.
    pub fn poll(&mut self, state: &mut State, handle: &DisplayHandle) {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return;
        };
        if text.len() as u64 == self.offset {
            // Brak nowych danych — to normalny stan przy każdym `poll`.
            return;
        }
        if (text.len() as u64) < self.offset {
            // Plik skrócony (nowy test) — zacznij od początku.
            self.offset = 0;
        }
        let start = self.offset as usize;
        if start > text.len() || !text.is_char_boundary(start) {
            self.offset = 0;
            return;
        }
        let chunk = text[start..].to_owned();
        self.offset = text.len() as u64;
        for line in chunk.lines() {
            self.apply(line, state, handle);
        }
    }

    fn apply(&mut self, line: &str, state: &mut State, handle: &DisplayHandle) {
        let mut parts = line.split_whitespace();
        let Some(command) = parts.next() else {
            return;
        };
        match command {
            "keydown" | "keyup" => {
                if let Some(code) = parts.next().and_then(|c| c.parse::<u16>().ok()) {
                    self.key(code, command == "keydown", state, handle);
                }
                return;
            }
            _ => {}
        }
        let point = parse_point(&mut parts);
        match (command, point) {
            ("move", Some(p)) => send_move(state, handle, p),
            ("down", Some(p)) => {
                send_button(state, handle, p, PointerButton::Left, ButtonState::Pressed)
            }
            ("up", Some(p)) => {
                send_button(state, handle, p, PointerButton::Left, ButtonState::Released)
            }
            ("rdown", Some(p)) => {
                send_button(state, handle, p, PointerButton::Right, ButtonState::Pressed)
            }
            ("rup", Some(p)) => send_button(
                state,
                handle,
                p,
                PointerButton::Right,
                ButtonState::Released,
            ),
            ("click", Some(p)) => {
                send_move(state, handle, p);
                send_button(state, handle, p, PointerButton::Left, ButtonState::Pressed);
                send_button(state, handle, p, PointerButton::Left, ButtonState::Released);
            }
            ("rclick", Some(p)) => {
                send_move(state, handle, p);
                send_button(state, handle, p, PointerButton::Right, ButtonState::Pressed);
                send_button(
                    state,
                    handle,
                    p,
                    PointerButton::Right,
                    ButtonState::Released,
                );
            }
            _ => {}
        }
    }

    /// Wysyła zdarzenie klawisza z keysymem/tekstem obliczonym przez xkb.
    fn key(&mut self, code: u16, pressed: bool, state: &mut State, handle: &DisplayHandle) {
        let Some(xkb) = self.xkb.as_mut() else {
            return;
        };
        let kc = xkbcommon::xkb::Keycode::new(u32::from(code) + 8);
        let dir = if pressed {
            xkbcommon::xkb::KeyDirection::Down
        } else {
            xkbcommon::xkb::KeyDirection::Up
        };
        xkb.update_key(kc, dir);
        let keysym = xkb.key_get_one_sym(kc).raw();
        let text = if pressed {
            let s = xkb.key_get_utf8(kc);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        } else {
            None
        };
        let event = Event::Key(KeyEvent {
            keysym,
            keycode: u32::from(code),
            text,
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
            modifiers: Modifiers::NONE,
            repeat: false,
        });
        crate::seat::handle_input(state, handle, &event);
    }
}

fn parse_point(parts: &mut std::str::SplitWhitespace<'_>) -> Option<Point> {
    let x: f64 = parts.next()?.parse().ok()?;
    let y: f64 = parts.next()?.parse().ok()?;
    Some(Point::new(x, y))
}

fn send_move(state: &mut State, handle: &DisplayHandle, position: Point) {
    let event = Event::PointerMotion {
        position,
        modifiers: Modifiers::NONE,
    };
    crate::seat::handle_input(state, handle, &event);
}

fn send_button(
    state: &mut State,
    handle: &DisplayHandle,
    position: Point,
    button: PointerButton,
    button_state: ButtonState,
) {
    let event = Event::PointerButton(PointerEvent {
        position,
        button,
        state: button_state,
        modifiers: Modifiers::NONE,
    });
    crate::seat::handle_input(state, handle, &event);
}
