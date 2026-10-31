//! Wyjście: nested (okno na nadrzędnym kompozytorze) oraz **realne DRM/KMS** (scanout).
//!
//! Wątek nested pełni dwie role: rysuje scenę przez `libluna-render` i odbiera wejście
//! od nadrzędnego kompozytora, przekazując je do magistrali (`crate::input`).
//! Wątek DRM renderuje scenę wprost do buforów „dumb” i skanuje je na monitorach
//! (page-flip). Wejście w trybie DRM (evdev) jest kolejnym krokiem.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use libluna_core::{Color, Point, Rect, Size, Theme, ThemeSettings};
use libluna_platform::{run, AppConfig, Frame};
use libluna_render::gpu::GpuRenderer;
use libluna_render::{ImageData, Renderer, Scene, SoftwareRenderer, TextEngine, TextRun};
use libluna_widgets::{ControlsSide, HeaderBar, PaintCtx};

use crate::drm::{DrmBackend, DrmError};
use crate::input::{self, Events};
use crate::monitors::{layout_horizontal, OutputInfo, Outputs};
use crate::state::{
    CornerHint, CornerKind, CursorModel, Overview, OverviewModel, ScreenshotModel, Window,
    WindowModel, WorkspaceCountModel, WorkspaceModel,
};
use crate::sync::LockExt;

/// Uruchamia wątek wyjścia (nested Wayland) o zadanym rozmiarze.
pub fn spawn(
    model: WindowModel,
    outputs: Outputs,
    events: Events,
    wake: UnixStream,
    ws: WorkspaceModel,
    overview: OverviewModel,
    screenshot: ScreenshotModel,
    ws_count: WorkspaceCountModel,
    width: u32,
    height: u32,
) {
    std::thread::spawn(move || {
        // Tryb nested nie ma sprzętowej tablicy gamma — chowamy kafelek night lightu.
        write_gamma_flag(false);
        let mut wake = wake;
        let config = AppConfig::new("Selene")
            .app_id("eu.paganlinux.Luna.Selene")
            .initial_size(width, height)
            .settings(ThemeSettings::load())
            // Esc należy do sesji gościa — nie może zamykać okna testowego Selene.
            .escape_to_exit(false);
        let theme = user_theme();
        let text = TextEngine::system();
        // Zrzut nested: trzymamy jeden renderer przez całą sesję — tworzenie
        // kontekstu EGL przy każdym zrzucie to kilkadziesiąt ms i zbędny churn.
        let mut shot: Option<Box<dyn Renderer>> = None;
        let result = run(config, move |frame: &mut Frame| {
            // Zgłoś bieżący rozmiar monitora (okno może być zmieniane przez rodzica).
            {
                let mut outs = outputs.lock_or_recover();
                if let Some(out) = outs.first_mut() {
                    out.width = frame.size.width as i32;
                    out.height = frame.size.height as i32;
                }
            }
            // Przekaż wejście do magistrali.
            input::push(&events, &mut wake, frame.events);
            // Narysuj scenę.
            let size = frame.size;
            let cur_ws = *ws.lock_or_recover();
            let ov = *overview.lock_or_recover();
            let ws_count = ws_count.load(std::sync::atomic::Ordering::Relaxed);
            build_scene(
                frame.scene,
                size,
                0.0,
                0.0,
                &model,
                cur_ws,
                ws_count,
                ov,
                true,
                None,
                true,
                &theme,
                &text,
            );
            // Zrzut ekranu także w trybie nested: kompozytor nie ma tu skanouta,
            // więc kodujemy bieżącą scenę bezpośrednio do PNG.
            if screenshot.swap(false, std::sync::atomic::Ordering::Relaxed) {
                let (w, h) = (size.width as u32, size.height as u32);
                // Zrzut przez GPU, gdy dostępne: tylko wtedy widać okna `dmabuf`
                // (CPU renderer rysuje dla nich zastępczy prostokąt).
                let renderer = shot.get_or_insert_with(|| match GpuRenderer::new(w, h) {
                    Ok(r) => Box::new(r) as Box<dyn Renderer>,
                    Err(_) => Box::new(SoftwareRenderer::new(w, h)),
                });
                renderer.resize(w, h);
                renderer.draw(frame.scene, frame.scale, Color::TRANSPARENT);
                if let Some(png) = renderer.encode_png() {
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let path = screenshot_dir().join(format!("luna-{stamp}-nested.png"));
                    match std::fs::write(&path, png) {
                        Ok(()) => eprintln!("selene: zrzut ekranu: {}", path.display()),
                        Err(e) => eprintln!("selene: nie udało się zapisać zrzutu: {e}"),
                    }
                }
            }
            // Utrzymuj klatki — model okien zmienia się poza tą pętlą.
            frame.request_frame();
        });
        // Nested rysuje do okna na nadrzędnym kompozytorze Wayland. Gdy go nie ma
        // (np. uruchomienie na X11), nie połykamy błędu — inaczej wygląda to jak
        // „kompozytor działa, ale nic nie widać”.
        if let Err(e) = result {
            eprintln!(
                "{}: {e}",
                libluna_i18n::tr("nested output unavailable (no parent compositor)")
            );
        }
    });
}

/// Otwiera backend DRM, czekając chwilę, gdy mastera trzyma jeszcze greeter LightDM.
///
/// LightDM potrafi wystartować sesję Wayland, **zanim ubije greeter** (Xorg), który
/// trzyma master DRM. Bez ponowień Selene od razu widzi `MasterBusy` i zostawia czarny
/// ekran.
///
/// Greeter oddaje master zwykle w ułamku sekundy, dlatego pierwsze próby są **gęste**
/// (20 ms) — wcześniej każde opóźnienie było płacone w krokach po 100 ms, co dawało
/// odczucie „panel wyskakuje po ~5 sekundach”. Po pół sekundy przechodzimy na 100 ms,
/// żeby nie palić procesora bez potrzeby.
fn open_drm_waiting(preferences: &libluna_core::DisplaySettings) -> Result<DrmBackend, DrmError> {
    let stats = std::env::var("SELENE_STATS").is_ok();
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(15);
    let mut attempt = 0u32;
    loop {
        let err = match DrmBackend::open(preferences) {
            Ok(backend) => {
                if stats && attempt > 0 {
                    eprintln!(
                        "selene/drm: przejęto ekran po {:.0} ms",
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                }
                return Ok(backend);
            }
            Err(e) => e,
        };
        if std::time::Instant::now() >= deadline {
            return Err(err);
        }
        if attempt == 0 && stats {
            eprintln!("selene/drm: init nieudany ({err:?}) — czekam na zwolnienie ekranu…");
        }
        let wait_ms = if attempt < 25 { 20 } else { 100 };
        attempt += 1;
        std::thread::sleep(Duration::from_millis(wait_ms));
    }
}

/// Informuje powłokę (plikiem w `$XDG_RUNTIME_DIR`), czy night light jest dostępny.
fn write_gamma_flag(supported: bool) {
    let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let path = std::path::PathBuf::from(rt).join("luna-gamma");
    let _ = std::fs::write(path, if supported { "1" } else { "0" });
}

/// Opóźnienie do limitu ~30 FPS w trybie oszczędzania (`None` = bez limitu).
fn throttle_delay(power_saver: bool, elapsed: Duration) -> Option<Duration> {
    if !power_saver {
        return None;
    }
    const MIN: Duration = Duration::from_millis(33);
    (elapsed < MIN).then(|| MIN - elapsed)
}

/// Zapisuje migawkę monitorów dla Ustawień: `nazwa|szer|wys|hz|x|y|skala|tryby`.
///
/// Dzięki temu strona „Ekrany” zna tryby z modesetu (z odświeżaniem), a nie tylko
/// pierwszą linię z `/sys/class/drm/*/modes`. Plik nazywa się `luna-monitors`,
/// bo `luna-outputs` jest markerem sesji (liczba monitorów → ile paneli uruchomić).
fn write_outputs_snapshot(
    outputs: &[OutputInfo],
    detailed: &[(String, u32, u32, u32, Vec<libluna_core::OutputMode>)],
) {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let mut text = String::from("# luna-monitors: nazwa|szer|wys|hz|x|y|skala|tryby\n");
    for (info, (_, _, _, _, modes)) in outputs.iter().zip(detailed) {
        let modes = modes
            .iter()
            .map(|mode| mode.config_value())
            .collect::<Vec<_>>()
            .join(",");
        text.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|{}|{}\n",
            info.name, info.width, info.height, info.refresh_hz, info.x, info.y, info.scale, modes
        ));
    }
    let path = std::path::PathBuf::from(runtime).join("luna-monitors");
    let _ = std::fs::write(path, text);
}

