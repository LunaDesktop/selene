//! Globalny stan kompozytora Selene.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use libluna_core::idle::IdleSettings;
use libluna_core::{Modifiers, Point, Rect};
use libluna_render::ImageData;
use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
    ext_workspace_handle_v1::ExtWorkspaceHandleV1, ext_workspace_manager_v1::ExtWorkspaceManagerV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_server::protocol::wl_data_device::WlDataDevice;
use wayland_server::protocol::wl_data_source::WlDataSource;
use wayland_server::protocol::wl_keyboard::WlKeyboard;
use wayland_server::protocol::wl_output::WlOutput;
use wayland_server::protocol::wl_pointer::WlPointer;
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::DisplayHandle;
use wayland_server::Resource;

use crate::data::DndState;
use crate::monitors::{OutputInfo, Outputs};
use crate::sync::LockExt;

/// Wysokość paska tytułu dekoracji (SSD), w pikselach logicznych.
/// Wysokość naszego paska tytułu (SSD). Musi być równa
/// `theme.metrics.headerbar.height` — kompozytor rysuje pasek widgetem libLuny.
pub const TITLEBAR: f64 = 38.0;
/// Szerokość przycisku sterowania oknem (minimalizuj/maksymalizuj/zamknij).
pub const BUTTON_W: f64 = 30.0;
/// Odstęp kontrolek od prawej krawędzi paska (token `spacing.sm`).
const CONTROL_PAD: f64 = 6.0;
/// Odstęp między kontrolkami (token `headerbar.control_gap`).
const CONTROL_GAP: f64 = 2.0;
/// Grubość krawędzi wyłapywanej do zmiany rozmiaru.
pub const BORDER: f64 = 6.0;

/// Zapamiętana geometria przed maksymalizacją/fullscreenem (do przywrócenia).
#[derive(Debug, Clone, Copy)]
pub struct SavedGeom {
    pub x: f64,
    pub y: f64,
    pub width: i32,
    pub height: i32,
}

/// Okno (toplevel/popup) w modelu kompozycji: pozycja, rozmiar, kolejność i piksele.
pub struct Window {
    /// Lewy-górny róg **widocznego** okna (`xdg_surface.set_window_geometry`),
    /// nie bufora — bufor okien CSD zawiera dodatkowo cienie.
    pub x: f64,
    pub y: f64,
    /// Rozmiar **widocznego** okna (bez cieni CSD). To on idzie w `configure`
    /// i wyznacza trafienia; bufor klienta może być większy o `image_offset`.
    pub width: i32,
    pub height: i32,
    /// Przesunięcie bufora względem widocznego okna (`xdg_surface.set_window_geometry`).
    /// Cienie CSD są rysowane poza obrębem okna i nie powinny przesuwać trafień.
    pub image_offset: (f64, f64),
    /// Kolejność (wyższa = wyżej); do renderu i trafień.
    pub z: i32,
    /// Czy okno ma fokus klawiatury (nieaktywne okna są lekko przyciemniane).
    pub focused: bool,
    /// Tytuł okna (z `xdg_toplevel.set_title`).
    pub title: String,
    /// Czy to `xdg_popup` (bez dekoracji, zamykany kliknięciem poza).
    pub popup: bool,
    /// Czy to powierzchnia `wlr-layer-shell` (panel/dock; bez dekoracji, kotwiczona).
    pub layer: bool,
    /// Czy powierzchnia jest widoczna (popup znika po `popup_done`).
    pub mapped: bool,
    /// Przestrzeń robocza, do której należy okno (panele/popupy są globalne).
    pub workspace: usize,
    /// Region wejścia powierzchni (`wl_surface.set_input_region`); `None` = cała.
    pub input: Option<Vec<Rect>>,
    /// Czy powierzchnia jawnie przechwytuje **wszystkie** kliknięcia (otwarty popup
    /// powłoki). Wtedy klik poza jej widoczną treścią/poza buforem jest przekazywany
    /// właśnie jej — dzięki temu popup zamyka się także po kliknięciu poza obrysem.
    pub input_capture: bool,
    /// Zminimalizowane (ukryte przez kompozytor; przywracane skrótem).
    pub minimized: bool,
    /// Zmaksymalizowane do obszaru monitora.
    pub maximized: bool,
    /// Pełny ekran.
    pub fullscreen: bool,
    /// Czy rysujemy **nasze** dekoracje (SSD). `true` tylko dla okien, które
    /// negocjowały tryb `server_side`; klienty z własnymi dekoracjami (GTK, X11) — `false`.
    pub ssd: bool,
    /// Geometria zapamiętana przed maksymalizacją/fullscreenem.
    pub saved: Option<SavedGeom>,
    /// Piksele (premultiplied RGBA8) ostatnio zacommitowanego bufora.
    pub data: Arc<ImageData>,
    /// Bufor GPU klienta (`zwp_linux_dmabuf_v1`); import robi backend rysujący.
    /// Gdy jest ustawiony, `data` to pusta atrapa 1×1 (bez kopii pikseli na CPU).
    pub dmabuf: Option<Arc<libluna_render::DmabufImage>>,
}

