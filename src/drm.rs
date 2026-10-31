//! Wykrywanie monitorów przez DRM/KMS (przenośne — działa na dowolnym Linuksie z `/dev/dri`).
//!
//! To świadomie **szkic**: otwieramy karty DRM, odpytujemy KMS o podłączone konektory
//! i ich tryby, a wynik zwracamy jako model [`OutputInfo`]. Gdy DRM jest niedostępne
//! (np. w środowisku bez uprawnień albo w trybie nested), zwracamy `None` i kompozytor
//! korzysta z pojedynczego wyjścia syntetycznego.
//!
//! Skanowanie (page-flip, alokacja buforów, master) dochodzi później — tutaj chodzi o to,
//! by Selene wiedział, ile i jakich monitorów ma użyć.

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;

use rustix::fs::{Mode, OFlags};
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};

use libluna_core::{DisplaySettings, OutputMode};

use crate::monitors::{layout_horizontal, OutputInfo};

// — Kody ioctl KMS (`_IOWR`) —
const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC040_64A0;
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC050_64A7;
const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC014_64A6;
const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC068_64A1;
const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC068_64A2;
/// `drm_mode_crtc_lut` (`sizeof` = 32 = 0x20).
const DRM_IOCTL_MODE_SETGAMMA: u32 = 0xC020_64A4;
const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC020_64B2;
const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC010_64B3;
const DRM_IOCTL_MODE_DESTROY_DUMB: u32 = 0xC004_64B4;
const DRM_IOCTL_MODE_ADDFB2: u32 = 0xC068_64B8;
/// `drm_mode_rmfb` — usuwa framebuffer (przy zmianie trybu na żywo).
const DRM_IOCTL_MODE_RMFB: u32 = 0xC004_64AF;
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC018_64B0;
/// `drm_mode_cursor` (`sizeof` = 28 = 0x1C).
const DRM_IOCTL_MODE_CURSOR: u32 = 0xC01C_64A3;

// — ioctle bez argumentu (`_IO`) —
const DRM_IOCTL_SET_MASTER: u32 = 0x0000_641E;
const DRM_IOCTL_DROP_MASTER: u32 = 0x0000_641F;

const DRM_MODE_CONNECTED: u32 = 1;
/// `DRM_MODE_TYPE_PREFERRED` (bit 3) — natywny tryb monitora.
const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
/// `DRM_MODE_PAGE_FLIP_EVENT` — proś o zdarzenie po zakończeniu flipu.
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 1;
/// Cztery znaki `XRGB8888` (kolejność bajtów B,G,R,X).
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// Cztery znaki `XBGR8888` (kolejność bajtów R,G,B,X) — **taka sama jak wyjście
/// tiny-skia**, więc klatkę można skopiować bez przepisywania kanałów.
const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;
/// Cztery znaki `ARGB8888` (kolejność bajtów B,G,R,A) — format kursora.
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
/// `DRM_MODE_CURSOR_BO` — ustaw obraz kursora (albo wyłącz, gdy `handle` = 0).
const DRM_MODE_CURSOR_BO: u32 = 0x01;
/// `DRM_MODE_CURSOR_MOVE` — przesuń kursor bez zmiany obrazu.
const DRM_MODE_CURSOR_MOVE: u32 = 0x02;
/// `DRM_EVENT_FLIP_COMPLETE`.
const DRM_EVENT_FLIP_COMPLETE: u32 = 2;

/// `struct drm_mode_card_res`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

/// `struct drm_mode_get_connector`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

/// `struct drm_mode_modeinfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

/// `struct drm_mode_get_encoder`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

/// `struct drm_mode_crtc` (używany też przez `GETCRTC`/`SETCRTC`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

/// `struct drm_mode_crtc_lut` — tablica korekcji gamma (night light).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeCrtcLut {
    crtc_id: u32,
    gamma_size: u32,
    red: u64,
    green: u64,
    blue: u64,
}

/// `struct drm_mode_create_dumb`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

/// `struct drm_mode_map_dumb`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

/// `struct drm_mode_destroy_dumb`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ModeDestroyDumb {
    handle: u32,
}

/// `struct drm_mode_fb_cmd2`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeFbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

/// `struct drm_mode_crtc_page_flip`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModePageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

/// `struct drm_mode_cursor` — ustawienie/przesunięcie kursora sprzętowego.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeCursor {
    flags: u32,
    crtc_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    handle: u32,
}

/// Opakowanie na ioctl typu „read-write” z jednym struktem wejścia/wyjścia.
struct Rw<T, const OP: u32>(MaybeUninit<T>);

impl<T, const OP: u32> Rw<T, OP> {
    fn new(value: T) -> Self {
        Self(MaybeUninit::new(value))
    }
}

unsafe impl<T: Copy, const OP: u32> rustix::ioctl::Ioctl for Rw<T, OP> {
    type Output = T;
    const OPCODE: rustix::ioctl::Opcode =
        rustix::ioctl::Opcode::old(OP as rustix::ioctl::RawOpcode);
    const IS_MUTATING: bool = true;

    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        self.0.as_mut_ptr().cast()
    }

    unsafe fn output_from_ptr(
        _: rustix::ioctl::IoctlOutput,
        extract: *mut core::ffi::c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(*(extract as *const T))
    }
}

/// Wykonuje ioctl „read-write” i zwraca zmodyfikowany strukt.
fn ioctl_rw<T: Copy, const OP: u32, F: AsFd>(fd: F, value: T) -> rustix::io::Result<T> {
    unsafe { rustix::ioctl::ioctl(fd, Rw::<T, OP>::new(value)) }
}

/// ioctl bez argumentu (np. `SET_MASTER`/`DROP_MASTER`).
struct NoArg<const OP: u32>;

unsafe impl<const OP: u32> rustix::ioctl::Ioctl for NoArg<OP> {
    type Output = ();
    const OPCODE: rustix::ioctl::Opcode =
        rustix::ioctl::Opcode::old(OP as rustix::ioctl::RawOpcode);
    const IS_MUTATING: bool = false;

    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        core::ptr::null_mut()
    }

    unsafe fn output_from_ptr(
        _: rustix::ioctl::IoctlOutput,
        _: *mut core::ffi::c_void,
    ) -> rustix::io::Result<()> {
        Ok(())
    }
}

fn ioctl_none<const OP: u32, F: AsFd>(fd: F) -> rustix::io::Result<()> {
    unsafe { rustix::ioctl::ioctl(fd, NoArg::<OP>) }
}

/// Lista ścieżek kart DRM (`/dev/dri/card*`), posortowana.
fn card_paths() -> Vec<PathBuf> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("card"))
                .unwrap_or(false)
        })
        .collect();
    cards.sort();
    cards
}