/// Uruchamia wątek wyjścia DRM/KMS — renderuje scenę do buforów i skanuje na monitorach.
///
/// Wymaga bycia DRM masterem; jeśli inną sesję trzyma master, kończy bez efektu
/// (wywołujący powinien wtedy użyć wyjścia nested).
pub fn spawn_drm(
    model: WindowModel,
    cursor: CursorModel,
    cursor_image: crate::state::CursorImageModel,
    ws: WorkspaceModel,
    overview: OverviewModel,
    screenshot: crate::state::ScreenshotModel,
    night_light: crate::state::NightLightModel,
    power_saver: crate::state::PowerSaverModel,
    outputs: Outputs,
    ws_count: WorkspaceCountModel,
    display_request: crate::state::DisplayRequestModel,
    position_request: crate::state::PositionRequestModel,
) {
    std::thread::spawn(move || {
        let display = libluna_core::DisplaySettings::load();
        let mut backend = match open_drm_waiting(&display) {
            Ok(backend) => backend,
            Err(reason) => {
                // Rozróżniamy powód — inaczej „brak mastera” maskuje błędy inicjalizacji.
                let msg = match reason {
                    DrmError::MasterBusy => {
                        libluna_i18n::tr("DRM backend unavailable (master held by another session)")
                    }
                    DrmError::Resources => {
                        libluna_i18n::tr("DRM backend unavailable (cannot read card resources)")
                    }
                    DrmError::NoOutputs => {
                        libluna_i18n::tr("DRM backend unavailable (no monitor could be set up)")
                    }
                };
                eprintln!("{msg}");
                return;
            }
        };
        let mut infos = backend.outputs();
        // Night light: sprawdzamy **realne** wsparcie CRTC dla tablicy gamma i od razu
        // wgrywamy zapisaną rampę (to zarazem test ioctla). Flaga w katalogu runtime
        // mówi powłoce, czy kafelek „Tryb nocny” jest w ogóle dostępny.
        let initial_night = display.night_light;
        night_light.store(initial_night, std::sync::atomic::Ordering::Relaxed);
        let gamma_ok = backend.supports_night_light() && backend.set_night_light(initial_night);
        write_gamma_flag(gamma_ok);
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!(
                "selene/drm: night light {}",
                if gamma_ok {
                    "dostępny (gamma LUT)"
                } else {
                    "niedostępny"
                }
            );
        }
        // **Faktyczne** tryby z modesetu trafiają do wspólnego modelu monitorów.
        // Wcześniej model zostawał przy wyniku wczesnego `discover()`; gdy kernel
        // zgłosił inny tryb (albo inne złącze), panel i okna miały złą geometrię,
        // a drugi monitor potrafił dostać nie-natywną rozdzielczość.
        let mut origins: Vec<(f64, f64)>;
        {
            let detailed = backend.outputs_with_modes();
            let mut list: Vec<OutputInfo> = infos
                .iter()
                .map(|(name, w, h, hz)| OutputInfo {
                    name: name.clone(),
                    width: *w as i32,
                    height: *h as i32,
                    refresh_hz: *hz as i32,
                    scale: 1,
                    x: 0,
                    y: 0,
                })
                .collect();
            // Układ zapisany przez użytkownika (`output <złącze> position = x,y`).
            // Brakujące pozycje dokładamy po prawej stronie zapisanych.
            let mut positioned = false;
            let mut right = 0;
            for info in &mut list {
                if let Some((x, y)) = display.position_for(&info.name) {
                    info.x = x;
                    info.y = y;
                    positioned = true;
                    right = right.max(x + info.width);
                }
            }
            if positioned {
                for info in &mut list {
                    if display.position_for(&info.name).is_none() {
                        info.x = right;
                        info.y = 0;
                        right += info.width;
                    }
                }
            } else {
                layout_horizontal(&mut list);
            }
            write_outputs_snapshot(&list, &detailed);
            origins = list
                .iter()
                .map(|info| (info.x as f64, info.y as f64))
                .collect();
            *outputs.lock_or_recover() = list;
        }
        let mut sizes: Vec<(u32, u32)> = infos.iter().map(|(_, w, h, _)| (*w, *h)).collect();
        for (name, w, h, hz) in &infos {
            println!(
                "{}: {} {}x{}@{}Hz",
                libluna_i18n::tr("DRM output"),
                name,
                w,
                h,
                hz
            );
        }
        let mut renderers: Vec<SoftwareRenderer> = sizes
            .iter()
            .map(|(w, h)| SoftwareRenderer::new(*w, *h))
            .collect();
        let mut gpu_renderers: Vec<GpuRenderer> = Vec::new();
        // Backend wyjścia: GPU, gdy poproszono (`SELENE_GPU`/`LUNA_GPU`) i EGL działa.
        // GPU komponuje okna `dmabuf` sprzętowo (CPU rysuje dla nich tylko zaślepkę),
        // a każdy monitor dostaje własny kontekst — render jest sekwencyjny, bo
        // kontekstu EGL nie można przenosić między wątkami.
        let mut use_gpu = if gpu_requested() {
            let mut gpu = Vec::with_capacity(sizes.len());
            let mut error = None;
            for (w, h) in &sizes {
                match GpuRenderer::new(*w, *h) {
                    Ok(r) => gpu.push(r),
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            if gpu.len() == sizes.len() {
                renderers.clear();
                gpu_renderers = gpu;
                true
            } else {
                eprintln!(
                    "selene/drm: GPU niedostępne ({}), zostaję przy CPU",
                    error.as_deref().unwrap_or("brak kontekstu EGL")
                );
                false
            }
        } else {
            false
        };
        if use_gpu && std::env::var("SELENE_STATS").is_ok() {
            eprintln!("selene/drm: backend gpu");
        }
        let bg = Color::from_rgba8(0x1e, 0x20, 0x28, 0xff);
        // Motyw i silnik tekstu dla dekoracji (widget `HeaderBar` z libLuny).
        // Bierzemy **motyw użytkownika** (jasny/ciemny, akcent, styl z pliku), a nie
        // twarde `Theme::dark()` — inaczej pasek okna X11 wyglądał inaczej niż okno
        // aplikacji libLuny. Co 2 s sprawdzamy plik ustawień, żeby zmiana motywu
        // w demo odświeżyła też dekoracje bez wylogowania.
        let mut theme = user_theme();
        let mut theme_checked = std::time::Instant::now();
        let text_engine = TextEngine::system();

        // Kursor: obraz bierzemy z modelu (kształt z `wp_cursor_shape_v1` albo bufor
        // klienta). Płaszczyzna sprzętowa dostaje własny BO; za duży obraz → scena.
        let mut cursor_source = crate::state::CursorImage::Default;
        crate::cursor::set_model(&cursor_source);
        let (mut cursor_gfx, mut cursor_hotspot) = crate::cursor::current();
        let mut hw_cursor = crate::cursor::fits_hardware(&cursor_gfx);
        for i in 0..sizes.len() {
            // Bezpieczna próba na (0, 0) i natychmiastowe ukrycie — uodparnia na
            // sterowniki DRM odrzucające ujemne współrzędne w `MODE_CURSOR`.
            if !hw_cursor
                || !backend.set_cursor_image(
                    i,
                    &cursor_gfx.rgba,
                    cursor_gfx.width,
                    cursor_gfx.height,
                    0,
                    0,
                )
                || !backend.hide_cursor(i)
            {
                hw_cursor = false;
            }
        }
        if !hw_cursor {
            eprintln!(
                "{}",
                libluna_i18n::tr("DRM hardware cursor unavailable; using software cursor")
            );
        }
        let mut cursor_shown = vec![false; sizes.len()];

        // Diagnostyka (SELENE_STATS=1): fps i rozbicie czasu na render/present/wait_flip.
        let stats = std::env::var("SELENE_STATS").is_ok();
        let mut flip_error_logged = false;
        let mut frames = 0u64;
        let (mut ms_render, mut ms_present, mut ms_wait) = (0.0f64, 0.0f64, 0.0f64);
        let mut report = std::time::Instant::now();
        let mut last_sigs: Vec<Option<u64>> = vec![None; sizes.len()];
        let mut last_cur: Option<Point> = None;
        let mut applied_night: Option<u8> = None;
        let mut applied_power = false;
        let mut last_present = std::time::Instant::now();
        loop {
            // Motyw użytkownika mógł się zmienić (demo/Ustawienia) — odśwież, gdy inny.
            if theme_checked.elapsed() >= Duration::from_secs(2) {
                theme_checked = std::time::Instant::now();
                let next = user_theme();
                if next != theme {
                    theme = next;
                    // Wymuś pełny render (sygnatury nie zawierają kolorów motywu).
                    for sig in last_sigs.iter_mut() {
                        *sig = None;
                    }
                }
            }
            // Zmiana trybu monitora z Ustawień (`luna-control output-mode`):
            // realny modeset DRM + odbudowa buforów i rendererów tego monitora.
            if let Some((name, mode)) = display_request.lock_or_recover().take() {
                match backend.set_mode(&name, mode) {
                    Some(_index) => {
                        let previous = outputs.lock_or_recover().clone();
                        infos = backend.outputs();
                        sizes = infos.iter().map(|(_, w, h, _)| (*w, *h)).collect();
                        if use_gpu {
                            let mut gpu = Vec::with_capacity(sizes.len());
                            let mut failure = None;
                            for (w, h) in &sizes {
                                match GpuRenderer::new(*w, *h) {
                                    Ok(renderer) => gpu.push(renderer),
                                    Err(err) => {
                                        failure = Some(err);
                                        break;
                                    }
                                }
                            }
                            if failure.is_none() && gpu.len() == sizes.len() {
                                gpu_renderers = gpu;
                            } else {
                                eprintln!(
                                    "selene/drm: GPU po zmianie trybu niedostępne ({}) — przechodzę na CPU",
                                    failure.as_deref().unwrap_or("nieznany błąd")
                                );
                                use_gpu = false;
                                gpu_renderers.clear();
                                renderers = sizes
                                    .iter()
                                    .map(|(w, h)| SoftwareRenderer::new(*w, *h))
                                    .collect();
                            }
                        } else {
                            renderers = sizes
                                .iter()
                                .map(|(w, h)| SoftwareRenderer::new(*w, *h))
                                .collect();
                        }
                        // Płaszczyzna kursora mogła zostać zresetowana przez modeset.
                        hw_cursor = crate::cursor::fits_hardware(&cursor_gfx);
                        for i in 0..sizes.len() {
                            if !hw_cursor
                                || !backend.set_cursor_image(
                                    i,
                                    &cursor_gfx.rgba,
                                    cursor_gfx.width,
                                    cursor_gfx.height,
                                    0,
                                    0,
                                )
                                || !backend.hide_cursor(i)
                            {
                                hw_cursor = false;
                            }
                        }
                        cursor_shown = vec![false; sizes.len()];
                        last_sigs = vec![None; sizes.len()];
                        last_present = std::time::Instant::now();
                        // Modeset zeruje LUT gamma — night light trzeba wgrać ponownie.
                        applied_night = None;

                        // Panel i Ustawienia dostają nową geometrię od razu.
                        // Pozycje z układu użytkownika zostają — zmienił się tylko rozmiar.
                        let detailed = backend.outputs_with_modes();
                        let mut list: Vec<OutputInfo> = infos
                            .iter()
                            .map(|(name, w, h, hz)| OutputInfo {
                                name: name.clone(),
                                width: *w as i32,
                                height: *h as i32,
                                refresh_hz: *hz as i32,
                                scale: 1,
                                x: 0,
                                y: 0,
                            })
                            .collect();
                        if previous.len() == list.len() {
                            for (info, old) in list.iter_mut().zip(previous.iter()) {
                                info.x = old.x;
                                info.y = old.y;
                            }
                        } else {
                            layout_horizontal(&mut list);
                        }
                        origins = list
                            .iter()
                            .map(|info| (info.x as f64, info.y as f64))
                            .collect();
                        write_outputs_snapshot(&list, &detailed);
                        *outputs.lock_or_recover() = list;
                        if stats {
                            eprintln!("selene/drm: tryb {name} zmieniony");
                        }
                    }
                    None => eprintln!("selene/drm: nie udało się ustawić trybu dla {name}"),
                }
            }
            // Przesunięcie monitora w układzie (`luna-control output-position`).
            // Bez modesetu: zmienia się tylko model wyjść i geometria dla klientów.
            if let Some((name, x, y)) = position_request.lock_or_recover().take() {
                let mut list = outputs.lock_or_recover().clone();
                if let Some(slot) = list.iter_mut().find(|output| output.name == name) {
                    slot.x = x;
                    slot.y = y;
                    origins = list
                        .iter()
                        .map(|info| (info.x as f64, info.y as f64))
                        .collect();
                    let detailed = backend.outputs_with_modes();
                    write_outputs_snapshot(&list, &detailed);
                    *outputs.lock_or_recover() = list;
                    // Inne kadrowanie monitorów — wymuś pełny render.
                    for sig in last_sigs.iter_mut() {
                        *sig = None;
                    }
                    last_cur = None;
                    if stats {
                        eprintln!("selene/drm: {name} -> {x},{y}");
                    }
                }
            }
            // Night light i oszczędzanie energii — zmiany z gniazda sterowania.
            if gamma_ok {
                let requested = night_light.load(std::sync::atomic::Ordering::Relaxed);
                if applied_night != Some(requested) {
                    applied_night = Some(requested);
                    backend.set_night_light(requested);
                    if stats {
                        eprintln!("selene/drm: night light {requested}%");
                    }
                }
            }
            let power = power_saver.load(std::sync::atomic::Ordering::Relaxed);
            if power != applied_power {
                applied_power = power;
                if stats {
                    eprintln!(
                        "selene/drm: power saver {}",
                        if power { "on" } else { "off" }
                    );
                }
            }
            // Zrzut ekranu (Super+F12): kompozytor trzyma gotowe klatki każdego monitora,
            // więc wystarczy je zapisać.
            if screenshot.swap(false, std::sync::atomic::Ordering::Relaxed) {
                // Zrzut z aktywnego backendu — z GPU widać też okna `dmabuf`.
                if use_gpu {
                    let refs: Vec<&dyn Renderer> =
                        gpu_renderers.iter().map(|r| r as &dyn Renderer).collect();
                    save_screenshots(&refs, &infos);
                } else {
                    let refs: Vec<&dyn Renderer> =
                        renderers.iter().map(|r| r as &dyn Renderer).collect();
                    save_screenshots(&refs, &infos);
                }
            }
            let ov = *overview.lock_or_recover();
            // Wygaszony ekran = brak kursora (także sprzętowego) do pierwszej aktywności.
            let cur = if ov.blanked {
                None
            } else {
                *cursor.lock_or_recover()
            };
            let cur_ws = *ws.lock_or_recover();
            let ws_count = ws_count.load(std::sync::atomic::Ordering::Relaxed);

            // Zmiana obrazu kursora (kształt z `wp_cursor_shape_v1` albo bufor klienta).
            // Nie zależy od ruchu myszy, więc wykrywamy ją osobno.
            let image_changed = {
                let next = cursor_image.lock_or_recover().clone();
                if next == cursor_source {
                    false
                } else {
                    cursor_source = next;
                    crate::cursor::set_model(&cursor_source);
                    let (gfx, hotspot) = crate::cursor::current();
                    cursor_gfx = gfx;
                    cursor_hotspot = hotspot;
                    true
                }
            };
            if image_changed {
                if hw_cursor && !crate::cursor::fits_hardware(&cursor_gfx) {
                    // Płaszczyzna bywa mniejsza niż kursor klienta — rysujemy w scenie.
                    hw_cursor = false;
                    for i in 0..sizes.len() {
                        backend.hide_cursor(i);
                        cursor_shown[i] = false;
                    }
                    eprintln!(
                        "selene/drm: kursor {}×{} za duży dla płaszczyzny — rysuję w scenie",
                        cursor_gfx.width, cursor_gfx.height
                    );
                }
                if hw_cursor {
                    apply_hw_cursor(
                        &mut backend,
                        &sizes,
                        &origins,
                        cur,
                        &cursor_gfx,
                        cursor_hotspot,
                        &mut cursor_shown,
                        true,
                    );
                }
                // Software: kursor jest częścią sceny — wymuś pełny render.
                for sig in last_sigs.iter_mut() {
                    *sig = None;
                }
            }

            let cur_changed = last_cur != cur;
            let mut any_changed = false;
            let mut changed = vec![false; sizes.len()];

            for i in 0..sizes.len() {
                let (w, h) = sizes[i];
                let (ox, oy) = origins[i];
                let sig = output_signature(
                    &model,
                    cur_ws,
                    ws_count,
                    ov,
                    if hw_cursor { None } else { cur },
                    ox,
                    oy,
                    w as f64,
                    h as f64,
                );
                if last_sigs[i] != Some(sig) {
                    last_sigs[i] = Some(sig);
                    changed[i] = true;
                    any_changed = true;
                }
            }

            if !any_changed && !cur_changed {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            last_cur = cur;

            // Kursor sprzętowy: ustaw/przesuń płaszczyznę kursora (bez dotykania sceny).
            if hw_cursor && (cur_changed || image_changed) {
                apply_hw_cursor(
                    &mut backend,
                    &sizes,
                    &origins,
                    cur,
                    &cursor_gfx,
                    cursor_hotspot,
                    &mut cursor_shown,
                    image_changed,
                );
            }

            if any_changed {
                // Tryb oszczędzania: maksymalnie ~30 klatek/s (oszczędza CPU/GPU
                // podczas animacji i przewijania; kursor sprzętowy pozostaje płynny).
                if let Some(delay) = throttle_delay(power, last_present.elapsed()) {
                    std::thread::sleep(delay);
                }
                last_present = std::time::Instant::now();
                // Renderowanie monitorów **równolegle**: każdy ma własny bufor i scenę,
                // więc na maszynie wielordzeniowej dwie klatki 1080p powstają w czasie
                // jednej. Sekwencyjny render dwóch monitorów zjadał cały budżet klatki
                // (7–10 ms każdy) i powłoka „zwalniała” do ~20 FPS.
                let t_render = std::time::Instant::now();
                if use_gpu {
                    // Sekwencyjnie: kontekst EGL jest przypięty do wątku tworzącego,
                    // a i tak jest znacznie szybszy od równoległego CPU.
                    for i in 0..sizes.len() {
                        if !changed[i] {
                            continue;
                        }
                        let (w, h) = sizes[i];
                        let (ox, oy) = origins[i];
                        let mut scene = Scene::new();
                        build_scene(
                            &mut scene,
                            Size::new(w as f64, h as f64),
                            ox,
                            oy,
                            &model,
                            cur_ws,
                            ws_count,
                            ov,
                            i == 0,
                            cur,
                            !hw_cursor,
                            &theme,
                            &text_engine,
                        );
                        gpu_renderers[i].draw(&scene, 1.0, bg);
                    }
                } else {
                    let theme = &theme;
                    let text_engine = &text_engine;
                    std::thread::scope(|scope| {
                        for (i, renderer) in renderers.iter_mut().enumerate() {
                            if !changed[i] {
                                continue;
                            }
                            let (w, h) = sizes[i];
                            let (ox, oy) = origins[i];
                            let model = &model;
                            scope.spawn(move || {
                                let mut scene = Scene::new();
                                build_scene(
                                    &mut scene,
                                    Size::new(w as f64, h as f64),
                                    ox,
                                    oy,
                                    model,
                                    cur_ws,
                                    ws_count,
                                    ov,
                                    i == 0,
                                    cur,
                                    !hw_cursor,
                                    &theme,
                                    &text_engine,
                                );
                                renderer.draw(&scene, 1.0, bg);
                            });
                        }
                    });
                }
                ms_render += t_render.elapsed().as_secs_f64() * 1000.0;

                let mut num_flips = 0;
                for i in 0..sizes.len() {
                    if !changed[i] {
                        continue;
                    }
                    let t = std::time::Instant::now();
                    let pixels = if use_gpu {
                        gpu_renderers[i].data()
                    } else {
                        renderers[i].data()
                    };
                    let flipped = backend.present(i, pixels);
                    if !flipped && !flip_error_logged {
                        eprintln!(
                            "{}",
                            libluna_i18n::tr("DRM page-flip failed (screen may stay blank)")
                        );
                        flip_error_logged = true;
                    }
                    ms_present += t.elapsed().as_secs_f64() * 1000.0;

                    if flipped {
                        num_flips += 1;
                    }
                }

                // Czekamy równolegle na flipy zakolejkowane na monitorach
                for _ in 0..num_flips {
                    let t = std::time::Instant::now();
                    backend.wait_flip();
                    ms_wait += t.elapsed().as_secs_f64() * 1000.0;
                }

                frames += 1;
                if stats && report.elapsed().as_secs_f64() >= 2.0 {
                    let secs = report.elapsed().as_secs_f64();
                    let frames_f = frames.max(1) as f64;
                    let per_monitor = frames_f * sizes.len() as f64;
                    eprintln!(
                        "selene/drm: {:.1} klatek/s | render {:.2} ms/klatke, present {:.2} ms, wait_flip {:.2} ms",
                        frames as f64 / secs,
                        ms_render / frames_f,
                        ms_present / per_monitor.max(1.0),
                        ms_wait / per_monitor.max(1.0)
                    );
                    frames = 0;
                    ms_render = 0.0;
                    ms_present = 0.0;
                    ms_wait = 0.0;
                    report = std::time::Instant::now();
                }
            }
        }
    });
}

/// Zapisuje bieżące klatki (po jednej na monitor) jako PNG — obsługa `Super+F12`.
pub fn save_screenshots(renderers: &[&dyn Renderer], infos: &[(String, u32, u32, u32)]) {
    let dir = screenshot_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for (i, renderer) in renderers.iter().enumerate() {
        let name = infos
            .get(i)
            .map(|(n, _, _, _)| n.as_str())
            .unwrap_or("ekran");
        let path = dir.join(format!("luna-{stamp}-{name}.png"));
        match renderer.encode_png() {
            Some(png) => match std::fs::write(&path, png) {
                Ok(()) => eprintln!("selene: zrzut ekranu: {}", path.display()),
                Err(e) => eprintln!("selene: nie udało się zapisać zrzutu: {e}"),
            },
            None => eprintln!("selene: nie udało się zakodować zrzutu"),
        }
    }
}

/// Czy wyjście ma używać GPU: `SELENE_GPU` lub `LUNA_GPU` ustawione na niepustą
/// wartość inną niż `0`. Brak EGL wciąż spada do CPU (nigdy nie zabija sesji).
fn gpu_requested() -> bool {
    ["SELENE_GPU", "LUNA_GPU"].iter().any(|name| {
        std::env::var(name)
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Katalog na zrzuty: `XDG_PICTURES_DIR` → `~/.config/user-dirs.dirs` → `$HOME/Pictures`
/// (oraz polskie `Obrazy`) → `/tmp`.
fn screenshot_dir() -> std::path::PathBuf {
    let mut pictures = std::env::var("XDG_PICTURES_DIR")
        .ok()
        .filter(|d| !d.is_empty());
    // Wiele sesji nie eksportuje `XDG_PICTURES_DIR`; użytkownik ma je w `user-dirs.dirs`.
    if pictures.is_none() {
        if let Some(home) = std::env::var_os("HOME") {
            let cfg = std::path::PathBuf::from(&home)
                .join(".config")
                .join("user-dirs.dirs");
            if let Ok(text) = std::fs::read_to_string(cfg) {
                for line in text.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("XDG_PICTURES_DIR=") {
                        let value = rest
                            .trim()
                            .trim_matches('"')
                            .replace("$HOME", &home.to_string_lossy());
                        if !value.is_empty() {
                            pictures = Some(value);
                        }
                        break;
                    }
                }
            }
        }
    }
    let mut candidates: Vec<String> = Vec::new();
    if let Some(p) = pictures {
        candidates.push(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        candidates.push(home.join("Pictures").to_string_lossy().into_owned());
        candidates.push(home.join("Obrazy").to_string_lossy().into_owned());
    }
    for dir in candidates {
        let p = std::path::PathBuf::from(dir).join("Screenshots");
        if std::fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    std::path::PathBuf::from("/tmp")
}

/// Ustawia obraz i pozycję kursora sprzętowego na wyjściach (DRM).
///
/// `force_image` wymusza nowy BO (po zmianie kształtu/obrazu), a nie tylko ruch —
/// `MODE_CURSOR` z flagą `MOVE` nie zmienia zawartości pamięci kursora.
#[allow(clippy::too_many_arguments)]
fn apply_hw_cursor(
    backend: &mut DrmBackend,
    sizes: &[(u32, u32)],
    origins: &[(f64, f64)],
    cur: Option<Point>,
    gfx: &libluna_render::ImageData,
    hotspot: (i32, i32),
    shown: &mut [bool],
    force_image: bool,
) {
    for i in 0..sizes.len() {
        let (ow, oh) = sizes[i];
        let (ox, oy) = origins[i];
        let inside = cur
            .map(|c| {
                let (lx, ly) = (c.x - ox, c.y - oy);
                lx >= 0.0 && ly >= 0.0 && lx < ow as f64 && ly < oh as f64
            })
            .unwrap_or(false);
        if inside {
            let c = cur.unwrap_or(Point::new(0.0, 0.0));
            let x = (c.x - ox) as i32 - hotspot.0;
            let y = (c.y - oy) as i32 - hotspot.1;
            if force_image || !shown[i] {
                backend.set_cursor_image(i, &gfx.rgba, gfx.width, gfx.height, x, y);
                shown[i] = true;
            } else {
                backend.move_cursor(i, x, y);
            }
        } else if shown[i] {
            backend.hide_cursor(i);
            shown[i] = false;
        }
    }
}

/// Sygnatura sceny dla konkretnego monitora — pozwala pominąć render monitora, na którym nic się nie zmieniło.
fn output_signature(
    model: &WindowModel,
    current_ws: usize,
    ws_count: usize,
    overview: Overview,
    cursor: Option<Point>,
    ox: f64,
    oy: f64,
    ow: f64,
    oh: f64,
) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(current_ws as u64);
    // Liczba przestrzeni zmienia pasek/podgląd, nawet gdy bieżąca przestrzeń się nie zmienia.
    mix(ws_count as u64);
    // Tapeta z Ustawień działa bez restartu sesji.
    mix(wallpaper_stamp());
    mix(u64::from(overview.on));
    mix(u64::from(overview.blanked));
    if let Some((slot, (px, py))) = overview.drag {
        mix(slot as u64);
        mix(px.to_bits());
        mix(py.to_bits());
    }
    // Poświata rogu: kwantyzacja postępu (256 kroków) wystarcza, a nie tworzy
    // nowej sygnatury dla każdej klatki po zatrzymaniu się na 100%.
    match overview.corner {
        Some(hint) => {
            mix(1);
            mix(hint.kind as u64);
            mix((hint.progress.clamp(0.0, 1.0) * 255.0).round() as u64);
            mix(hint.top_inset.to_bits());
        }
        None => mix(0),
    }
    let mon_rect = Rect::from_xywh(ox, oy, ow, oh);
    {
        let windows = model.lock_or_recover();
        for w in windows.iter() {
            if !w.mapped || w.minimized || !w.in_workspace(current_ws) {
                continue;
            }
            if !w.outer().intersects(&mon_rect) {
                continue;
            }
            mix(w.x.to_bits());
            mix(w.y.to_bits());
            mix(w.z as u64);
            mix(u64::from(w.focused));
            mix(w.width as u64);
            mix(w.height as u64);
            mix(u64::from(w.mapped) | (u64::from(w.minimized) << 1) | (u64::from(w.layer) << 2));
            // Pasek tytułu, pełny ekran, popup i tytuł zmieniają wygląd bez zmiany geometrii.
            mix(u64::from(w.ssd) | (u64::from(w.fullscreen) << 1) | (u64::from(w.popup) << 2));
            mix(w.title.as_bytes().iter().fold(0u64, |acc, b| {
                acc.wrapping_mul(31).wrapping_add(u64::from(*b))
            }));
            mix(std::sync::Arc::as_ptr(&w.data) as u64);
        }
    }
    if let Some(c) = cursor {
        if mon_rect.contains(c) {
            mix(c.x.to_bits());
            mix(c.y.to_bits());
        }
    }
    h
}

#[allow(dead_code)]
/// Sygnatura sceny (okna + kursor + przestrzeń + podgląd) — do wykrywania zmian.
fn scene_signature(
    model: &WindowModel,
    current_ws: usize,
    overview: Overview,
    cursor: Option<Point>,
) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(current_ws as u64);
    mix(u64::from(overview.on));
    mix(u64::from(overview.blanked));
    mix(wallpaper_stamp());
    if let Some((slot, (px, py))) = overview.drag {
        mix(slot as u64);
        mix(px.to_bits());
        mix(py.to_bits());
    }
    {
        let windows = model.lock_or_recover();
        mix(windows.len() as u64);
        for w in windows.iter() {
            mix(w.x.to_bits());
            mix(w.y.to_bits());
            mix(w.z as u64);
            mix(u64::from(w.focused));
            mix(w.width as u64);
            mix(w.height as u64);
            mix(w.image_offset.0.to_bits());
            mix(w.image_offset.1.to_bits());
            mix(u64::from(w.mapped) | (u64::from(w.minimized) << 1) | (u64::from(w.layer) << 2));
            mix(u64::from(w.ssd) | (u64::from(w.fullscreen) << 1) | (u64::from(w.popup) << 2));
            mix(w.title.as_bytes().iter().fold(0u64, |acc, b| {
                acc.wrapping_mul(31).wrapping_add(u64::from(*b))
            }));
            mix(std::sync::Arc::as_ptr(&w.data) as u64);
        }
    }
    if let Some(c) = cursor {
        mix(c.x.to_bits());
        mix(c.y.to_bits());
    }
    h
}

/// Motyw użytkownika dla dekoracji okien (`~/.config/luna/theme.conf` + styl z pliku).
///
/// Dzięki temu pasek SSD (okna X11 i klienci negocjujący `server_side`) ma dokładnie
/// ten sam wygląd co `HeaderBar` w aplikacjach libLuny — jasny/ciemny, akcent i styl.
fn user_theme() -> Theme {
    let system_dark = libluna_core::appearance::system_dark();
    ThemeSettings::load().resolve_themed(system_dark)
}

/// Buduje scenę okien w rozmiarze wyjścia, przesuniętą o `(ox, oy)` (origin monitora).
#[allow(clippy::too_many_arguments)]
fn build_scene(
    scene: &mut Scene,
    size: Size,
    ox: f64,
    oy: f64,
    model: &WindowModel,
    current_ws: usize,
    ws_count: usize,
    overview: Overview,
    _is_primary: bool,
    cursor: Option<Point>,
    show_cursor: bool,
    theme: &Theme,
    text: &TextEngine,
) {
    let sh = |r: Rect| r.offset(-ox, -oy);
    scene.clear();
    // Ekran wygaszony po bezczynności: czarna klatka zamiast pulpitu i kursora.
    // Wątek wyjścia i tak nie renderuje wtedy nic więcej (sygnatura się nie zmienia),
    // więc CPU/GPU odpoczywają do pierwszego zdarzenia wejścia.
    if overview.blanked {
        scene.fill_rect(
            Rect::from_size(size),
            Color::from_rgba8(0x00, 0x00, 0x00, 0xff),
        );
        return;
    }
    // Tło pulpitu: tapeta z `assets/branding` (gradient zostaje jako zapas).
    match wallpaper_for(size.width.max(1.0) as u32, size.height.max(1.0) as u32) {
        Some(img) => {
            scene.background(img);
        }
        None => fill_vgradient(
            scene,
            Rect::from_size(size),
            Color::from_rgba8(0x23, 0x28, 0x3a, 0xff),
            Color::from_rgba8(0x0f, 0x11, 0x18, 0xff),
        ),
    }

    {
        let windows = model.lock_or_recover();
        let mut order: Vec<&Window> = windows.iter().collect();
        order.sort_by_key(|w| w.z);
        // Przyciemniamy nieaktywne okna tylko wtedy, gdy któreś zwykłe okno ma fokus.
        // Dzięki temu panel/popup z fokusem nie przygasza całego pulpitu.
        let focused_normal = order.iter().any(|w| {
            w.focused
                && !w.popup
                && !w.layer
                && w.mapped
                && !w.minimized
                && w.in_workspace(current_ws)
        });
        let inactive_dim = Color::from_rgba8(0x00, 0x00, 0x00, 0x40);
        // Widoczny obszar tego monitora (globalnie) — do odsiania okien z innych wyjść.
        let out_rect = Rect::from_xywh(ox, oy, size.width, size.height);
        for w in order {
            if !w.mapped || w.minimized || !w.in_workspace(current_ws) {
                continue;
            }
            // Okno z innego monitora nie wnosi pikseli, a jego zaokrąglony clip
            // kosztowałby pełną maskę bufora (kilka ms na klatkę).
            let bounds = w.outer().union(&w.buffer_rect());
            if !bounds.intersects(&out_rect) {
                continue;
            }
            // Tło treści: **nie malujemy czerni** pod buforem klienta. Bufory bywają
            // częściowo przezroczyste (cienie i zaokrąglenia CSD w GTK), a czarne tło
            // przeświecało przez nie jako ramka wokół okna.
            // `buffer_rect()` uwzględnia cienie CSD (`set_window_geometry`) — bufor
            // jest większy od widocznego okna i musi być rysowany z przesunięciem.
            // Zaokrąglone narożniki: wymuszamy dla wszystkich zwykłych okien (także CSD),
            // promieniem z motywu. Maksymalizowane/fullscreen zostają ostre.
            let outer = sh(w.outer());
            // Okno na całą szerokość monitora („przyklejone” do obu krawędzi) nie
            // potrzebuje zaokrągleń — narożniki i tak leżą na krawędzi ekranu, a clip
            // wymusza budowę maski o rozmiarze całego bufora i blokuje szybki blit.
            let spans_width = outer.x() <= 0.5 && outer.right() >= size.width - 0.5;
            let radius = if !w.popup && !w.layer && !w.maximized && !w.fullscreen && !spans_width {
                theme.radius.large
            } else {
                0.0
            };
            if radius > 0.0 {
                scene.push_clip_rounded(outer, radius);
            }
            match &w.dmabuf {
                Some(db) => {
                    scene.image_dmabuf(sh(w.buffer_rect()), std::sync::Arc::clone(db));
                }
                None => {
                    scene.image(sh(w.buffer_rect()), w.data.clone());
                }
            }
            // Pasek tytułu rysujemy **tylko** oknom z naszym SSD (`w.ssd`). Klienty
            // z własnymi dekoracjami (GTK/libadwaita, CSD) nie mają `ssd`, a wcześniej
            // rysowaliśmy im mimo to tytuł i przyciski okna — lewitowały nad treścią.
            if w.ssd {
                // Dokładnie ten sam widget, którego używają aplikacje libLuny
                // (`libluna_widgets::HeaderBar`) — dzięki temu dekoracje kompozytora
                // i aplikacji są identyczne (tło, typografia, kontrolki, odstępy).
                let bar = sh(w.titlebar());
                let mut ctx = PaintCtx::new(scene, theme, text);
                // Podświetlenie kontrolki pod kursorem — bez tego pasek SSD zachowywał
                // się „martwo” w porównaniu z oknami libLuny (tam hover działa).
                let hovered = cursor.and_then(|c| {
                    let p = Point::new(c.x - ox, c.y - oy);
                    if bar.contains(p) {
                        HeaderBar::new().control_at(bar, theme, p)
                    } else {
                        None
                    }
                });
                let mut header = HeaderBar::new()
                    .title(w.title.clone())
                    .controls(ControlsSide::Right);
                // Dopasuj kolor paska do górnej części okna (jeśli bufor ma kryjące piksele).
                if let Some((bg, fg)) = crate::decoration::tint_from_image(&w.data) {
                    header = header.tinted(bg, fg);
                }
                header.paint(&mut ctx, bar, hovered);
                // **Bez** własnej ramki wokół całego okna — libLuna rysuje tylko pasek
                // (z własną linią pod nim). Dodatkowy `stroke` wyglądał jak obca ramka
                // od innego środowiska, zwłaszcza na oknach X11.
            }
            // Nieaktywne okno lekko przygaszone — sąsiednie okna nie zlewają się
            // w jedną płaszczyznę, a fokus jest widoczny bez zgadywania.
            // Rysujemy **przed** zdjęciem clipu, żeby przygaszenie miało zaokrąglone
            // narożniki razem z oknem.
            if focused_normal && !w.focused && !w.popup && !w.layer {
                scene.fill_rect(outer, inactive_dim);
            }
            if radius > 0.0 {
                scene.pop_clip();
            }
        }
    }

    // Podgląd okien i przestrzeni (Super+Tab) — na każdym monitorze (jego okna).
    if overview.on {
        draw_overview(
            scene,
            size,
            ox,
            oy,
            model,
            current_ws,
            ws_count,
            overview.drag,
        );
    }

    // Poświata „gorącego rogu”: rośnie przy przytrzymaniu kursora w rogu ekranu.
    // Rysujemy ją po oknach, ale przed kursorem i tylko gdy podgląd jest zamknięty
    // (podgląd zajmuje cały ekran własnymi kafelkami).
    if !overview.on {
        if let Some(hint) = overview.corner {
            draw_corner_hint(scene, size, ox, oy, hint, theme);
        }
    }

    // Kursor rysuje ten monitor, na którym leży (tryb DRM — nikt go za nas nie narysuje).
    // Obraz (strzałka, kształt z motywu albo bufor klienta) pochodzi z `crate::cursor`.
    if let Some(c) = cursor.filter(|_| show_cursor) {
        let (cx, cy) = (c.x - ox, c.y - oy);
        if cx >= 0.0 && cy >= 0.0 && cx < size.width && cy < size.height {
            crate::cursor::paint(scene, Point::new(cx, cy));
        }
    }
}

/// Wypełnia prostokąt pionowym gradientem (pasami — tanie, a tło nie jest płaskie).
/// Tapeta pulpitu (z `assets/branding`), przeskalowana „cover” do rozmiaru wyjścia.
///
/// Skalowanie robimy **raz** na rozmiar i trzymamy w cache — w pętli renderowania
/// idzie wtedy tani blit 1:1, a nie skalowanie pełnego ekranu co klatkę.
fn wallpaper_for(w: u32, h: u32) -> Option<std::sync::Arc<ImageData>> {
    type SizedCache = Vec<(u64, (u32, u32), std::sync::Arc<ImageData>)>;
    let (w, h) = (w.max(1), h.max(1));
    let stamp = wallpaper_stamp();
    static CACHE: std::sync::OnceLock<std::sync::Mutex<SizedCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = cache.lock_or_recover();
    // Nowa tapeta (mtime / ścieżka) unieważnia przeskalowane kopie starych rozmiarów.
    guard.retain(|(cached_stamp, _, _)| *cached_stamp == stamp);
    if let Some((_, _, img)) = guard
        .iter()
        .find(|(cached_stamp, (cw, ch), _)| *cached_stamp == stamp && *cw == w && *ch == h)
    {
        return Some(std::sync::Arc::clone(img));
    }
    let src = wallpaper_source(stamp)?;
    let img = std::sync::Arc::new(libluna_render::wallpaper::cover(&src, w, h));
    guard.push((stamp, (w, h), std::sync::Arc::clone(&img)));
    Some(img)
}

/// Sygnatura tapety: zmienia się przy podmianie pliku w Ustawieniach.
///
/// Kompozytor działa dalej bez restartu — wystarczy zapis `wallpaper.conf`.
fn wallpaper_stamp() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for var in ["LUNA_WALLPAPER", "SELENE_WALLPAPER", "SELENE_NO_WALLPAPER"] {
        std::env::var_os(var).hash(&mut hasher);
    }
    if let Some(config) = libluna_render::wallpaper::config_path() {
        if let Ok(meta) = std::fs::metadata(&config) {
            meta.modified().ok().hash(&mut hasher);
            meta.len().hash(&mut hasher);
        }
    }
    if let Some(path) = libluna_render::wallpaper::configured_path() {
        path.hash(&mut hasher);
        if let Ok(meta) = std::fs::metadata(&path) {
            meta.modified().ok().hash(&mut hasher);
            meta.len().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Dekoduje tapetę raz na wersję pliku. Źródło: `LUNA_WALLPAPER` / `SELENE_WALLPAPER` /
/// `~/.config/luna/wallpaper.conf`, a w ostateczności wbudowana grafika.
/// Wyłączenie (powrót do gradientu): `SELENE_NO_WALLPAPER=1`.
fn wallpaper_source(stamp: u64) -> Option<std::sync::Arc<ImageData>> {
    type SourceCache = Option<(u64, Option<std::sync::Arc<ImageData>>)>;
    static SRC: std::sync::OnceLock<std::sync::Mutex<SourceCache>> = std::sync::OnceLock::new();
    let cache = SRC.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = cache.lock_or_recover();
    if let Some((cached_stamp, src)) = guard.as_ref() {
        if *cached_stamp == stamp {
            return src.clone();
        }
    }
    let src = if std::env::var_os("SELENE_NO_WALLPAPER").is_some() {
        None
    } else {
        libluna_render::wallpaper::configured_path()
            .and_then(|path| libluna_render::wallpaper::load_path(&path))
            .map(std::sync::Arc::new)
            .or_else(|| {
                libluna_render::wallpaper::decode_png(include_bytes!(
                    "../../assets/branding/paganlinux.png"
                ))
                .map(std::sync::Arc::new)
            })
    };
    *guard = Some((stamp, src.clone()));
    src
}

fn fill_vgradient(scene: &mut Scene, rect: Rect, top: Color, bottom: Color) {
    const BANDS: usize = 64;
    let h = rect.height() / BANDS as f64;
    for i in 0..BANDS {
        let t = i as f32 / (BANDS - 1) as f32;
        let c = top.lerp(bottom, t);
        scene.fill_rect(
            Rect::from_xywh(rect.x(), rect.y() + i as f64 * h, rect.width(), h + 1.0),
            c,
        );
    }
}

/// Rysuje podgląd okien i przestrzeni — kafelki z podglądem treści + pasek przestrzeni.
#[allow(clippy::too_many_arguments)]
/// Rysuje poświatę „gorącego rogu”.
///
/// Kilka półprzezroczystych kół zakotwiczonych w narożniku ekranu (widoczna jest
/// tylko ćwiartka) daje miękką poświatę bez gradientów. Rozmiar i krycie rosną
/// z postępem przytrzymania, więc użytkownik widzi, że akcja zaraz zadziała.
fn draw_corner_hint(
    scene: &mut Scene,
    size: Size,
    ox: f64,
    oy: f64,
    hint: CornerHint,
    theme: &Theme,
) {
    let progress = hint.progress.clamp(0.0, 1.0) as f32;
    let (x, y) = match hint.kind {
        CornerKind::TopLeft => (ox, oy + hint.top_inset),
        CornerKind::BottomLeft => (ox, oy + size.height),
        CornerKind::BottomRight => (ox + size.width, oy + size.height),
    };
    let center = Point::new(x - ox, y - oy);
    let accent = theme.colors.accent;
    // Warstwy: (skala promienia, krycie). Środek jaśniejszy, brzeg delikatny.
    let radius = 56.0 + 84.0 * f64::from(progress);
    for (scale, alpha) in [
        (1.0f64, 0.08 + 0.10 * progress),
        (0.74, 0.09 + 0.11 * progress),
        (0.48, 0.10 + 0.12 * progress),
    ] {
        scene.fill_circle(center, radius * scale, accent.scale_alpha(alpha));
    }
}

fn draw_overview(
    scene: &mut Scene,
    size: Size,
    ox: f64,
    oy: f64,
    model: &WindowModel,
    current_ws: usize,
    ws_count: usize,
    drag: Option<(usize, (f64, f64))>,
) {
    // Przygaszenie tła.
    scene.fill_rect(
        Rect::from_size(size),
        Color::from_rgba8(0x0b, 0x0d, 0x13, 0xd6),
    );
    let slots = {
        let w = model.lock_or_recover();
        crate::state::overview_slots_on(&w, current_ws, ox, size.width)
    };
    let tiles = crate::state::overview_tiles(slots.len(), (size.width, size.height));
    let windows = model.lock_or_recover();
    for (k, &slot) in slots.iter().enumerate() {
        let (Some(r), Some(w)) = (tiles.get(k), windows.get(slot)) else {
            continue;
        };
        let title_h = crate::state::OVERVIEW_TITLE;
        scene.fill_rounded_rect(*r, 8.0, Color::from_rgba8(0x1a, 0x1d, 0x25, 0xff));
        let bar = Rect::from_xywh(r.x(), r.y(), r.width(), title_h);
        scene.fill_rounded_rect(bar, 8.0, Color::from_rgba8(0x2c, 0x2f, 0x38, 0xff));
        let img = Rect::from_xywh(
            r.x(),
            r.y() + title_h,
            r.width(),
            (r.height() - title_h).max(1.0),
        );
        match &w.dmabuf {
            Some(db) => scene.image_dmabuf(img, std::sync::Arc::clone(db)),
            None => scene.image(img, w.data.clone()),
        };
        scene.stroke_rounded_rect(*r, 8.0, 1.5, Color::from_rgba8(0x6c, 0x74, 0x86, 0xff));
        scene.text(
            TextRun::new(Point::new(r.x() + 10.0, r.y() + 18.0), &w.title, 13.0)
                .with_color(Color::from_rgba8(0xee, 0xee, 0xee, 0xff)),
        );
    }
    // Pasek przestrzeni roboczych.
    let chips = crate::state::overview_ws_chips(ws_count, (size.width, size.height));
    for (i, c) in chips.iter().enumerate() {
        let bg = if i == current_ws {
            Color::from_rgba8(0x5b, 0x8d, 0xef, 0xff)
        } else {
            Color::from_rgba8(0x2c, 0x2f, 0x38, 0xff)
        };
        scene.fill_rounded_rect(*c, c.height() / 2.0, bg);
        scene.text(
            TextRun::new(
                Point::new(c.center().x - 6.0, c.center().y + 6.0),
                &format!("{}", i + 1),
                16.0,
            )
            .with_color(Color::from_rgba8(0xf5, 0xf5, 0xf5, 0xff)),
        );
    }
    // „Duch” przeciąganego okna pod kursorem.
    if let Some((slot, (px, py))) = drag {
        let local = Point::new(px - ox, py - oy);
        let g = Rect::from_xywh(local.x - 90.0, local.y - 60.0, 180.0, 120.0);
        scene.fill_rounded_rect(g, 8.0, Color::from_rgba8(0x5b, 0x8d, 0xef, 0xcc));
        scene.stroke_rounded_rect(g, 8.0, 2.0, Color::from_rgba8(0xff, 0xff, 0xff, 0xdd));
        if let Some(w) = windows.get(slot) {
            scene.text(
                TextRun::new(Point::new(g.x() + 10.0, g.y() + 20.0), &w.title, 13.0)
                    .with_color(Color::from_rgba8(0xff, 0xff, 0xff, 0xff)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libluna_render::SoftwareRenderer;

    #[test]
    fn corner_glow_paints_the_corner() {
        let theme = Theme::dark();
        let size = Size::new(200.0, 200.0);
        let mut scene = Scene::new();
        scene.fill_rect(Rect::from_size(size), Color::from_rgba8(0, 0, 0, 255));
        draw_corner_hint(
            &mut scene,
            size,
            0.0,
            0.0,
            CornerHint {
                kind: CornerKind::BottomRight,
                progress: 0.9,
                top_inset: 0.0,
            },
            &theme,
        );
        let mut renderer = SoftwareRenderer::new(200, 200);
        renderer.draw(&scene, 1.0, Color::from_rgba8(0, 0, 0, 255));
        let data = renderer.data();
        let at = |x: usize, y: usize| {
            let i = (y * 200 + x) * 4;
            [data[i], data[i + 1], data[i + 2]]
        };
        assert_ne!(at(199, 199), [0, 0, 0], "róg ma być podświetlony");
        assert_ne!(at(170, 170), [0, 0, 0], "poświata ma mieć miękki zasięg");
        assert_eq!(at(60, 60), [0, 0, 0], "środek ekranu zostaje czysty");
    }

    #[test]
    fn blanked_scene_is_black() {
        use std::sync::{Arc, Mutex};
        let model: WindowModel = Arc::new(Mutex::new(Vec::new()));
        let theme = Theme::dark();
        let text = TextEngine::none();
        let mut scene = Scene::new();
        let overview = Overview {
            blanked: true,
            ..Overview::default()
        };
        build_scene(
            &mut scene,
            Size::new(64.0, 64.0),
            0.0,
            0.0,
            &model,
            0,
            4,
            overview,
            true,
            None,
            false,
            &theme,
            &text,
        );
        let mut renderer = SoftwareRenderer::new(64, 64);
        renderer.draw(&scene, 1.0, Color::from_rgba8(0, 0, 0, 255));
        assert!(
            renderer
                .data()
                .chunks_exact(4)
                .all(|px| px[0] == 0 && px[1] == 0 && px[2] == 0),
            "wygaszony ekran ma być cały czarny (bez tapety, okien i kursora)"
        );
    }

    #[test]
    fn software_cursor_paints_pixels() {
        // Kursor w scenie rysuje `crate::cursor` (domyślnie wbudowana strzałka).
        let mut scene = Scene::new();
        crate::cursor::paint(&mut scene, Point::new(8.0, 8.0));
        let mut renderer = SoftwareRenderer::new(48, 48);
        renderer.draw(&scene, 1.0, Color::from_rgba8(0, 0, 0, 255));
        let painted = renderer
            .data()
            .chunks_exact(4)
            .any(|px| px[0] > 0 || px[1] > 0 || px[2] > 0);
        assert!(painted, "kursor powinien zostawić niezerowe piksele");
    }

    #[test]
    #[ignore = "generuje podglad do docs/images (uruchom recznie)"]
    fn render_desktop_preview() {
        use std::sync::{Arc, Mutex};
        let model: WindowModel = Arc::new(Mutex::new(Vec::new()));
        let mut scene = Scene::new();
        build_scene(
            &mut scene,
            Size::new(1920.0, 1080.0),
            0.0,
            0.0,
            &model,
            0,
            1,
            Overview::default(),
            true,
            None,
            true,
            &Theme::dark(),
            &TextEngine::none(),
        );
        let mut renderer = SoftwareRenderer::new(1920, 1080);
        renderer.draw(&scene, 1.0, Color::from_rgba8(0x1e, 0x20, 0x28, 0xff));
        let png = renderer.encode_png().expect("PNG");
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/images/luna-desktop.png"
        );
        std::fs::write(path, png).expect("zapis PNG");
        println!("saved {path}");
    }

    #[test]
    fn power_saver_caps_presentation_at_30_fps() {
        assert_eq!(throttle_delay(false, Duration::from_millis(5)), None);
        assert_eq!(
            throttle_delay(true, Duration::from_millis(10)),
            Some(Duration::from_millis(23))
        );
        assert_eq!(throttle_delay(true, Duration::from_millis(40)), None);
    }

    #[test]
    fn window_content_is_visible_in_render() {
        use std::sync::{Arc, Mutex};
        let mut rgba = vec![0u8; 100 * 80 * 4];
        for px in rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&[0xff, 0x00, 0x00, 0xff]);
        }
        let model: WindowModel = Arc::new(Mutex::new(vec![Window {
            x: 10.0,
            y: 10.0,
            width: 100,
            height: 80,
            image_offset: (0.0, 0.0),
            z: 1,
            title: String::new(),
            popup: false,
            layer: false,
            mapped: true,
            focused: true,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: false,
            saved: None,
            dmabuf: None,
            data: Arc::new(ImageData::new(100, 80, rgba)),
        }]));
        let mut scene = Scene::new();
        build_scene(
            &mut scene,
            Size::new(200.0, 150.0),
            0.0,
            0.0,
            &model,
            0,
            1,
            Overview::default(),
            true,
            None,
            false,
            &Theme::dark(),
            &TextEngine::none(),
        );
        let mut renderer = SoftwareRenderer::new(200, 150);
        renderer.draw(&scene, 1.0, Color::BLACK);
        let px = &renderer.data()[(50 * 200 + 50) * 4..][..4];
        assert_eq!(px, [0xff, 0x00, 0x00, 0xff], "treść okna musi być widoczna");

        // Ten sam test na GPU — tu wychodziły różnice (treść okna bywała pomijana).
        if let Ok(mut gpu) = libluna_render::gpu::GpuRenderer::new(200, 150) {
            gpu.draw(&scene, 1.0, Color::BLACK);
            let px = &gpu.data()[(50 * 200 + 50) * 4..][..4];
            assert_eq!(
                px,
                [0xff, 0x00, 0x00, 0xff],
                "GPU: treść okna musi być widoczna"
            );
        }
    }

    #[test]
    fn desktop_background_is_the_wallpaper() {
        use std::sync::{Arc, Mutex};
        let model: WindowModel = Arc::new(Mutex::new(Vec::new()));
        let mut scene = Scene::new();
        build_scene(
            &mut scene,
            Size::new(320.0, 180.0),
            0.0,
            0.0,
            &model,
            0,
            1,
            Overview::default(),
            true,
            None,
            true,
            &Theme::dark(),
            &TextEngine::none(),
        );
        // Pierwsze polecenie to tło pulpitu — tapeta przeskalowana dokładnie do rozmiaru
        // wyjścia (renderer kopiuje ją wprost), a nie gradient z 64 pasów.
        match &scene.commands()[0] {
            libluna_render::DrawCommand::Background { data } => {
                assert_eq!((data.width, data.height), (320, 180));
            }
            other => panic!("oczekiwano tapety, a jest {other:?}"),
        }
    }

    #[test]
    fn unfocused_window_is_dimmed() {
        use libluna_render::DrawCommand;
        use std::sync::{Arc, Mutex};
        let data = Arc::new(ImageData::new(100, 80, vec![0xffu8; 100 * 80 * 4]));
        let window = |x: f64, z: i32, focused: bool| Window {
            x,
            y: 0.0,
            width: 100,
            height: 80,
            image_offset: (0.0, 0.0),
            z,
            title: String::new(),
            popup: false,
            layer: false,
            mapped: true,
            focused,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: false,
            saved: None,
            dmabuf: None,
            data: data.clone(),
        };
        let model: WindowModel = Arc::new(Mutex::new(vec![
            window(0.0, 1, false),
            window(20.0, 2, true),
        ]));
        let mut scene = Scene::new();
        build_scene(
            &mut scene,
            Size::new(400.0, 300.0),
            0.0,
            0.0,
            &model,
            0,
            1,
            Overview::default(),
            true,
            None,
            false,
            &Theme::dark(),
            &TextEngine::none(),
        );
        let dim = Color::from_rgba8(0x00, 0x00, 0x00, 0x40);
        let dimmed: Vec<Rect> = scene
            .commands()
            .iter()
            .filter_map(|command| match command {
                DrawCommand::FillRect { rect, color } if *color == dim => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(dimmed, vec![Rect::from_xywh(0.0, 0.0, 100.0, 80.0)]);
    }

    #[test]
    fn ssd_header_highlights_hovered_close_like_libluna() {
        use libluna_render::DrawCommand;
        use std::sync::{Arc, Mutex};
        let data = Arc::new(ImageData::new(400, 300, vec![0u8; 400 * 300 * 4]));
        let windows = vec![Window {
            x: 0.0,
            y: 0.0,
            width: 400,
            height: 300,
            image_offset: (0.0, 0.0),
            z: 1,
            title: "Test".to_owned(),
            popup: false,
            layer: false,
            mapped: true,
            focused: false,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: true,
            saved: None,
            dmabuf: None,
            data,
        }];
        let model: WindowModel = Arc::new(Mutex::new(windows));
        let theme = Theme::light();
        let has_error_fill = |scene: &Scene| {
            scene.commands().iter().any(|c| {
                matches!(
                    c,
                    DrawCommand::FillRoundedRect { color, .. } if *color == theme.colors.error
                )
            })
        };
        // Środek kontrolki „zamknij”: szerokość 400, pad 6, bok 30.
        let close = Point::new(400.0 - 6.0 - 30.0 + 15.0, 23.0);
        let mut scene = Scene::new();
        build_scene(
            &mut scene,
            Size::new(800.0, 600.0),
            0.0,
            0.0,
            &model,
            0,
            1,
            Overview::default(),
            true,
            Some(close),
            false,
            &theme,
            &TextEngine::none(),
        );
        assert!(
            has_error_fill(&scene),
            "hover na „zamknij” powinien dać czerwone tło jak HeaderBar libLuny"
        );

        // Poza kontrolkami — brak podświetlenia.
        let mut scene = Scene::new();
        build_scene(
            &mut scene,
            Size::new(800.0, 600.0),
            0.0,
            0.0,
            &model,
            0,
            1,
            Overview::default(),
            true,
            Some(Point::new(100.0, 23.0)),
            false,
            &theme,
            &TextEngine::none(),
        );
        assert!(!has_error_fill(&scene));
    }

    #[test]
    fn ssd_header_takes_color_from_window_content() {
        use libluna_render::DrawCommand;
        use std::sync::{Arc, Mutex};
        // Ciemny, kryjący pasek narzędzi w górnych 28 px → pasek SSD w tym samym kolorze.
        let (w, h) = (400u32, 300u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..28u32 {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[0x11, 0x22, 0x33, 0xff]);
            }
        }
        let model: WindowModel = Arc::new(Mutex::new(vec![Window {
            x: 0.0,
            y: 0.0,
            width: 400,
            height: 300,
            image_offset: (0.0, 0.0),
            z: 1,
            title: "Test".to_owned(),
            popup: false,
            layer: false,
            mapped: true,
            focused: false,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: true,
            saved: None,
            dmabuf: None,
            data: Arc::new(ImageData::new(w, h, rgba)),
        }]));
        let mut scene = Scene::new();
        build_scene(
            &mut scene,
            Size::new(800.0, 600.0),
            0.0,
            0.0,
            &model,
            0,
            1,
            Overview::default(),
            true,
            None,
            false,
            &Theme::dark(),
            &TextEngine::none(),
        );
        let tint = Color::from_rgba8(0x11, 0x22, 0x33, 0xff);
        assert!(
            scene.commands().iter().any(|c| matches!(
                c,
                DrawCommand::FillRect { rect, color }
                    if *color == tint && rect.y() == 0.0 && rect.height() == crate::state::TITLEBAR
            )),
            "pasek SSD powinien przyjąć kolor górnej części treści okna"
        );
    }

    #[test]
    #[ignore = "benchmark reczny"]
    fn bench_1080p_frame() {
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        let model: WindowModel = Arc::new(Mutex::new(Vec::new()));
        let bg = Color::from_rgba8(0x1e, 0x20, 0x28, 0xff);
        let mut renderer = SoftwareRenderer::new(1920, 1080);
        let n = 30;
        let t = Instant::now();
        for _ in 0..n {
            let mut scene = Scene::new();
            build_scene(
                &mut scene,
                Size::new(1920.0, 1080.0),
                0.0,
                0.0,
                &model,
                0,
                1,
                Overview::default(),
                true,
                Some(Point::new(100.0, 100.0)),
                true,
                &Theme::dark(),
                &TextEngine::none(),
            );
            renderer.draw(&scene, 1.0, bg);
        }
        println!(
            "render 1920x1080: {:.2} ms/klatke",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }

    #[test]
    #[ignore = "benchmark reczny"]
    fn bench_1080p_with_panel() {
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        let data = Arc::new(libluna_render::ImageData::new(
            1920,
            680,
            vec![128u8; 1920 * 680 * 4],
        ));
        let windows = vec![Window {
            x: 0.0,
            y: 0.0,
            width: 1920,
            height: 680,
            image_offset: (0.0, 0.0),
            z: 1_000_000,
            title: String::new(),
            popup: false,
            layer: true,
            mapped: true,
            focused: false,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: true,
            saved: None,
            dmabuf: None,
            data,
        }];
        let model: WindowModel = Arc::new(Mutex::new(windows));
        let bg = Color::from_rgba8(0x1e, 0x20, 0x28, 0xff);
        let mut renderer = SoftwareRenderer::new(1920, 1080);
        let n = 20;
        // Sam render sceny:
        let t = Instant::now();
        for _ in 0..n {
            let mut scene = Scene::new();
            build_scene(
                &mut scene,
                Size::new(1920.0, 1080.0),
                0.0,
                0.0,
                &model,
                0,
                1,
                Overview::default(),
                true,
                Some(Point::new(100.0, 100.0)),
                true,
                &Theme::dark(),
                &TextEngine::none(),
            );
            renderer.draw(&scene, 1.0, bg);
        }
        println!(
            "render 1080p + panel 1920x680: {:.2} ms/klatke",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }

    #[test]
    #[ignore = "benchmark reczny"]
    fn bench_sparse_panel() {
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        let (w, h) = (1920u32, 680u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let mut fill = |x0: u32, y0: u32, x1: u32, y1: u32| {
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i..i + 4].copy_from_slice(&[30, 30, 30, 255]);
                }
            }
        };
        // Pasek na całą szerokość + launcher 720×600 (reszta przezroczysta).
        fill(0, 0, w, 48);
        fill(600, 72, 1320, 672);
        let data = Arc::new(ImageData::new(w, h, rgba).with_sparse(true));
        let windows = vec![Window {
            x: 0.0,
            y: 0.0,
            width: w as i32,
            height: h as i32,
            image_offset: (0.0, 0.0),
            z: 1_000_000,
            title: String::new(),
            popup: false,
            layer: true,
            mapped: true,
            focused: false,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: true,
            saved: None,
            dmabuf: None,
            data,
        }];
        let model: WindowModel = Arc::new(Mutex::new(windows));
        let bg = Color::from_rgba8(0x1e, 0x20, 0x28, 0xff);
        let mut renderer = SoftwareRenderer::new(1920, 1080);
        let n = 20;
        let t = Instant::now();
        for _ in 0..n {
            let mut scene = Scene::new();
            build_scene(
                &mut scene,
                Size::new(1920.0, 1080.0),
                0.0,
                0.0,
                &model,
                0,
                1,
                Overview::default(),
                true,
                Some(Point::new(100.0, 100.0)),
                true,
                &Theme::dark(),
                &TextEngine::none(),
            );
            renderer.draw(&scene, 1.0, bg);
        }
        println!(
            "render 1080p + sparse panel: {:.2} ms/klatke",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }
}