impl Window {
    /// Czy okno pokazuje się w danej przestrzeni roboczej.
    ///
    /// Powierzchnie warstwowe (pasek, tło, OSD) są globalne — należą do każdej
    /// przestrzeni; zwykłe okna i popupy widoczne są tylko w swojej.
    pub fn in_workspace(&self, ws: usize) -> bool {
        self.layer || self.workspace == ws
    }

    /// Wysokość dekoracji (zero dla popupów, paneli i pełnego ekranu).
    pub fn bar(&self) -> f64 {
        // Pasek rysujemy **tylko** gdy okno o to poprosiło (`ssd`). Klienty GTK/
        // libadwaita zawsze rysują własne dekoracje (CSD) — dokładanie naszych dawało
        // podwójny pasek tytułu, a okna X11 (XWayland) swoje dekoracje przynoszą ze sobą.
        if self.popup || self.layer || self.fullscreen || !self.ssd {
            0.0
        } else {
            TITLEBAR
        }
    }

    /// Prostokąt całego okna (dekoracja + treść).
    pub fn outer(&self) -> Rect {
        Rect::from_xywh(
            self.x,
            self.y,
            self.width as f64,
            self.bar() + self.height as f64,
        )
    }

    /// Prostokąt, w którym rysujemy bufor klienta.
    ///
    /// Dla okien CSD bufor jest większy od widocznego okna (cienie) — rysujemy go
    /// przesuniętego o `image_offset`, a nie rozciągniętego do `content()`.
    pub fn buffer_rect(&self) -> Rect {
        let c = self.content();
        let (iw, ih) = self.image_size();
        Rect::from_xywh(
            c.x() - self.image_offset.0,
            c.y() - self.image_offset.1,
            iw as f64,
            ih as f64,
        )
    }

    /// Rozmiar obrazu klienta: `dmabuf` albo bufor shm.
    pub fn image_size(&self) -> (u32, u32) {
        match &self.dmabuf {
            Some(d) => (d.width, d.height),
            None => (self.data.width, self.data.height),
        }
    }

    /// Pasek tytułu (zerowy dla popupów).
    pub fn titlebar(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.width as f64, self.bar())
    }

    /// Obszar treści (klienta).
    pub fn content(&self) -> Rect {
        Rect::from_xywh(
            self.x,
            self.y + self.bar(),
            self.width as f64,
            self.height as f64,
        )
    }

    /// Przycisk sterowania oknem licząc od prawej (0 = zamknij, 1 = maksymalizuj, 2 = minimalizuj).
    fn control_rect(&self, from_right: usize) -> Rect {
        // Kontrolki rysujemy **tylko** oknom z naszym paskiem (SSD). Wcześniej
        // istniały też dla okien CSD — niewidzialne strefy close/max/min leżały
        // wtedy na treści aplikacji i „czasem” zamykały/maksymalizowały okno
        // zamiast przekazać klik do klienta.
        if self.popup || self.layer || !self.ssd {
            return Rect::ZERO;
        }
        // Układ jak w `libluna_widgets::HeaderBar`: kwadratowe kontrolki 30 px,
        // 2 px odstępu, 6 px od prawej krawędzi, wyśrodkowane w pionie.
        let side = (TITLEBAR - BUTTON_W) / 2.0;
        Rect::from_xywh(
            self.x + self.width as f64
                - CONTROL_PAD
                - BUTTON_W * (from_right as f64 + 1.0)
                - CONTROL_GAP * from_right as f64,
            self.y + side,
            BUTTON_W,
            BUTTON_W,
        )
    }

    /// Kwadratowy przycisk zamknięcia (tylko dla topleveli).
    pub fn close_rect(&self) -> Rect {
        self.control_rect(0)
    }

    /// Przycisk maksymalizacji.
    pub fn maximize_rect(&self) -> Rect {
        self.control_rect(1)
    }

    /// Przycisk minimalizacji.
    pub fn minimize_rect(&self) -> Rect {
        self.control_rect(2)
    }

    /// Bitmaska krawędzi wyłapywanej do zmiany rozmiaru dla punktu (0 gdy brak).
    /// 1 = góra, 2 = dół, 4 = lewo, 8 = prawo.
    pub fn resize_edges(&self, p: Point) -> u8 {
        if self.popup || self.layer || self.fullscreen || self.maximized {
            return 0;
        }
        let o = self.outer();
        if !o.contains(p) {
            return 0;
        }
        let mut e = 0u8;
        if p.x <= o.x() + BORDER {
            e |= 4;
        }
        if p.x >= o.right() - BORDER {
            e |= 8;
        }
        if p.y <= o.y() + BORDER {
            e |= 1;
        }
        if p.y >= o.bottom() - BORDER {
            e |= 2;
        }
        e
    }
}

/// Współdzielony model okien (pisany przez wątek protokołu, czytany przez wyjście).
pub type WindowModel = Arc<Mutex<Vec<Window>>>;

/// Żądanie zrzutu ekranu (`Super+F12`).
///
/// Kompozytor ma w pamięci gotową klatkę każdego monitora, więc zrzut to zapis tych
/// buforów do PNG — bez odpytywania DRM. Flagę ustawia wątek protokołu (skrót),
/// a realizuje wątek wyjścia.
pub type ScreenshotModel = Arc<std::sync::atomic::AtomicBool>;