/// Szczegółowy raport wykrywania (`SELENE_DRM_PROBE=1`).
///
/// Nie zmienia trybu i nie wymaga bycia DRM masterem — wypisuje wszystkie karty,
/// konektory i tryby zgłoszone przez EDID, razem z tym, który Selene by wybrał.
pub fn probe_report() -> String {
    let mut out = String::new();
    let mut used = false;
    for card in card_paths() {
        let Ok(fd) = rustix::fs::open(&card, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty()) else {
            continue;
        };
        let master = ioctl_none::<DRM_IOCTL_SET_MASTER, _>(&fd).is_ok();
        if master {
            let _ = ioctl_none::<DRM_IOCTL_DROP_MASTER, _>(&fd);
        }
        let Some(res) = fetch_resources(fd.as_fd()) else {
            out.push_str(&format!(
                "{}: master={} (nie można odczytać zasobów)\n",
                card.display(),
                master
            ));
            continue;
        };
        let mut connected = 0usize;
        let mut report = String::new();
        for id in &res.connectors {
            let Some((c, modes)) = read_connector(fd.as_fd(), *id) else {
                continue;
            };
            let name = connector_name(c.connector_type, c.connector_type_id);
            if c.connection != DRM_MODE_CONNECTED {
                report.push_str(&format!("  {name}: disconnected\n"));
                continue;
            }
            connected += 1;
            report.push_str(&format!(
                "  {name}: connected, {}x{} mm, tryby={}\n",
                c.mm_width,
                c.mm_height,
                modes.len()
            ));
            for m in &modes {
                let preferred = m.type_ & DRM_MODE_TYPE_PREFERRED != 0;
                report.push_str(&format!(
                    "    {}x{}@{} Hz{}\n",
                    m.hdisplay,
                    m.vdisplay,
                    m.vrefresh,
                    if preferred { "  [PREFERRED]" } else { "" }
                ));
            }
            if let Some(best) = pick_best_mode(&modes) {
                report.push_str(&format!(
                    "    → wybrany: {}x{}@{} Hz{}\n",
                    best.hdisplay,
                    best.vdisplay,
                    best.vrefresh,
                    if best.type_ & DRM_MODE_TYPE_PREFERRED != 0 {
                        " [PREFERRED]"
                    } else {
                        ""
                    }
                ));
            }
        }
        out.push_str(&format!(
            "{}: master={} konektory={} podłączone={}{}\n",
            card.display(),
            master,
            res.connectors.len(),
            connected,
            if !used && connected > 0 {
                "  ← discover() użyje tej karty"
            } else {
                ""
            }
        ));
        out.push_str(&report);
        if connected > 0 {
            used = true;
        }
    }
    if out.is_empty() {
        out.push_str("brak kart DRM w /dev/dri\n");
    }
    out
}

/// Odczytuje konektor wraz z pełną listą trybów (bez zmiany trybu).
fn read_connector(fd: BorrowedFd, id: u32) -> Option<(ModeGetConnector, Vec<ModeInfo>)> {
    let c = ioctl_rw::<ModeGetConnector, DRM_IOCTL_MODE_GETCONNECTOR, _>(
        fd,
        ModeGetConnector {
            connector_id: id,
            ..ModeGetConnector::default()
        },
    )
    .ok()?;
    let count = c.count_modes as usize;
    if c.connection != DRM_MODE_CONNECTED || count == 0 {
        return Some((c, Vec::new()));
    }
    let mut modes = vec![ModeInfo::default(); count];
    let c = ioctl_rw::<ModeGetConnector, DRM_IOCTL_MODE_GETCONNECTOR, _>(
        fd,
        ModeGetConnector {
            connector_id: id,
            count_modes: count as u32,
            modes_ptr: modes.as_mut_ptr() as u64,
            ..ModeGetConnector::default()
        },
    )
    .ok()?;
    Some((c, modes))
}

/// Wykrywa monitory. `None` oznacza, że DRM/KMS nie jest dostępne (użyj wyjścia nested).
///
/// Preferujemy kartę, na której **można zostać masterem** — dokładnie tę samą, którą
/// wybierze [`DrmBackend::open`]. Wcześniej braliśmy pierwszą kartę z podłączonymi
/// monitorami, więc na maszynie z dwoma GPU model wyjść mógł pochodzić z innej karty
/// niż faktyczny scanout.
pub fn discover() -> Option<Vec<OutputInfo>> {
    let stats = std::env::var("SELENE_STATS").is_ok();
    let mut fallback: Option<Vec<OutputInfo>> = None;
    for card in card_paths() {
        let Ok(fd) = rustix::fs::open(&card, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty()) else {
            continue;
        };
        let master = ioctl_none::<DRM_IOCTL_SET_MASTER, _>(&fd).is_ok();
        if master {
            let _ = ioctl_none::<DRM_IOCTL_DROP_MASTER, _>(&fd);
        }
        let Some(mut outs) = probe_card(fd.as_fd()) else {
            continue;
        };
        if outs.is_empty() {
            continue;
        }
        layout_horizontal(&mut outs);
        if stats {
            eprintln!(
                "selene/drm: {} master={} wyjścia={}",
                card.display(),
                master,
                outs.len()
            );
        }
        if master {
            return Some(outs);
        }
        fallback.get_or_insert(outs);
    }
    fallback
}

/// Odpytuje jedną kartę DRM o podłączone konektory.
fn probe_card(fd: BorrowedFd) -> Option<Vec<OutputInfo>> {
    // Najpierw liczymy konektory (bez tablic).
    let res =
        ioctl_rw::<ModeCardRes, DRM_IOCTL_MODE_GETRESOURCES, _>(fd, ModeCardRes::default()).ok()?;
    let count = res.count_connectors as usize;
    if count == 0 {
        return Some(Vec::new());
    }

    // Pobieramy identyfikatory konektorów.
    let mut ids = vec![0u32; count];
    let req = ModeCardRes {
        count_connectors: count as u32,
        connector_id_ptr: ids.as_mut_ptr() as u64,
        ..ModeCardRes::default()
    };
    ioctl_rw::<ModeCardRes, DRM_IOCTL_MODE_GETRESOURCES, _>(fd, req).ok()?;

    let mut outs = Vec::new();
    for id in ids {
        if let Some(info) = probe_connector(fd, id) {
            outs.push(info);
        }
    }
    Some(outs)
}

