//! Sterowanie kompozytorem z powłoki (lokalny gniazdo Unix).
//!
//! Protokoły Wayland nie mają żądania „przenieś to okno na przestrzeń”, więc powłoka
//! wysyła krótkie polecenie tekstowe do `$XDG_RUNTIME_DIR/luna-control`. Gniazdo jest
//! odpytywane w głównej pętli razem z wejściem — bez dodatkowych wątków i bez
//! blokowania na kliencie (timeout odczytu).

use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;

use wayland_server::Resource;

use crate::state::State;
use crate::sync::LockExt;

pub struct Control {
    listener: UnixListener,
}

impl Control {
    /// Tworzy gniazdo `luna-control` (usuwa pozostałość po poprzedniej sesji).
    pub fn spawn() -> Option<Self> {
        let rt = std::env::var_os("XDG_RUNTIME_DIR")?;
        let path = PathBuf::from(rt).join("luna-control");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).ok()?;
        listener.set_nonblocking(true).ok()?;
        Some(Self { listener })
    }

    pub fn fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Przyjmuje i wykonuje wszystkie oczekujące polecenia.
    pub fn accept(&self, state: &mut State) {
        while let Ok((mut stream, _)) = self.listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
            let mut buf = [0u8; 1024];
            if let Ok(n) = stream.read(&mut buf) {
                let text = String::from_utf8_lossy(&buf[..n]);
                for line in text.lines() {
                    handle(state, line.trim());
                }
            }
        }
    }
}