/// Siła night lightu (0 = wyłączony, 100 = najmocniej ciepło).
pub type NightLightModel = Arc<std::sync::atomic::AtomicU8>;

/// Tryb oszczędzania energii (kompozytor ogranicza wtedy tempo prezentacji).
pub type PowerSaverModel = Arc<std::sync::atomic::AtomicBool>;

/// Żądanie zmiany trybu monitora: `(nazwa złącza, tryb)`; `None` = automatyczny.
///
/// Ustawia je gniazdo sterowania (`output-mode …`), a wątek wyjścia wykonuje
/// realny modeset i odbudowuje bufory oraz renderery.
pub type DisplayRequestModel = Arc<Mutex<Option<(String, Option<libluna_core::OutputMode>)>>>;

/// Żądanie przesunięcia monitora w układzie: `(nazwa, x, y)`.
///
/// Pozycja nie wymaga modesetu — zmienia model wyjść i geometrię wysyłaną klientom.
pub type PositionRequestModel = Arc<Mutex<Option<(String, i32, i32)>>>;

/// Współdzielona pozycja kursora (tryb DRM/evdev); `None` = nie rysujemy kursora.
pub type CursorModel = Arc<Mutex<Option<Point>>>;

/// Obraz kursora: domyślna strzałka, nazwany kształt (`wp_cursor_shape_v1`)
/// albo bufor dostarczony przez klienta (`wl_pointer.set_cursor`).
#[derive(Clone, PartialEq)]
pub enum CursorImage {
    /// Klient ukrył kursor (`wl_pointer.set_cursor` z NULL) — np. gry i odtwarzacze.
    Hidden,
    Default,
    Shape(String),
    Custom {
        data: Arc<libluna_render::ImageData>,
        hotspot: (i32, i32),
    },
}

impl std::fmt::Debug for CursorImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hidden => f.write_str("hidden"),
            Self::Default => f.write_str("default"),
            Self::Shape(name) => write!(f, "shape:{name}"),
            Self::Custom { data, hotspot } => write!(
                f,
                "custom:{}x{}@{},{} ",
                data.width, data.height, hotspot.0, hotspot.1
            ),
        }
    }
}

/// Współdzielony obraz kursora (kompozytor ↔ wątek wyjścia).
pub type CursorImageModel = Arc<Mutex<CursorImage>>;

/// Współdzielony numer bieżącej przestrzeni roboczej (kompozytor ↔ wątek wyjścia).
pub type WorkspaceModel = Arc<Mutex<usize>>;
/// Liczba przestrzeni roboczych współdzielona z wątkiem wyjścia.
pub type WorkspaceCountModel = Arc<std::sync::atomic::AtomicUsize>;

/// Stan podglądu okien/przestrzeni (współdzielony z wątkiem wyjścia).
#[derive(Debug, Clone, Copy, Default)]
pub struct Overview {
    /// Czy podgląd jest włączony.
    pub on: bool,
    /// Przeciągane okno: `(slot, pozycja kursora)`.
    pub drag: Option<(usize, (f64, f64))>,
    /// Podpowiedź „gorącego rogu” (poświata przy przytrzymaniu kursora).
    /// Współdzielona z wątkiem wyjścia, który rysuje ją na scenie.
    pub corner: Option<CornerHint>,
    /// Czy ekran jest wygaszony (czarna klatka, kursor schowany).
    /// Trzymamy to razem z modelem rysowania, żeby wątek wyjścia widział zmianę
    /// bez dodatkowej synchronizacji.
    pub blanked: bool,
}

/// Który róg ekranu jest aktualnie „przytrzymywany” kursorem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CornerKind {
    /// Lewy górny róg obszaru roboczego (poniżej panelu) — podgląd jak `Super+Tab`.
    TopLeft,
    /// Dolny lewy róg — poprzednia przestrzeń.
    BottomLeft,
    /// Dolny prawy róg — następna przestrzeń.
    BottomRight,
}

/// Stan poświaty w rogu: rodzaj, postęp przytrzymania (0–1) i górny margines panelu.
///
/// `progress = 0` to pierwsza klatka po wjechaniu w róg; `1` oznacza pełne
/// przytrzymanie (akcja już wykonana). Rysuje to wątek wyjścia — dlatego dane
/// muszą być `Copy` i tanie w kopiowaniu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CornerHint {
    pub kind: CornerKind,
    pub progress: f64,
    /// Wysokość panelu na tym monitorze (dla lewego górnego rogu).
    pub top_inset: f64,
}

/// Współdzielony tryb podglądu okien/przestrzeni (kompozytor ↔ wątek wyjścia).
pub type OverviewModel = Arc<Mutex<Overview>>;

/// Margines podglądu (od krawędzi ekranu).
pub const OVERVIEW_PAD: f64 = 56.0;
/// Odstęp między kafelkami podglądu.
pub const OVERVIEW_GAP: f64 = 16.0;
/// Wysokość pasa „chipsów” przestrzeni u dołu podglądu.
pub const OVERVIEW_STRIP_H: f64 = 56.0;
/// Wysokość paska tytułu kafelka.
pub const OVERVIEW_TITLE: f64 = 26.0;