/// Odpytuje jeden konektor: status, tryby, wybiera tryb natywny (lub pierwszy).
fn probe_connector(fd: BorrowedFd, id: u32) -> Option<OutputInfo> {
    let c = ioctl_rw::<ModeGetConnector, DRM_IOCTL_MODE_GETCONNECTOR, _>(
        fd,
        ModeGetConnector {
            connector_id: id,
            ..ModeGetConnector::default()
        },
    )
    .ok()?;
    if c.connection != DRM_MODE_CONNECTED {
        return None;
    }
    let mode_count = c.count_modes as usize;
    if mode_count == 0 {
        return None;
    }

    let mut modes = vec![ModeInfo::default(); mode_count];
    let req = ModeGetConnector {
        connector_id: id,
        count_modes: mode_count as u32,
        modes_ptr: modes.as_mut_ptr() as u64,
        ..ModeGetConnector::default()
    };
    let c = ioctl_rw::<ModeGetConnector, DRM_IOCTL_MODE_GETCONNECTOR, _>(fd, req).ok()?;

    let mode = pick_best_mode(&modes)?;

    Some(OutputInfo {
        name: connector_name(c.connector_type, c.connector_type_id),
        width: mode.hdisplay as i32,
        height: mode.vdisplay as i32,
        refresh_hz: mode.vrefresh as i32,
        scale: 1,
        x: 0,
        y: 0,
    })
}

/// Wybiera tryb monitora: **najpierw tryb natywny z EDID** (`PREFERRED`), a dopiero
/// potem największa rozdzielczość i najwyższy `vrefresh`.
///
/// Wcześniej kolejność była odwrotna („największa rozdzielczość, potem PREFERRED”),
/// więc monitor o natywnym 1600×900, który dodatkowo zgłasza 1920×1080, dostawał
/// 1920×1080 — obraz był skalowany przez monitor i wyglądał na rozmyty.
fn pick_best_mode(modes: &[ModeInfo]) -> Option<&ModeInfo> {
    modes.iter().max_by_key(|m| {
        let area = m.hdisplay as u64 * m.vdisplay as u64;
        let hz = m.vrefresh as u64;
        let preferred = u64::from(m.type_ & DRM_MODE_TYPE_PREFERRED != 0);
        (preferred, area, hz)
    })
}

/// Tryb zapisany przez użytkownika, a gdy go nie ma (albo monitor go nie zgłasza)
/// — automatyczny wybór [`pick_best_mode`].
fn pick_mode(modes: &[ModeInfo], wanted: Option<OutputMode>) -> Option<&ModeInfo> {
    if let Some(wanted) = wanted {
        let mut candidates = modes.iter().filter(|mode| {
            OutputMode::new(
                mode.hdisplay,
                mode.vdisplay,
                mode.vrefresh.min(u16::MAX as u32) as u16,
            )
            .matches(wanted)
        });
        let hit = if wanted.refresh == 0 {
            // Sama rozdzielczość — bierzemy najwyższe dostępne odświeżanie.
            candidates.max_by_key(|mode| mode.vrefresh)
        } else {
            candidates.next()
        };
        if hit.is_some() {
            return hit;
        }
    }
    pick_best_mode(modes)
}

/// Nazwa konektora w stylu Wayland (np. `HDMI-A-1`, `eDP-1`).
fn connector_name(kind: u32, index: u32) -> String {
    // Wartości `enum drm_mode_connector_type` z `drm_mode.h` — **nie przesuwać**.
    // Wcześniej mapa była przesunięta od 6 w górę: prawdziwy DP (10) nazywał się
    // „HDMI-A”, a HDMI-A (11) — „HDMI-B”, przez co nazwy nie zgadzały się z `/sys`
    // i Ustawienia widziały „trzeci” monitor (duplikat z innej listy).
    let base = match kind {
        1 => "VGA",
        2 => "DVI-I",
        3 => "DVI-D",
        4 => "DVI-A",
        5 => "Composite",
        6 => "SVIDEO",
        7 => "LVDS",
        8 => "Component",
        9 => "DIN",
        10 => "DP",
        11 => "HDMI-A",
        12 => "HDMI-B",
        13 => "TV",
        14 => "eDP",
        15 => "Virtual",
        16 => "DSI",
        17 => "DPI",
        18 => "Writeback",
        _ => "Connector",
    };
    format!("{base}-{index}")
}

// ————————————————————————————————————————————————————————————————————————
// Scanout: realne wyjście na monitory przez KMS (dumb buffer + page-flip)
// ————————————————————————————————————————————————————————————————————————

/// Zasoby karty: identyfikatory konektorów, CRTC i enkoderów.
struct Resources {
    connectors: Vec<u32>,
    crtcs: Vec<u32>,
    #[allow(dead_code)]
    encoders: Vec<u32>,
}

fn fetch_resources(fd: BorrowedFd) -> Option<Resources> {
    let res =
        ioctl_rw::<ModeCardRes, DRM_IOCTL_MODE_GETRESOURCES, _>(fd, ModeCardRes::default()).ok()?;
    let (cn, cc, ce) = (
        res.count_connectors as usize,
        res.count_crtcs as usize,
        res.count_encoders as usize,
    );
    let (mut connectors, mut crtcs, mut encoders) =
        (vec![0u32; cn], vec![0u32; cc], vec![0u32; ce]);
    let req = ModeCardRes {
        count_connectors: cn as u32,
        connector_id_ptr: connectors.as_mut_ptr() as u64,
        count_crtcs: cc as u32,
        crtc_id_ptr: crtcs.as_mut_ptr() as u64,
        count_encoders: ce as u32,
        encoder_id_ptr: encoders.as_mut_ptr() as u64,
        ..ModeCardRes::default()
    };
    ioctl_rw::<ModeCardRes, DRM_IOCTL_MODE_GETRESOURCES, _>(fd, req).ok()?;
    Some(Resources {
        connectors,
        crtcs,
        encoders,
    })
}

/// Bufor „dumb” (bez GPU): sterta pamięci zmapowana do renderowania i skanowania.
struct DumbBuffer {
    fb_id: u32,
    /// Uchwyt GEM (potrzebny m.in. do kursora sprzętowego).
    handle: u32,
    pitch: usize,
    size: usize,
    /// Rozmiar alokacji — kursor może zmienić rozmiar (motyw 24 px, klient 64 px),
    /// więc bufor odtwarzamy, zamiast pisać poza starszy, mniejszy zakres.
    width: u32,
    height: u32,
    ptr: *mut u8,
}

// Bezpieczne: bufor należy wyłącznie do wątku wyjścia i nie jest współdzielony.
unsafe impl Send for DumbBuffer {}

impl Drop for DumbBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                let _ = munmap(self.ptr.cast(), self.size);
            }
        }
    }
}