fn handle(state: &mut State, line: &str) {
    if line.is_empty() {
        return;
    }
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        // `move-to-workspace <indeks> <tytuł okna>`
        "move-to-workspace" => {
            let (ws, title) = rest.split_once(' ').unwrap_or((rest, ""));
            if let Ok(ws) = ws.parse::<usize>() {
                crate::seat::move_window_by_title(state, title, ws);
            }
        }
        // `workspace-count <n>` — zmiana liczby przestrzeni bez restartu sesji.
        "workspace-count" => {
            if let Ok(count) = rest.trim().parse::<usize>() {
                state.set_workspace_count(count);
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!("selene/control: workspace-count = {}", state.ws_count);
                }
            }
        }
        // `workspace-corner <0|1>` — przełączanie przestrzeni dolnymi rogami ekranu.
        "workspace-corner" => {
            state.set_workspace_corner_switch(matches!(rest.trim(), "1" | "true" | "on"));
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/control: workspace-corner = {}",
                    state.ws_corner_switch
                );
            }
        }
        // `idle-reload` — przeładuj `~/.config/luna/idle.conf` bez wylogowania.
        // Ustawienia zapisują plik i wysyłają to polecenie; Selene od razu
        // przelicza progi wygaszania/blokady/usypiania.
        "idle-reload" => {
            state.reload_idle();
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/control: idle screen_off={} lock={} suspend={}",
                    state.idle.screen_off_secs, state.idle.lock_secs, state.idle.suspend_secs
                );
            }
        }
        // `workspace-overview-corner <0|1>` — podgląd Super+Tab w lewym górnym rogu.
        "workspace-overview-corner" => {
            state.set_workspace_overview_corner(matches!(rest.trim(), "1" | "true" | "on"));
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/control: workspace-overview-corner = {}",
                    state.ws_overview_corner
                );
            }
        }
        // `input repeat <opóźnienie_ms> <klawisze/s>` oraz `input natural-scroll <0|1>`.
        "input" => {
            let (what, values) = rest.split_once(' ').unwrap_or((rest, ""));
            match what.trim() {
                "repeat" => {
                    let mut parts = values.split_whitespace();
                    if let (Some(delay), Some(rate)) = (parts.next(), parts.next()) {
                        if let (Ok(delay), Ok(rate)) = (delay.parse::<u32>(), rate.parse::<u32>()) {
                            state.repeat_delay = delay.clamp(100, 2000);
                            state.repeat_rate = rate.clamp(5, 60);
                            for keyboard in &state.keyboards {
                                if keyboard.version() >= 4 {
                                    let _ = keyboard.send_event(
                                        wayland_server::protocol::wl_keyboard::Event::RepeatInfo {
                                            rate: state.repeat_rate as i32,
                                            delay: state.repeat_delay as i32,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
                "natural-scroll" => {
                    state.natural_scroll = matches!(values.trim(), "1" | "true" | "on");
                }
                _ => {}
            }
        }
        // `workspace-name <indeks> <nazwa>` — zmiana nazwy bez restartu sesji.
        "workspace-name" => {
            let (index, name) = rest.split_once(' ').unwrap_or((rest, ""));
            if let Ok(index) = index.parse::<usize>() {
                state.set_workspace_name(index, name);
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!(
                        "selene/control: workspace {} → {:?}",
                        index + 1,
                        state.workspace_name(index)
                    );
                }
            }
        }
        // `windows` — diagnostyka: lista okien w modelu kompozycji.
        "windows" => {
            let ws = *state.ws_current.lock_or_recover();
            let wins = state.windows.lock_or_recover();
            eprintln!("selene/control: okna={} ws={ws}", wins.len());
            for (i, w) in wins.iter().enumerate() {
                eprintln!(
                    "selene/control:   [{i}] mapped={} min={} popup={} layer={} z={} \"{}\" {}x{} @ ({:.0},{:.0}) data={}x{}",
                    w.mapped,
                    w.minimized,
                    w.popup,
                    w.layer,
                    w.z,
                    w.title,
                    w.width,
                    w.height,
                    w.x,
                    w.y,
                    w.data.width,
                    w.data.height
                );
            }
        }
        // `dmabuf` — czy `zwp_linux_dmabuf_v1` jest ogłoszony (tylko z GPU).
        "dmabuf" => {
            let (formats, modifiers) = crate::dmabuf::counts();
            eprintln!(
                "selene/control: dmabuf enabled={} formats={formats} modifiers={modifiers}",
                crate::dmabuf::enabled()
            );
        }
        // `cursor` — bieżący obraz kursora (kształt z `wp_cursor_shape_v1`,
        // obraz klienta albo domyślna strzałka); `cursor shape <nazwa>` wymusza
        // kształt do zrzutów/testów.
        "cursor" => {
            let rest = rest.trim();
            if let Some(name) = rest.strip_prefix("shape ") {
                let name = name.trim();
                if !name.is_empty() {
                    *state.cursor_image.lock_or_recover() =
                        crate::state::CursorImage::Shape(name.to_owned());
                }
            } else if rest == "default" {
                *state.cursor_image.lock_or_recover() = crate::state::CursorImage::Default;
            }
            let image = state.cursor_image.lock_or_recover().clone();
            let (w, h) = match &image {
                crate::state::CursorImage::Custom { data, .. } => (data.width, data.height),
                _ => (0, 0),
            };
            eprintln!(
                "selene/control: cursor image={image:?} surface={} size={w}x{h} pointers={} focus={:?}",
                state.cursor_surface.is_some(),
                state.pointers.len(),
                state.pointer_focus.as_ref().map(|(slot, _)| *slot)
            );
        }
        // `outputs` — geometria monitorów (testy sesji liczą z niej współrzędne).
        "outputs" => {
            let outs = state.outputs.lock_or_recover();
            eprintln!("selene/control: outputs={}", outs.len());
            for (i, o) in outs.iter().enumerate() {
                eprintln!(
                    "selene/control:   output {i} {} {}x{} @ ({},{})",
                    o.name, o.width, o.height, o.x, o.y
                );
            }
        }
        // `x11` — diagnostyka XWayland/XWM: ekran, właściciel WM, liczba okien.
        // Używane przez `scripts/x11-smoke.sh` i przy diagnozie „aplikacja X11 się nie mapuje”.
        "x11" => match crate::xwm::global() {
            Some(xwm) => {
                let s = xwm.status();
                eprintln!(
                    "selene/control: x11 display=:{} wm={} managed={} windows={}",
                    s.display, s.is_wm, s.managed, s.windows
                );
                for (win, info) in xwm.managed_info() {
                    eprintln!(
                        "selene/control:   x11 0x{win:x} \"{}\" content={}x{} border={} csd={} over={}",
                        info.title,
                        info.width,
                        info.height,
                        info.border,
                        info.client_decorations,
                        info.override_redirect
                    );
                }
            }
            None => eprintln!("selene/control: x11 display=- wm=false managed=0 windows=0"),
        },
        // `screenshot` — zapis klatek wszystkich monitorów (Super+F12).
        "screenshot" => {
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/control: zrzut ekranu");
            }
            state
                .screenshot
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // `night-light <0..100>` — siła ciepłej korekty gamma.
        "night-light" => {
            if let Ok(percent) = rest.parse::<u8>() {
                state
                    .night_light
                    .store(percent.min(100), std::sync::atomic::Ordering::Relaxed);
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!("selene/control: night light {percent}%");
                }
            }
        }
        // `output-mode <złącze> <WxH@Hz|auto>` — zmiana trybu monitora na żywo.
        "output-mode" => {
            let (name, value) = rest.split_once(' ').unwrap_or((rest, "auto"));
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            let value = value.trim();
            let mode = if value.is_empty() || value.eq_ignore_ascii_case("auto") {
                None
            } else {
                match libluna_core::OutputMode::parse(value) {
                    Some(mode) => Some(mode),
                    None => {
                        if std::env::var("SELENE_STATS").is_ok() {
                            eprintln!("selene/control: nieznany tryb {value}");
                        }
                        return;
                    }
                }
            };
            // Zapis, żeby wybór przetrwał restart sesji.
            let mut settings = libluna_core::DisplaySettings::load();
            settings.set_mode(name, mode);
            let _ = settings.save();
            *state.display_request.lock_or_recover() = Some((name.to_owned(), mode));
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/control: output {name} -> {value}");
            }
        }
        // `output-position <złącze> <x> <y>` — przesunięcie monitora w układzie.
        "output-position" => {
            let mut parts = rest.split(' ');
            let name = parts.next().unwrap_or_default().trim();
            let x = parts
                .next()
                .and_then(|value| value.trim().parse::<i32>().ok());
            let y = parts
                .next()
                .and_then(|value| value.trim().parse::<i32>().ok());
            let (Some(x), Some(y)) = (x, y) else {
                return;
            };
            if name.is_empty() {
                return;
            }
            let mut settings = libluna_core::DisplaySettings::load();
            settings.set_position(name, Some((x, y)));
            let _ = settings.save();
            *state.position_request.lock_or_recover() = Some((name.to_owned(), x, y));
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/control: output {name} at {x},{y}");
            }
        }
        // `power-saver <0|1>` — ograniczenie tempa prezentacji.
        "power-saver" => {
            let on = matches!(rest.trim(), "1" | "on" | "true");
            state
                .power_saver
                .store(on, std::sync::atomic::Ordering::Relaxed);
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/control: power saver {}",
                    if on { "on" } else { "off" }
                );
            }
        }
        _ => {
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/control: nieznane polecenie: {line}");
            }
        }
    }
}