/// Sloty okien bieżącej przestrzeni (bez paneli/popupów), w kolejności z-order.
#[allow(dead_code)]
pub fn overview_slots(windows: &[Window], current_ws: usize) -> Vec<usize> {
    let mut v: Vec<(usize, i32)> = windows
        .iter()
        .enumerate()
        .filter(|(_, w)| {
            w.mapped && !w.minimized && !w.popup && !w.layer && w.workspace == current_ws
        })
        .map(|(i, w)| (i, w.z))
        .collect();
    v.sort_by_key(|&(_, z)| z);
    v.into_iter().map(|(i, _)| i).collect()
}

/// Sloty okien bieżącej przestrzeni, których **środek** leży na wyjściu `[ox, ox+w)`.
pub fn overview_slots_on(windows: &[Window], current_ws: usize, ox: f64, w: f64) -> Vec<usize> {
    let mut v: Vec<(usize, i32)> = windows
        .iter()
        .enumerate()
        .filter(|(_, win)| {
            win.mapped && !win.minimized && !win.popup && !win.layer && win.workspace == current_ws
        })
        .filter(|(_, win)| {
            let cx = win.x + win.width as f64 / 2.0;
            cx >= ox && cx < ox + w
        })
        .map(|(i, w)| (i, w.z))
        .collect();
    v.sort_by_key(|&(_, z)| z);
    v.into_iter().map(|(i, _)| i).collect()
}

/// Prostokąty kafelków w siatce na obszarze `size` (zostawiając pas u dołu).
pub fn overview_tiles(count: usize, size: (f64, f64)) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let cols = (count as f64).sqrt().ceil() as usize;
    let cols = cols.max(1);
    let rows = count.div_ceil(cols);
    let avail_w = (size.0 - 2.0 * OVERVIEW_PAD).max(1.0);
    let avail_h = (size.1 - 2.0 * OVERVIEW_PAD - OVERVIEW_STRIP_H).max(1.0);
    let tw = ((avail_w - (cols - 1) as f64 * OVERVIEW_GAP) / cols as f64).max(1.0);
    let th = ((avail_h - (rows - 1) as f64 * OVERVIEW_GAP) / rows as f64).max(1.0);
    let grid_w = cols as f64 * tw + (cols - 1) as f64 * OVERVIEW_GAP;
    let grid_h = rows as f64 * th + (rows - 1) as f64 * OVERVIEW_GAP;
    let x0 = (size.0 - grid_w) / 2.0;
    let y0 = OVERVIEW_PAD + ((avail_h - grid_h) / 2.0).max(0.0);
    (0..count)
        .map(|k| {
            let c = (k % cols) as f64;
            let r = (k / cols) as f64;
            Rect::from_xywh(
                x0 + c * (tw + OVERVIEW_GAP),
                y0 + r * (th + OVERVIEW_GAP),
                tw,
                th,
            )
        })
        .collect()
}

/// Prostokąty „chipsów” przestrzeni na dole podglądu.
pub fn overview_ws_chips(count: usize, size: (f64, f64)) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let (w, h, gap) = (128.0, 36.0, 12.0);
    let total = count as f64 * w + (count - 1) as f64 * gap;
    let x0 = (size.0 - total) / 2.0;
    let y = size.1 - OVERVIEW_PAD - h;
    (0..count)
        .map(|i| Rect::from_xywh(x0 + i as f64 * (w + gap), y, w, h))
        .collect()
}

/// Trwające przeciąganie okna.
#[derive(Debug, Clone, Copy)]
pub struct DragState {
    pub slot: usize,
    /// Przesunięcie kursora względem lewego-górnego narożnika okna w chwili wciśnięcia.
    pub off_x: f64,
    pub off_y: f64,
    /// Pozycja kursora w chwili rozpoczęcia przeciągania — pozwala odróżnić
    /// zwykły klik na pasku tytułu od realnego przeciągnięcia do krawędzi.
    pub start_px: f64,
    pub start_py: f64,
    /// Geometria okna w chwili rozpoczęcia przeciągania. To ona wraca jako rozmiar
    /// domyślny po przyciągnięciu do krawędzi (a nie pozycja upuszczenia).
    pub origin: SavedGeom,
}

/// Trwające przeskalowanie okna (za dowolną krawędź/narożnik).
#[derive(Debug, Clone, Copy)]
pub struct ResizeState {
    pub slot: usize,
    /// Bitmaska krawędzi (1=góra, 2=dół, 4=lewo, 8=prawo).
    pub edges: u8,
    pub start_x: f64,
    pub start_y: f64,
    pub start_w: f64,
    pub start_h: f64,
    pub start_px: f64,
    pub start_py: f64,
}