/// Jedno wyjście DRM: konektor + CRTC + podwójny bufor.
struct DrmOutput {
    name: String,
    /// Konektor, do którego przypięty jest CRTC (potrzebny przy zmianie trybu).
    connector_id: u32,
    crtc_id: u32,
    /// Rozmiar sprzętowej tablicy gamma (0 = brak wsparcia dla night lightu).
    gamma_size: u32,
    width: u32,
    height: u32,
    hz: u32,
    /// Wszystkie tryby zgłoszone przez monitor (bez duplikatów, malejąco).
    modes: Vec<OutputMode>,
    /// Surowa lista trybów z modesetu — `MODE_SETCRTC` wymaga pełnego `ModeInfo`.
    raw_modes: Vec<ModeInfo>,
    buffers: [DumbBuffer; 2],
    front: usize,
    /// Czy `present` musi przepisywać kanały (bufor `XRGB8888` zamiast `XBGR8888`).
    swizzle: bool,
    /// Bufor obrazu kursora sprzętowego (tworzony leniwie).
    cursor: Option<DumbBuffer>,
}

/// Powód, dla którego nie udało się otworzyć wyjścia DRM (do diagnostyki).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrmError {
    /// Inna sesja trzyma mastera DRM — nie odbieramy ekranu.
    MasterBusy,
    /// Nie udało się odczytać zasobów karty (`GETRESOURCES`).
    Resources,
    /// Nie udało się ustawić trybu na żadnym podłączonym monitorze.
    NoOutputs,
}

/// Backend wyjścia DRM/KMS (jedna karta, wiele monitorów).
pub struct DrmBackend {
    fd: OwnedFd,
    outputs: Vec<DrmOutput>,
}

impl Drop for DrmBackend {
    fn drop(&mut self) {
        // Zwolnij bycie masterem, gdy kończymy.
        let _ = ioctl_none::<DRM_IOCTL_DROP_MASTER, _>(&self.fd);
    }
}

impl DrmBackend {
    /// Otwiera kartę, przejmuje mastera i ustawia tryb na wszystkich podłączonych monitorach.
    /// Zwraca `None`, gdy DRM jest niedostępne lub ktoś inny trzyma mastera (np. sesja użytkownika).
    ///
    /// `preferences` pochodzi z `~/.config/luna/displays.conf` — zapisany tryb
    /// (rozdzielczość + Hz) wygrywa z automatycznym wyborem „natywny/NAJWiększy”.
    pub fn open(preferences: &DisplaySettings) -> Result<Self, DrmError> {
        let mut last = DrmError::NoOutputs;
        for card in card_paths() {
            let Ok(fd) = rustix::fs::open(&card, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            else {
                continue;
            };
            if ioctl_none::<DRM_IOCTL_SET_MASTER, _>(&fd).is_err() {
                last = DrmError::MasterBusy; // inna sesja trzyma mastera
                continue;
            }
            let Some(res) = fetch_resources(fd.as_fd()) else {
                let _ = ioctl_none::<DRM_IOCTL_DROP_MASTER, _>(&fd);
                last = DrmError::Resources;
                continue;
            };
            let mut outputs = Vec::new();
            let mut used_crtcs = Vec::new();
            let mut skipped = 0usize;
            for &conn in &res.connectors {
                if let Some(out) =
                    setup_output(fd.as_fd(), conn, &res, &mut used_crtcs, preferences)
                {
                    outputs.push(out);
                } else {
                    skipped += 1;
                }
            }
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/drm: skonfigurowano {} wyjść (pominięto {} z {} złączy)",
                    outputs.len(),
                    skipped,
                    res.connectors.len()
                );
            }
            if outputs.is_empty() {
                let _ = ioctl_none::<DRM_IOCTL_DROP_MASTER, _>(&fd);
                last = DrmError::NoOutputs;
                continue;
            }
            return Ok(Self { fd, outputs });
        }
        Err(last)
    }

    /// `(nazwa, szerokość, wysokość, odświeżanie)` każdego wyjścia.
    pub fn outputs(&self) -> Vec<(String, u32, u32, u32)> {
        self.outputs
            .iter()
            .map(|o| (o.name.clone(), o.width, o.height, o.hz))
            .collect()
    }

    /// Wyjścia wraz z pełną listą trybów (dla Ustawień i pliku `luna-monitors`).
    pub fn outputs_with_modes(&self) -> Vec<(String, u32, u32, u32, Vec<OutputMode>)> {
        self.outputs
            .iter()
            .map(|o| (o.name.clone(), o.width, o.height, o.hz, o.modes.clone()))
            .collect()
    }

    /// Czy którykolwiek CRTC ma sprzętową tablicę gamma (night light).
    pub fn supports_night_light(&self) -> bool {
        self.outputs.iter().any(|o| o.gamma_size > 0)
    }

    /// Zmienia tryb wyjścia **na żywo** (bez restartu sesji).
    ///
    /// Tworzy nowe bufory i framebuffery, przepina CRTC i dopiero po sukcesie
    /// zwalnia stare framebuffery; stare bufory znikają przy przypisaniu nowych.
    /// Zwraca indeks wyjścia przy sukcesie, `None` gdy monitor nie istnieje,
    /// nie zgłasza takiego trybu albo jądro odrzuciło modeset.
    pub fn set_mode(&mut self, name: &str, wanted: Option<OutputMode>) -> Option<usize> {
        let index = self.outputs.iter().position(|out| out.name == name)?;
        let fd = self.fd.as_fd();
        let (connector_id, crtc_id, current, mode) = {
            let out = &self.outputs[index];
            let mode = *pick_mode(&out.raw_modes, wanted)?;
            (
                out.connector_id,
                out.crtc_id,
                (out.width, out.height, out.hz),
                mode,
            )
        };
        if current == (mode.hdisplay as u32, mode.vdisplay as u32, mode.vrefresh) {
            return Some(index);
        }

        let width = mode.hdisplay as u32;
        let height = mode.vdisplay as u32;
        let mut swizzle = false;
        let make_pair = |format: u32| -> Option<(DumbBuffer, DumbBuffer)> {
            Some((
                create_dumb(fd, width, height, format)?,
                create_dumb(fd, width, height, format)?,
            ))
        };
        let (b0, b1) = match make_pair(DRM_FORMAT_XBGR8888) {
            Some(pair) => pair,
            None => {
                swizzle = true;
                make_pair(DRM_FORMAT_XRGB8888)?
            }
        };
        if !set_crtc(fd, crtc_id, b0.fb_id, connector_id, &mode) {
            destroy_fb(fd, b0.fb_id);
            destroy_fb(fd, b1.fb_id);
            return None;
        }

        let out = &mut self.outputs[index];
        for old in &out.buffers {
            destroy_fb(fd, old.fb_id);
        }
        out.width = width;
        out.height = height;
        out.hz = mode.vrefresh;
        out.swizzle = swizzle;
        out.front = 0;
        out.buffers = [b0, b1];
        // Bufor kursora zostanie utworzony na nowo przy następnym ruchu.
        out.cursor = None;
        Some(index)
    }

    /// Ustawia ciepłą korektę gamma na wszystkich wyjściach.
    ///
    /// `percent` = 0 przywraca neutralną rampę, 100 to najcieplejszy wariant.
    /// Zwraca `true`, jeśli udało się ustawić LUT na przynajmniej jednym CRTC.
    pub fn set_night_light(&self, percent: u8) -> bool {
        let strength = percent.min(100) as f64 / 100.0;
        let mut any = false;
        for out in &self.outputs {
            if out.gamma_size == 0 {
                continue;
            }
            if set_gamma_lut(self.fd.as_fd(), out.crtc_id, out.gamma_size, strength) {
                any = true;
            }
        }
        any
    }

    /// Wypycha klatkę (RGBA, wymiary = rozmiar wyjścia) i kolejkuje page-flip.
    /// Po wywołaniu należy poczekać na [`DrmBackend::wait_flip`].
    pub fn present(&mut self, idx: usize, rgba: &[u8]) -> bool {
        let fd = &self.fd;
        let Some(out) = self.outputs.get_mut(idx) else {
            return false;
        };
        let back = (out.front + 1) & 1;
        if out.swizzle {
            swizzle_into(
                out.buffers[back].ptr,
                out.buffers[back].pitch,
                rgba,
                out.width as usize,
                out.height as usize,
            );
        } else {
            copy_rows_into(
                out.buffers[back].ptr,
                out.buffers[back].pitch,
                rgba,
                out.width as usize,
                out.height as usize,
            );
        }
        let req = ModePageFlip {
            crtc_id: out.crtc_id,
            fb_id: out.buffers[back].fb_id,
            flags: DRM_MODE_PAGE_FLIP_EVENT,
            reserved: 0,
            user_data: idx as u64,
        };
        if ioctl_rw::<ModePageFlip, DRM_IOCTL_MODE_PAGE_FLIP, _>(fd, req).is_err() {
            return false;
        }
        out.front = back;
        true
    }

    /// Czeka na zakończenie page-flipu (zdarzenie z jądra).
    pub fn wait_flip(&self) {
        let mut buf = [0u8; 32];
        loop {
            match rustix::io::read(&self.fd, &mut buf) {
                Ok(n) if n >= 8 => {
                    let ty = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    if ty == DRM_EVENT_FLIP_COMPLETE {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// Ustawia obraz kursora sprzętowego na wyjściu `idx` w pozycji `(x, y)`.
    ///
    /// `false`, gdy sterownik nie obsługuje kursora — wywołujący wraca do kursora
    /// programowego (rysowanego w scenie).
    pub fn set_cursor_image(
        &mut self,
        idx: usize,
        rgba: &[u8],
        w: u32,
        h: u32,
        x: i32,
        y: i32,
    ) -> bool {
        let fd = self.fd.as_fd();
        let Some(out) = self.outputs.get_mut(idx) else {
            return false;
        };
        // Rozmiar kursora może się zmienić (motyw 24 px → obraz klienta 64 px).
        // Stary bufor ma inny `pitch`/rozmiar, więc bez odtworzenia pisalibyśmy
        // poza jego zakres.
        if let Some(old) = out.cursor.as_ref() {
            if old.width != w || old.height != h {
                destroy_dumb(fd, old.handle);
                out.cursor = None;
            }
        }
        if out.cursor.is_none() {
            match create_dumb_fmt(fd, w, h, DRM_FORMAT_ARGB8888, false) {
                Some(b) => out.cursor = Some(b),
                None => return false,
            }
        }
        let Some(cur) = out.cursor.as_mut() else {
            return false;
        };
        swizzle_argb_into(cur.ptr, cur.pitch, rgba, w as usize, h as usize);
        let req = ModeCursor {
            flags: DRM_MODE_CURSOR_BO,
            crtc_id: out.crtc_id,
            x,
            y,
            width: w,
            height: h,
            handle: cur.handle,
        };
        ioctl_rw::<ModeCursor, DRM_IOCTL_MODE_CURSOR, _>(&self.fd, req).is_ok()
    }

    /// Przesuwa kursor sprzętowy na wyjściu `idx` (bez zmiany obrazu).
    pub fn move_cursor(&mut self, idx: usize, x: i32, y: i32) -> bool {
        let Some(crtc_id) = self.outputs.get(idx).map(|o| o.crtc_id) else {
            return false;
        };
        let req = ModeCursor {
            flags: DRM_MODE_CURSOR_MOVE,
            crtc_id,
            x,
            y,
            ..ModeCursor::default()
        };
        ioctl_rw::<ModeCursor, DRM_IOCTL_MODE_CURSOR, _>(&self.fd, req).is_ok()
    }

    /// Wyłącza kursor sprzętowy na wyjściu `idx`.
    pub fn hide_cursor(&mut self, idx: usize) -> bool {
        let Some(crtc_id) = self.outputs.get(idx).map(|o| o.crtc_id) else {
            return false;
        };
        let req = ModeCursor {
            flags: DRM_MODE_CURSOR_BO,
            crtc_id,
            ..ModeCursor::default()
        };
        ioctl_rw::<ModeCursor, DRM_IOCTL_MODE_CURSOR, _>(&self.fd, req).is_ok()
    }
}

/// Konfiguruje jedno wyjście: tryb, bufor dumb, framebuffer i `SETCRTC`.
fn setup_output(
    fd: BorrowedFd,
    conn_id: u32,
    res: &Resources,
    used_crtcs: &mut Vec<u32>,
    preferences: &DisplaySettings,
) -> Option<DrmOutput> {
    let c = ioctl_rw::<ModeGetConnector, DRM_IOCTL_MODE_GETCONNECTOR, _>(
        fd,
        ModeGetConnector {
            connector_id: conn_id,
            ..ModeGetConnector::default()
        },
    )
    .ok()?;
    if c.connection != DRM_MODE_CONNECTED {
        return None;
    }
    let count = c.count_modes as usize;
    if count == 0 {
        return None;
    }
    let mut modes = vec![ModeInfo::default(); count];
    let c = ioctl_rw::<ModeGetConnector, DRM_IOCTL_MODE_GETCONNECTOR, _>(
        fd,
        ModeGetConnector {
            connector_id: conn_id,
            count_modes: count as u32,
            modes_ptr: modes.as_mut_ptr() as u64,
            ..ModeGetConnector::default()
        },
    )
    .ok()?;
    let name = connector_name(c.connector_type, c.connector_type_id);
    let wanted = preferences.mode_for(&name);
    let mode = *pick_mode(&modes, wanted)?;
    let mut offered: Vec<OutputMode> = modes
        .iter()
        .filter(|mode| mode.hdisplay > 0 && mode.vdisplay > 0)
        .map(|mode| {
            OutputMode::new(
                mode.hdisplay,
                mode.vdisplay,
                mode.vrefresh.min(u16::MAX as u32) as u16,
            )
        })
        .collect();
    offered.sort_by(|a, b| {
        b.width
            .cmp(&a.width)
            .then_with(|| b.height.cmp(&a.height))
            .then_with(|| b.refresh.cmp(&a.refresh))
    });
    offered.dedup();

    // Enkoder → CRTC. Po przejęciu ekranu CRTC po poprzedniej sesji bywa jeszcze
    // zajęty (`set_crtc` kończy się `-EBUSY`) — dlatego próbujemy **kolejnych**
    // kandydatów. Wcześniej poddawaliśmy się po pierwszej nieudanej próbie i losowo
    // brakowało jednego monitora („działa tylko jeden ekran”).
    let enc = ioctl_rw::<ModeGetEncoder, DRM_IOCTL_MODE_GETENCODER, _>(
        fd,
        ModeGetEncoder {
            encoder_id: c.encoder_id,
            ..ModeGetEncoder::default()
        },
    )
    .ok()?;

    // Kolejność kandydatów: obecny CRTC enkodera → wg maski `possible_crtcs` → reszta karty.
    let mut candidates: Vec<u32> = Vec::new();
    if enc.crtc_id != 0 {
        candidates.push(enc.crtc_id);
    }
    for (idx, &crtc) in res.crtcs.iter().enumerate() {
        if (enc.possible_crtcs & (1 << idx)) != 0 {
            candidates.push(crtc);
        }
    }
    candidates.extend(res.crtcs.iter().copied());

    let width = mode.hdisplay as u32;
    let height = mode.vdisplay as u32;
    // Preferujemy `XBGR8888`: pamięć to R,G,B,X, czyli dokładnie wyjście tiny-skia.
    // `present` kopiuje wtedy wiersze bez przepisywania kanałów (oszczędność ~1–2 ms
    // na klatkę 1080p). Gdy sterownik nie wspiera formatu — wracamy do `XRGB8888`.
    let mut swizzle = false;
    let make_pair = |fmt: u32| -> Option<(DumbBuffer, DumbBuffer)> {
        Some((
            create_dumb(fd, width, height, fmt)?,
            create_dumb(fd, width, height, fmt)?,
        ))
    };
    let (b0, b1) = match make_pair(DRM_FORMAT_XBGR8888) {
        Some(pair) => pair,
        None => {
            swizzle = true;
            make_pair(DRM_FORMAT_XRGB8888)?
        }
    };

    let mut crtc_id = None;
    for cand in candidates {
        if used_crtcs.contains(&cand) {
            continue;
        }
        if set_crtc(fd, cand, b0.fb_id, conn_id, &mode) {
            crtc_id = Some(cand);
            break;
        }
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!("selene/drm: CRTC {cand} zajęty — próbuję następny");
        }
    }
    let crtc_id = crtc_id?;
    // CRTC oznaczamy jako użyty **dopiero po udanym** `set_crtc`.
    used_crtcs.push(crtc_id);
    Some(DrmOutput {
        name,
        connector_id: conn_id,
        crtc_id,
        gamma_size: gamma_size(fd, crtc_id),
        width,
        height,
        hz: mode.vrefresh,
        modes: offered,
        raw_modes: modes,
        buffers: [b0, b1],
        front: 0,
        swizzle,
        cursor: None,
    })
}

/// Usuwa framebuffer (po udanym modesecie na nowy obraz).
fn destroy_fb(fd: BorrowedFd, fb_id: u32) {
    let _ = ioctl_rw::<u32, DRM_IOCTL_MODE_RMFB, _>(fd, fb_id);
}

/// Tworzy bufor dumb, mapuje go i rejestruje jako framebuffer (scanout).
fn create_dumb(fd: BorrowedFd, width: u32, height: u32, format: u32) -> Option<DumbBuffer> {
    create_dumb_fmt(fd, width, height, format, true)
}

/// Tworzy bufor dumb w zadanym formacie; `register_fb` dodaje też framebuffer
/// (niepotrzebne dla kursora, gdzie liczy się tylko uchwyt GEM).
fn create_dumb_fmt(
    fd: BorrowedFd,
    width: u32,
    height: u32,
    format: u32,
    register_fb: bool,
) -> Option<DumbBuffer> {
    let dumb = ioctl_rw::<ModeCreateDumb, DRM_IOCTL_MODE_CREATE_DUMB, _>(
        fd,
        ModeCreateDumb {
            width,
            height,
            bpp: 32,
            ..ModeCreateDumb::default()
        },
    )
    .ok()?;
    let map = ioctl_rw::<ModeMapDumb, DRM_IOCTL_MODE_MAP_DUMB, _>(
        fd,
        ModeMapDumb {
            handle: dumb.handle,
            ..ModeMapDumb::default()
        },
    )
    .ok()?;
    let ptr = unsafe {
        mmap(
            core::ptr::null_mut(),
            dumb.size as usize,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            fd,
            map.offset,
        )
    }
    .ok()? as *mut u8;
    let fb_id = if register_fb {
        ioctl_rw::<ModeFbCmd2, DRM_IOCTL_MODE_ADDFB2, _>(
            fd,
            ModeFbCmd2 {
                width,
                height,
                pixel_format: format,
                handles: [dumb.handle, 0, 0, 0],
                pitches: [dumb.pitch, 0, 0, 0],
                ..ModeFbCmd2::default()
            },
        )
        .ok()?
        .fb_id
    } else {
        0
    };
    Some(DumbBuffer {
        fb_id,
        handle: dumb.handle,
        pitch: dumb.pitch as usize,
        size: dumb.size as usize,
        width,
        height,
        ptr,
    })
}

/// Zwalnia uchwyt GEM (`destroy_dumb`) — inaczej każdy nowy rozmiar kursora
/// zostawiałby w jądrze martwy bufor.
fn destroy_dumb(fd: BorrowedFd, handle: u32) {
    let _ =
        ioctl_rw::<ModeDestroyDumb, DRM_IOCTL_MODE_DESTROY_DUMB, _>(fd, ModeDestroyDumb { handle });
}

/// Odczytuje rozmiar sprzętowej tablicy gamma CRTC (0 = brak wsparcia).
fn gamma_size(fd: BorrowedFd, crtc_id: u32) -> u32 {
    ioctl_rw::<ModeCrtc, DRM_IOCTL_MODE_GETCRTC, _>(
        fd,
        ModeCrtc {
            crtc_id,
            ..ModeCrtc::default()
        },
    )
    .map(|c| c.gamma_size)
    .unwrap_or(0)
}

/// Buduje neutralną lub ciepłą rampę gamma.
///
/// `strength` = 0 daje idealną przekątną (brak zmiany kolorów), a 1 najmocniej
/// tłumi kanał niebieski. Wartości mają pełny zakres 16 bitów na kanał.
fn gamma_ramp(size: u32, strength: f64) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let n = size as usize;
    let mut red = vec![0u16; n];
    let mut green = vec![0u16; n];
    let mut blue = vec![0u16; n];
    if n == 0 {
        return (red, green, blue);
    }
    let strength = strength.clamp(0.0, 1.0);
    let last = (n - 1).max(1) as f64;
    for i in 0..n {
        let level = i as f64 / last * 65535.0;
        red[i] = level.round() as u16;
        green[i] = (level * (1.0 - 0.10 * strength)).round() as u16;
        blue[i] = (level * (1.0 - 0.38 * strength)).round() as u16;
    }
    (red, green, blue)
}

/// Wgrywa tablicę gamma na CRTC (`SETGAMMA`).
fn set_gamma_lut(fd: BorrowedFd, crtc_id: u32, size: u32, strength: f64) -> bool {
    if size == 0 {
        return false;
    }
    let (red, green, blue) = gamma_ramp(size, strength);
    let lut = ModeCrtcLut {
        crtc_id,
        gamma_size: size,
        red: red.as_ptr() as u64,
        green: green.as_ptr() as u64,
        blue: blue.as_ptr() as u64,
    };
    ioctl_rw::<ModeCrtcLut, DRM_IOCTL_MODE_SETGAMMA, _>(fd, lut).is_ok()
}

/// Ustawia tryb na CRTC (`SETCRTC`).
fn set_crtc(fd: BorrowedFd, crtc_id: u32, fb_id: u32, connector_id: u32, mode: &ModeInfo) -> bool {
    let mut connectors = [connector_id];
    let req = ModeCrtc {
        set_connectors_ptr: connectors.as_mut_ptr() as u64,
        count_connectors: 1,
        crtc_id,
        fb_id,
        mode_valid: 1,
        mode: *mode,
        ..ModeCrtc::default()
    };
    ioctl_rw::<ModeCrtc, DRM_IOCTL_MODE_SETCRTC, _>(fd, req).is_ok()
}

/// Przepisuje RGBA do bufora kursora w formacie `ARGB8888` (kolejność B,G,R,A).
fn swizzle_argb_into(dst: *mut u8, pitch: usize, rgba: &[u8], width: usize, height: usize) {
    if dst.is_null() || rgba.len() < width * height * 4 {
        return;
    }
    unsafe {
        for y in 0..height {
            let src = &rgba[y * width * 4..(y + 1) * width * 4];
            let row = dst.add(y * pitch);
            for x in 0..width {
                let s = &src[x * 4..x * 4 + 4];
                let d = row.add(x * 4);
                *d = s[2];
                *d.add(1) = s[1];
                *d.add(2) = s[0];
                *d.add(3) = s[3];
            }
        }
    }
}

/// Przepisuje klatkę RGBA do bufora w formacie `XRGB8888` (kolejność B,G,R,X).
fn swizzle_into(dst: *mut u8, pitch: usize, rgba: &[u8], width: usize, height: usize) {
    if dst.is_null() || rgba.len() < width * height * 4 {
        return;
    }
    unsafe {
        for y in 0..height {
            let src = &rgba[y * width * 4..(y + 1) * width * 4];
            let row_dst = std::slice::from_raw_parts_mut(dst.add(y * pitch) as *mut u32, width);
            for (d, s) in row_dst.iter_mut().zip(src.chunks_exact(4)) {
                *d = u32::from_ne_bytes([s[2], s[1], s[0], 0xff]);
            }
        }
    }
}

/// Kopiuje klatkę RGBA do bufora `XBGR8888` (ta sama kolejność bajtów w pamięci)
/// wiersz po wierszu. Gdy `pitch` zgadza się z szerokością, to jeden `memcpy`.
fn copy_rows_into(dst: *mut u8, pitch: usize, rgba: &[u8], width: usize, height: usize) {
    if dst.is_null() || rgba.len() < width * height * 4 {
        return;
    }
    let row_bytes = width * 4;
    unsafe {
        if pitch == row_bytes {
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), dst, row_bytes * height);
        } else {
            for y in 0..height {
                std::ptr::copy_nonoverlapping(
                    rgba.as_ptr().add(y * row_bytes),
                    dst.add(y * pitch),
                    row_bytes,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_names_follow_wayland_convention() {
        assert_eq!(connector_name(14, 1), "eDP-1");
        assert_eq!(connector_name(11, 2), "HDMI-A-2");
        assert_eq!(connector_name(12, 1), "HDMI-B-1");
        assert_eq!(connector_name(10, 1), "DP-1");
        assert_eq!(connector_name(999, 0), "Connector-0");
    }

    #[test]
    fn ioctl_struct_sizes_match_kernel_padding() {
        // Rozmiary z nagłówków DRM — sprawdzają poprawność `#[repr(C)]` na 64-bit.
        assert_eq!(core::mem::size_of::<ModeCardRes>(), 64);
        assert_eq!(core::mem::size_of::<ModeGetConnector>(), 80);
        assert_eq!(core::mem::size_of::<ModeInfo>(), 68);
        assert_eq!(core::mem::size_of::<ModeGetEncoder>(), 20);
        assert_eq!(core::mem::size_of::<ModeCrtc>(), 104);
        assert_eq!(core::mem::size_of::<ModeCreateDumb>(), 32);
        assert_eq!(core::mem::size_of::<ModeMapDumb>(), 16);
        assert_eq!(core::mem::size_of::<ModeFbCmd2>(), 104);
        assert_eq!(core::mem::size_of::<ModePageFlip>(), 24);
        assert_eq!(core::mem::size_of::<ModeCursor>(), 28);
    }

    #[test]
    #[ignore = "benchmark reczny"]
    fn bench_present_copy() {
        let (w, h) = (1920usize, 1080usize);
        let src = vec![7u8; w * h * 4];
        let mut dst = vec![0u8; w * h * 4];
        let n = 20;
        let t = std::time::Instant::now();
        for _ in 0..n {
            copy_rows_into(
                std::hint::black_box(dst.as_mut_ptr()),
                w * 4,
                std::hint::black_box(&src),
                w,
                h,
            );
        }
        let _ = std::hint::black_box(dst[12345]);
        println!(
            "copy_rows_into (XBGR8888): {:.2} ms/klatke",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
        let t = std::time::Instant::now();
        for _ in 0..n {
            swizzle_into(
                std::hint::black_box(dst.as_mut_ptr()),
                w * 4,
                std::hint::black_box(&src),
                w,
                h,
            );
        }
        let _ = std::hint::black_box(dst[12345]);
        println!(
            "swizzle_into (XRGB8888):  {:.2} ms/klatke",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }

    #[test]
    fn xrgb8888_fourcc_is_little_endian_bgrx() {
        // 'X','R','2','4' — kontroluje kolejność bajtów przy przepisywaniu klatki.
        assert_eq!(DRM_FORMAT_XRGB8888.to_le_bytes(), *b"XR24");
        // 'A','R','2','4' — format kursora (z kanałem alfa).
        assert_eq!(DRM_FORMAT_ARGB8888.to_le_bytes(), *b"AR24");
        // 'X','B','2','4' — w pamięci R,G,B,X, czyli wprost wyjście tiny-skia.
        assert_eq!(DRM_FORMAT_XBGR8888.to_le_bytes(), *b"XB24");
    }

    #[test]
    fn copy_rows_matches_rgba_layout() {
        // 2×2 RGBA → bufor XBGR8888: bajty bez zmian, tylko wiersze wg `pitch`.
        let src: Vec<u8> = (0..16).collect();
        let mut dst = vec![0u8; 16];
        copy_rows_into(dst.as_mut_ptr(), 8, &src, 2, 2);
        assert_eq!(dst, src);

        // Większy `pitch` (padding w wierszu) — kopiowane tylko 8 bajtów na wiersz.
        let mut dst = vec![0xAAu8; 24];
        copy_rows_into(dst.as_mut_ptr(), 12, &src, 2, 2);
        assert_eq!(&dst[0..8], &src[0..8]);
        assert_eq!(&dst[12..20], &src[8..16]);
        // Padding pozostaje nietknięty.
        assert!(dst[8..12].iter().all(|&b| b == 0xAA));
    }

    fn mode(w: u16, h: u16, hz: u32, preferred: bool) -> ModeInfo {
        ModeInfo {
            hdisplay: w,
            vdisplay: h,
            vrefresh: hz,
            type_: if preferred {
                DRM_MODE_TYPE_PREFERRED
            } else {
                0
            },
            ..ModeInfo::default()
        }
    }

    #[test]
    fn picks_highest_resolution_then_refresh() {
        let modes = [
            mode(1280, 720, 60, false),
            mode(1920, 1080, 60, false),
            mode(1920, 1080, 144, false),
        ];
        let best = pick_best_mode(&modes).unwrap();
        assert_eq!(
            (best.hdisplay, best.vdisplay, best.vrefresh),
            (1920, 1080, 144)
        );
    }

    #[test]
    fn configured_mode_wins_over_the_best_one() {
        let modes = [
            mode(1280, 720, 60, false),
            mode(1920, 1080, 60, false),
            mode(1920, 1080, 144, false),
        ];
        let wanted = OutputMode::new(1280, 720, 60);
        let picked = pick_mode(&modes, Some(wanted)).unwrap();
        assert_eq!(
            (picked.hdisplay, picked.vdisplay, picked.vrefresh),
            (1280, 720, 60)
        );
    }

    #[test]
    fn missing_or_refresh_less_mode_falls_back_to_auto() {
        let modes = [
            mode(1280, 720, 60, false),
            mode(1920, 1080, 60, false),
            mode(1920, 1080, 144, false),
        ];
        // Zapisana rozdzielczość bez Hz — wygrywa najwyższe odświeżanie.
        let picked = pick_mode(&modes, Some(OutputMode::new(1920, 1080, 0))).unwrap();
        assert_eq!(picked.vrefresh, 144);

        // Monitor nie zgłasza zapisanego trybu — wracamy do automatycznego wyboru.
        let picked = pick_mode(&modes, Some(OutputMode::new(3840, 2160, 60))).unwrap();
        assert_eq!((picked.hdisplay, picked.vdisplay), (1920, 1080));
    }

    #[test]
    fn native_mode_wins_over_larger_scale() {
        // Monitor o natywnym 1600x900, który dodatkowo zgłasza 1920x1080 — bierzemy natywny.
        let modes = [
            mode(1920, 1080, 60, false),
            mode(1600, 900, 60, true),
            mode(1280, 720, 60, false),
        ];
        let best = pick_best_mode(&modes).unwrap();
        assert_eq!((best.hdisplay, best.vdisplay), (1600, 900));
    }

    #[test]
    fn fallback_ignores_missing_preferred_flag() {
        // Gdy EDID nie oznacza żadnego trybu jako natywny — bierzemy największy.
        let modes = [mode(1280, 720, 60, false), mode(1920, 1080, 60, false)];
        assert_eq!(pick_best_mode(&modes).unwrap().hdisplay, 1920);
    }

    #[test]
    fn preferred_mode_breaks_ties() {
        let modes = [mode(1920, 1080, 60, false), mode(1920, 1080, 60, true)];
        assert!(pick_best_mode(&modes).unwrap().type_ & DRM_MODE_TYPE_PREFERRED != 0);
    }

    #[test]
    fn drm_gamma_structs_match_kernel_abi() {
        assert_eq!(std::mem::size_of::<ModeCrtcLut>(), 32);
        assert_eq!(std::mem::size_of::<ModeCrtc>(), 104);
        assert_eq!(DRM_IOCTL_MODE_GETCRTC, 0xC068_64A1);
        assert_eq!(DRM_IOCTL_MODE_SETGAMMA, 0xC020_64A4);
    }

    #[test]
    fn neutral_gamma_is_identity() {
        let (r, g, b) = gamma_ramp(256, 0.0);
        assert_eq!(r, g);
        assert_eq!(g, b);
        assert_eq!(r[0], 0);
        assert_eq!(r[255], 65535);
    }

    #[test]
    fn warm_gamma_reduces_blue_most() {
        let (r, g, b) = gamma_ramp(256, 0.6);
        assert_eq!(r[0], 0);
        assert_eq!(r[255], 65535);
        assert!(b[128] < g[128], "niebieski powinien być najciemniejszy");
        assert!(
            g[128] <= r[128],
            "zielony nie powinien przebijać czerwonego"
        );
        // Biały punkt staje się cieplejszy — niebieski nie dobija do pełnej skali.
        assert!(b[255] < 65535);
    }
}