/// Stan współdzielony przez wszystkie handlery protokołu.
pub struct State {
    /// Start pracy kompozytora — do znaczników czasu (frame callbacks).
    pub start: Instant,
    /// Licznik seriali (dla `xdg_surface.configure` itp.).
    serial: u32,
    /// Liczniki diagnostyczne.
    pub clients: u32,
    pub surfaces: u32,
    pub toplevels: u32,
    /// Model okien współdzielony z wątkiem wyjścia.
    pub windows: WindowModel,
    /// Pozycja kursora do narysowania (tryb DRM/evdev), współdzielona z wątkiem wyjścia.
    pub cursor: CursorModel,
    /// Obraz kursora (kształt z `wp_cursor_shape_v1` albo bufor klienta).
    pub cursor_image: CursorImageModel,
    /// Powierzchnia kursora z `wl_pointer.set_cursor` — jej commity aktualizują obraz.
    pub cursor_surface: Option<WlSurface>,
    /// Hotspot obrazu klienta (piksele powierzchni kursora).
    pub cursor_hotspot: (i32, i32),
    /// Ostatnie wciśnięcie lewego przycisku na pasku tytułu: `(slot, czas)`.
    /// Pozwala rozpoznać podwójny klik (maksymalizacja/przywrcenie domyślnego rozmiaru).
    pub last_title_click: Option<(usize, Instant)>,
    /// Powierzchnie okien (indeks = slot w [`State::windows`]).
    pub slots: Vec<WlSurface>,
    /// Licznik kolejności (z-order).
    z_counter: i32,
    /// Monitory (współdzielone z wątkiem wyjścia).
    pub outputs: Outputs,
    /// Podłączeni menedżerowie `zwlr_foreign_toplevel` (paski/taskbary).
    pub toplevel_managers: Vec<ZwlrForeignToplevelManagerV1>,

    // — Przestrzenie robocze (`ext_workspace_v1`) —
    /// Liczba przestrzeni roboczych.
    pub ws_count: usize,
    /// Liczba przestrzeni współdzielona z wątkiem wyjścia (rysuje podgląd i pasek).
    pub ws_count_model: Arc<std::sync::atomic::AtomicUsize>,
    /// Nazwy przestrzeni (puste = numer). Czytane z `~/.config/luna/workspaces.conf`.
    pub ws_names: Vec<String>,
    /// Czy dolne rogi ekranu przełączają przestrzenie (opcja użytkownika).
    pub ws_corner_switch: bool,
    /// Czy lewy górny róg otwiera podgląd okien/przestrzeni (Super+Tab).
    pub ws_overview_corner: bool,
    /// Zapadka: róg już zadziałał, dopóki kursor z niego nie wyjdzie.
    pub ws_corner_latch: bool,
    /// Róg, w którym kursor stoi teraz (None = poza strefami).
    pub ws_corner_hover: Option<CornerKind>,
    /// Kiedy kursor wszedł w bieżący róg (początek odliczania przytrzymania).
    pub ws_corner_since: Option<Instant>,
    /// Górny margines panelu zapamiętany przy wejściu w róg (rysowanie poświaty).
    pub ws_corner_top: f64,
    /// Bieżąca przestrzeń robocza (współdzielona z wątkiem wyjścia).
    pub ws_current: WorkspaceModel,
    /// Tryb podglądu okien/przestrzeni (`Super+Tab`), współdzielony z wątkiem wyjścia.
    pub overview: OverviewModel,
    /// Podłączeni menedżerowie `ext_workspace` oraz ich grupy i uchwyty przestrzeni.
    pub ws_managers: Vec<ExtWorkspaceManagerV1>,
    pub ws_groups: Vec<ExtWorkspaceGroupHandleV1>,
    pub ws_handles: Vec<ExtWorkspaceHandleV1>,
    /// Uchwyt wyświetlania — potrzebny do tworzenia nowych uchwytów przestrzeni
    /// przy zmianie liczby w Ustawieniach (bez restartu Selene).
    pub display_handle: Option<DisplayHandle>,

    // — Wejście —
    /// Wskaźniki/klawiatury **per klient**. Jeden globalny zasób prowadził do wysyłania
    /// zdarzeń z powierzchnią obcego klienta (panic „objects from wrong client”) oraz do
    /// braku wejścia w aplikacjach uruchomionych z launchera.
    pub pointers: Vec<WlPointer>,
    pub keyboards: Vec<WlKeyboard>,
    pub pointer_focus: Option<(usize, WlSurface)>,
    pub keyboard_focus: Option<(usize, WlSurface)>,
    /// Modyfikatory, o których już powiadomiliśmy klienta klawiatury.
    pub sent_mods: Modifiers,
    /// Blokady (`wl_keyboard.modifiers.mods_locked`): CapsLock i NumLock.
    /// Bez tego klienty mogły ignorować stan blokad, a numeryczna klawiatura
    /// dawała kody nawigacji zamiast cyfr/przecinków.
    pub caps_lock: bool,
    pub num_lock: bool,
    /// Ustawienia powtarzania klawiszy (ms / klawisze na sekundę).
    pub repeat_delay: u32,
    pub repeat_rate: u32,
    /// Odwrócony kierunek przewijania (naturalne przewijanie).
    pub natural_scroll: bool,
    /// Ustawienia bezczynności (`~/.config/luna/idle.conf`).
    pub idle: IdleSettings,
    /// Ostatnie zdarzenie wejścia (zegar wygaszania/blokady/usypiania).
    pub idle_last_input: Instant,
    /// Blokada z powodu bezczynności już wystartowała (zapadka, do czasu aktywności).
    pub idle_lock_fired: bool,
    /// Usypianie z powodu bezczynności już wystartowało (zapadka).
    pub idle_suspend_fired: bool,
    /// Sesja zablokowana przez `luna-lock` (overlay + klawiatura wyłączna).
    /// Blokujemy wtedy globalne skróty kompozytora i podgląd okien.
    pub locked: bool,
    /// Bieżąca selekcja (schowek): źródło danych.
    pub selection: Option<WlDataSource>,
    /// Urządzenia schowka zarejestrowane przez klientów (do rozsyłania selekcji).
    pub data_devices: Vec<WlDataDevice>,
    /// Zasoby `wl_output` związane przez klientów. Trzymamy je, żeby po faktycznym
    /// modesecie DRM wysłać poprawione `geometry`/`mode`/`scale`.
    pub bound_outputs: Vec<(usize, WlOutput)>,
    /// Sygnatura listy monitorów ostatnio rozesłanej klientom.
    pub outputs_sig: u64,
    /// Ile globali `wl_output` już utworzono (może się zwiększyć po faktycznym
    /// wykryciu większej liczby monitorów przez DRM).
    pub output_globals: usize,
    /// Stan trwającego przeciągania (drag & drop).
    pub dnd: Option<DndState>,
    /// Ostatnia pozycja wskaźnika (współrzędne globalne).
    pub pointer_pos: (f64, f64),
    /// Czy lewy przycisk wskaźnika jest wciśnięty. Potrzebne, bo `xdg_toplevel.move`
    /// przychodzi od klienta **asynchronicznie** — przy bardzo szybkim kliknięciu
    /// docierał już po zwolnieniu przycisku i zostawiał „przyklejone” przeciąganie.
    pub left_down: bool,
    /// Strefa wyłączna górnych paneli, **per indeks wyjścia**. Panel jest tylko na
    /// jednym monitorze — drugi nie może dostawać u góry pustej szczeliny.
    pub top_insets: std::collections::HashMap<usize, f64>,
    /// Trwające przeciąganie okna.
    pub drag: Option<DragState>,
    /// Trwające przeskalowanie okna.
    pub resize: Option<ResizeState>,
    /// Żądanie zrzutu ekranu (Super+F12) — przełącza je wątek wyjścia.
    pub screenshot: ScreenshotModel,
    /// Siła night lightu (gamma LUT) — ustawiana z powłoki przez gniazdo sterowania.
    pub night_light: NightLightModel,
    /// Tryb oszczędzania (limit tempa prezentacji w wątku wyjścia).
    pub power_saver: PowerSaverModel,
    /// Żądanie zmiany trybu monitora z Ustawień (`luna-control output-mode`).
    pub display_request: DisplayRequestModel,
    /// Żądanie przesunięcia monitora (`luna-control output-position`).
    pub position_request: PositionRequestModel,
}

impl Default for State {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            serial: 0,
            clients: 0,
            surfaces: 0,
            toplevels: 0,
            windows: Arc::new(Mutex::new(Vec::new())),
            cursor: Arc::new(Mutex::new(None)),
            cursor_image: Arc::new(Mutex::new(CursorImage::Default)),
            cursor_surface: None,
            cursor_hotspot: (0, 0),
            slots: Vec::new(),
            z_counter: 1,
            outputs: Arc::new(Mutex::new(vec![OutputInfo::default()])),
            toplevel_managers: Vec::new(),
            ws_count: 4,
            ws_count_model: Arc::new(std::sync::atomic::AtomicUsize::new(4)),
            ws_names: Vec::new(),
            ws_corner_switch: false,
            ws_overview_corner: true,
            ws_corner_latch: false,
            ws_corner_hover: None,
            ws_corner_since: None,
            ws_corner_top: 0.0,
            ws_current: Arc::new(Mutex::new(0)),
            overview: Arc::new(Mutex::new(Overview::default())),
            ws_managers: Vec::new(),
            ws_groups: Vec::new(),
            ws_handles: Vec::new(),
            display_handle: None,
            pointers: Vec::new(),
            keyboards: Vec::new(),
            pointer_focus: None,
            keyboard_focus: None,
            sent_mods: Modifiers::NONE,
            caps_lock: false,
            num_lock: false,
            repeat_delay: 600,
            repeat_rate: 25,
            natural_scroll: false,
            locked: false,
            selection: None,
            data_devices: Vec::new(),
            bound_outputs: Vec::new(),
            outputs_sig: 0,
            output_globals: 0,
            dnd: None,
            pointer_pos: (0.0, 0.0),
            left_down: false,
            top_insets: std::collections::HashMap::new(),
            last_title_click: None,
            drag: None,
            resize: None,
            screenshot: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            night_light: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            display_request: Arc::new(Mutex::new(None)),
            position_request: Arc::new(Mutex::new(None)),
            power_saver: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            idle: IdleSettings::default(),
            idle_last_input: Instant::now(),
            idle_lock_fired: false,
            idle_suspend_fired: false,
        }
    }
}

impl State {
    /// Kolejny serial protokołu.
    pub fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    /// Czas od startu w milisekundach (dla `wl_callback.done`).
    pub fn time_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Kolejna wartość z-order (podnosi okno na wierzch).
    pub fn next_z(&mut self) -> i32 {
        self.z_counter += 1;
        self.z_counter
    }

    /// Nazwa przestrzeni `index` (numer, gdy użytkownik nie ustawił nazwy).
    pub fn workspace_name(&self, index: usize) -> String {
        self.ws_names
            .get(index)
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| (index + 1).to_string())
    }

    /// Wczytuje liczbę i nazwy przestrzeni z konfiguracji użytkownika.
    ///
    /// Liczba obowiązuje od startu kompozytora; nazwy można potem zmieniać
    /// w locie przez `luna-control` (`workspace-name`).
    pub fn apply_workspace_layout(&mut self) {
        let layout = libluna_core::WorkspaceLayout::load();
        self.ws_count = layout.count();
        self.ws_count_model
            .store(self.ws_count, std::sync::atomic::Ordering::Relaxed);
        self.ws_names = layout.names();
        self.ws_corner_switch = layout.corner_switch();
        self.ws_overview_corner = layout.overview_corner();
    }

    /// Wczytuje ustawienia klawiatury i myszy (`~/.config/luna/input.conf`).
    pub fn apply_input_settings(&mut self) {
        let input = libluna_core::InputSettings::load();
        self.repeat_delay = input.repeat_delay();
        self.repeat_rate = input.repeat_rate();
        self.natural_scroll = input.natural_scroll();
    }

    /// Włącza/wyłącza przełączanie przestrzeni dolnymi rogami ekranu.
    pub fn set_workspace_corner_switch(&mut self, enabled: bool) {
        self.ws_corner_switch = enabled;
        if !enabled {
            self.reset_corner();
        }
    }

    /// Włącza/wyłącza podgląd (Super+Tab) w lewym górnym rogu.
    pub fn set_workspace_overview_corner(&mut self, enabled: bool) {
        self.ws_overview_corner = enabled;
        if !enabled {
            self.reset_corner();
        }
    }

    /// Zmienia liczbę przestrzeni bez restartu kompozytora.
    ///
    /// Uchwyty `ext_workspace_v1` są dodawane/usuwane i rozgłaszane do podłączonych
    /// pasków — panel oraz podgląd `Super+Tab` widzą nowy zestaw od razu.
    pub fn set_workspace_count(&mut self, count: usize) {
        let count = count.clamp(libluna_core::MIN_WORKSPACES, libluna_core::MAX_WORKSPACES);
        if count == self.ws_count {
            return;
        }
        self.ws_count = count;
        self.ws_count_model
            .store(count, std::sync::atomic::Ordering::Relaxed);
        self.ws_corner_latch = false;
        let current = *self.ws_current.lock_or_recover();
        if current >= count {
            crate::seat::switch_workspace(self, count.saturating_sub(1));
        }
        crate::workspace::sync_workspace_handles(self);
    }

    /// Ustawia nazwę przestrzeni i rozgłasza ją podłączonym klientom.
    pub fn set_workspace_name(&mut self, index: usize, name: &str) {
        if index >= self.ws_count {
            return;
        }
        let name = name.trim();
        let name = if name.is_empty() {
            (index + 1).to_string()
        } else {
            name.to_owned()
        };
        if self.ws_names.len() <= index {
            self.ws_names.resize(index + 1, String::new());
        }
        self.ws_names[index] = name.clone();
        for handle in &self.ws_handles {
            if handle.data::<usize>() == Some(&index) {
                let _ = handle.send_event(
                    wayland_protocols::ext::workspace::v1::server::ext_workspace_handle_v1::Event::Name {
                        name: name.clone(),
                    },
                );
            }
        }
    }

    /// Przeładowuje ustawienia bezczynności (po `luna-control idle-reload`).
    pub fn reload_idle(&mut self) {
        self.idle = IdleSettings::load();
        // Skrócenie progu poniżej bieżącej bezczynności ma zadziałać od razu.
        self.idle_lock_fired = false;
        self.idle_suspend_fired = false;
    }

    /// Zapisuje aktywność użytkownika: zeruje zegar, wygasza poświatę i budzi ekran.
    ///
    /// Wywoływane przy **każdym** zdarzeniu wejścia — także wtedy, gdy aplikacja
    /// ma fokus wyłączny (np. gra), bo i tam użytkownik jest obecny.
    pub fn note_activity(&mut self) {
        self.idle_last_input = Instant::now();
        self.idle_suspend_fired = false;
        if !self.locked {
            self.idle_lock_fired = false;
        }
        self.set_blanked(false);
    }

    /// Włącza/wyłącza wygaszenie ekranu (widoczne od razu w wątku wyjścia).
    pub fn set_blanked(&mut self, on: bool) {
        let mut overview = self.overview.lock_or_recover();
        if overview.blanked == on {
            return;
        }
        overview.blanked = on;
        drop(overview);
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!(
                "selene/idle: ekran {}",
                if on { "wygaszony" } else { "włączony" }
            );
        }
    }

    /// Czy ekran jest aktualnie wygaszony.
    pub fn is_blanked(&self) -> bool {
        self.overview.lock_or_recover().blanked
    }

    /// Czyści stan rogu: brak poświaty i brak aktywnego odliczania.
    pub fn reset_corner(&mut self) {
        self.ws_corner_hover = None;
        self.ws_corner_since = None;
        self.ws_corner_latch = false;
        self.overview.lock_or_recover().corner = None;
    }

    /// Czy trwa podgląd okien/przestrzeni.
    pub fn overview_on(&self) -> bool {
        self.overview.lock_or_recover().on
    }

    /// Włącza/wyłącza podgląd okien/przestrzeni (wyłączenie kończy przeciąganie).
    pub fn set_overview(&self, on: bool) {
        let mut o = self.overview.lock_or_recover();
        o.on = on;
        if !on {
            o.drag = None;
        }
    }

    /// Ustawia przeciągane okno podglądu: `(slot, pozycja kursora)`.
    pub fn set_overview_drag(&self, drag: Option<(usize, (f64, f64))>) {
        self.overview.lock_or_recover().drag = drag;
    }

    /// Aktualizuje tylko pozycję przeciąganego okna (zachowując slot).
    pub fn move_overview_drag(&self, pos: (f64, f64)) {
        let mut o = self.overview.lock_or_recover();
        if let Some((slot, _)) = o.drag {
            o.drag = Some((slot, pos));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serials_are_non_zero_and_increasing() {
        let mut s = State::default();
        let a = s.next_serial();
        let b = s.next_serial();
        assert!(a >= 1);
        assert!(b > a);
    }

    #[test]
    fn z_order_increases() {
        let mut s = State::default();
        let a = s.next_z();
        let b = s.next_z();
        assert!(b > a);
    }

    fn window_at_ws(ws: usize, z: i32) -> Window {
        Window {
            x: 0.0,
            y: 0.0,
            width: 10,
            height: 10,
            image_offset: (0.0, 0.0),
            z,
            title: String::new(),
            popup: false,
            layer: false,
            mapped: true,
            focused: false,
            workspace: ws,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: true,
            saved: None,
            dmabuf: None,
            data: std::sync::Arc::new(libluna_render::ImageData::empty(1, 1)),
        }
    }

    #[test]
    fn overview_slots_filter_and_order() {
        let mut panel = window_at_ws(0, 100);
        panel.layer = true;
        let ws = vec![
            window_at_ws(0, 5),
            window_at_ws(1, 9),
            window_at_ws(0, 1),
            panel,
        ];
        // Tylko okna bieżącej przestrzeni, bez panelu, w kolejności z.
        assert_eq!(overview_slots(&ws, 0), vec![2, 0]);
        assert_eq!(overview_slots(&ws, 1), vec![1]);
    }

    #[test]
    fn overview_slots_split_by_output() {
        let mut a = window_at_ws(0, 1);
        a.x = 100.0;
        a.width = 200; // środek 200 → monitor 0
        let mut b = window_at_ws(0, 2);
        b.x = 2000.0;
        b.width = 100; // środek 2050 → monitor 1
        let ws = vec![a, b];
        assert_eq!(overview_slots_on(&ws, 0, 0.0, 1920.0), vec![0]);
        assert_eq!(overview_slots_on(&ws, 0, 1920.0, 1280.0), vec![1]);
        assert!(overview_slots_on(&ws, 1, 0.0, 1920.0).is_empty());
    }

    #[test]
    fn overview_tiles_and_chips_layout() {
        let size = (1920.0, 1080.0);
        assert!(overview_tiles(0, size).is_empty());
        let four = overview_tiles(4, size);
        assert_eq!(four.len(), 4);
        // Drugi kafelek na prawo od pierwszego i bez nachodzenia.
        assert!(four[1].x() > four[0].x());
        assert!(four[0].right() <= four[1].x());
        for r in &four {
            assert!(r.x() >= 0.0 && r.right() <= size.0 + 0.5);
            assert!(r.bottom() <= size.1 + 0.5);
        }
        let chips = overview_ws_chips(4, size);
        assert_eq!(chips.len(), 4);
        assert!(chips[0].x() < chips[3].x());
    }

    #[test]
    fn csd_buffer_is_drawn_shifted_by_window_geometry() {
        // Okno CSD: widoczny prostokąt to (100,50)+200x120, a bufor 228x144
        // (cienie 14/12 po każdej stronie). Bufor rysujemy od (86,38), nie od (100,50).
        let mut w = window_at_ws(0, 1);
        w.x = 100.0;
        w.y = 50.0;
        w.width = 200;
        w.height = 120;
        w.ssd = false;
        w.image_offset = (14.0, 12.0);
        w.data = std::sync::Arc::new(libluna_render::ImageData::empty(228, 144));
        let b = w.buffer_rect();
        assert_eq!(
            (b.x(), b.y(), b.width(), b.height()),
            (86.0, 38.0, 228.0, 144.0)
        );
        // Trafienia liczymy od widocznego okna, nie od cienia.
        assert!(w.content().contains(Point::new(100.0, 50.0)));
        assert!(!w.content().contains(Point::new(95.0, 45.0)));
    }
}
