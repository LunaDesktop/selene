//! `zwp_linux_dmabuf_v1` — bufory GPU klientów bez kopiowania przez CPU.
//!
//! Ogłaszamy ten protokół **tylko**, gdy kompozytor rysuje przez GPU
//! (`LUNA_GPU=1`, ustawiane też przez `SELENE_GPU=1`) i EGL potrafi zaimportować
//! `dmabuf` (`libluna_render::gpu::dmabuf_formats`). Bez GPU klienci zostają
//! przy `wl_shm` — nie ma sensu obiecywać czegoś, czego nie da się wyświetlić.
//!
//! Zakres etapu 2 (`docs/gpu.md`):
//! - wersja 3 protokołu: `format` + `modifier`, więc bez `feedback` z v4,
//! - formaty **jednoplanowe** `ARGB/XRGB/ABGR/XBGR8888` (pulpit, aplikacje),
//!   z modyfikatorami z EGL (tiling/compression),
//! - `create_immed` (ścieżka większości klientów: GTK, wgpu/Zed, Firefox)
//!   oraz `create` przez `created` (starsze klienty),
//! - import wykonuje `GpuRenderer` leniwie, po `Arc<DmabufImage>`.
//!
//! Bufor jest zwalniany klientowi (`wl_buffer.release`) **przy następnym commicie**
//! tej powierzchni — GPU ma wtedy pewność, że poprzednia klatka została odczytana.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use libluna_render::gpu;
use libluna_render::{DmabufImage, DmabufPlane};
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_server::{
    protocol::{wl_buffer, wl_shm},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, WEnum,
};

use crate::shm::BufferData;
use crate::state::State;

/// Formaty `DRM_FORMAT_*` obsługiwane w etapie 2 (jednoplanowe, 32-bit RGBA).
const SINGLE_PLANE_FORMATS: [u32; 4] = [
    0x3432_5241, // DRM_FORMAT_ARGB8888
    0x3432_5258, // DRM_FORMAT_XRGB8888
    0x3432_4241, // DRM_FORMAT_ABGR8888
    0x3432_4258, // DRM_FORMAT_XBGR8888
];

/// Maksymalna liczba planów w protokole.
const MAX_PLANES: usize = 4;

/// `DRM_FORMAT_MOD_INVALID` — brak jawnego modyfikatora.
const MOD_INVALID: u64 = u64::MAX;

/// Czy global `zwp_linux_dmabuf_v1` został zarejestrowany (diagnostyka/testy).
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Liczba formatów i par format/modyfikator w ogłoszeniu.
pub fn counts() -> (usize, usize) {
    (
        supported().len(),
        supported().iter().map(|(_, m)| m.len()).sum(),
    )
}

/// Format + obsługiwane modyfikatory (raz na proces).
fn supported() -> &'static [(u32, Vec<u64>)] {
    static CACHE: OnceLock<Vec<(u32, Vec<u64>)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let all = gpu::dmabuf_formats();
        let mut out = Vec::new();
        for format in SINGLE_PLANE_FORMATS {
            let Some((_, mods)) = all.iter().find(|(f, _)| *f == format) else {
                continue;
            };
            let mut mods = mods.clone();
            if mods.is_empty() {
                mods.push(MOD_INVALID);
            }
            out.push((format, mods));
        }
        out
    })
}

/// Czy jest sens ogłaszać `zwp_linux_dmabuf_v1`.
pub fn available() -> bool {
    !supported().is_empty()
}

/// Jeden plan dodany przez klienta.
struct Plane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
    modifier: u64,
}

#[derive(Default)]
struct ParamsInner {
    planes: Vec<Option<Plane>>,
    used: bool,
}

/// Dane `zwp_linux_buffer_params_v1`.
#[derive(Default)]
pub struct Params {
    inner: Mutex<ParamsInner>,
}

impl Params {
    fn is_used(&self) -> bool {
        self.inner.lock().map(|p| p.used).unwrap_or(true)
    }

    fn add(&self, plane_idx: u32, plane: Plane) {
        let Ok(mut p) = self.inner.lock() else {
            return;
        };
        let idx = plane_idx as usize;
        if p.used || idx >= MAX_PLANES {
            return;
        }
        if p.planes.len() <= idx {
            p.planes.resize_with(idx + 1, || None);
        }
        if p.planes[idx].is_some() {
            return;
        }
        p.planes[idx] = Some(plane);
    }

    /// Buduje obraz z parametrów (i oznacza obiekt jako użyty).
    fn build(&self, width: i32, height: i32, format: u32, flags: u32) -> Option<DmabufImage> {
        let mut p = self.inner.lock().ok()?;
        if p.used || width <= 0 || height <= 0 || width > 16384 || height > 16384 {
            return None;
        }
        // Etap 2: bez `y_invert`/`interlaced` — klient dostanie `failed`.
        if flags != 0 {
            return None;
        }
        let Some((_, mods)) = supported().iter().find(|(f, _)| *f == format) else {
            return None;
        };
        let plane = p.planes.first_mut()?.take()?;
        if plane.stride == 0 || !(mods.contains(&plane.modifier) || plane.modifier == MOD_INVALID) {
            return None;
        }
        p.used = true;
        Some(DmabufImage {
            width: width as u32,
            height: height as u32,
            format,
            modifier: plane.modifier,
            planes: vec![DmabufPlane {
                fd: plane.fd,
                offset: plane.offset,
                stride: plane.stride,
            }],
        })
    }
}

impl GlobalDispatch<ZwpLinuxDmabufV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpLinuxDmabufV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let dmabuf = data_init.init(resource, ());
        // v3: wysyłamy oba zdarzenia (`format` jest deprecated od v4, ale przy v3
        // klienty starsze go oczekują; nowsze i tak używają `modifier`).
        for (format, modifiers) in supported() {
            let _ = dmabuf.format(*format);
            for m in modifiers {
                let _ = dmabuf.modifier(*format, (m >> 32) as u32, *m as u32);
            }
        }
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!(
                "selene/dmabuf: bind {} formatów, {} par format/modyfikator",
                supported().len(),
                supported().iter().map(|(_, m)| m.len()).sum::<usize>()
            );
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpLinuxDmabufV1,
        request: zwp_linux_dmabuf_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // v3: `create_params`; `destroy`, `get_default_feedback` itd. obsługuje domyślnie.
        if let zwp_linux_dmabuf_v1::Request::CreateParams { params_id } = request {
            data_init.init(params_id, Params::default());
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, Params> for State {
    fn request(
        _state: &mut Self,
        client: &Client,
        resource: &ZwpLinuxBufferParamsV1,
        request: zwp_linux_buffer_params_v1::Request,
        data: &Params,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_buffer_params_v1::Request::Add {
                fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                data.add(
                    plane_idx,
                    Plane {
                        fd,
                        offset,
                        stride,
                        modifier: (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
                    },
                );
            }
            zwp_linux_buffer_params_v1::Request::Create {
                width,
                height,
                format,
                flags,
            } => {
                let flags = match flags {
                    WEnum::Value(f) => f.bits(),
                    WEnum::Unknown(v) => v,
                };
                match data.build(width, height, format, flags) {
                    Some(image) => {
                        let bd = BufferData::dmabuf(
                            std::sync::Arc::new(image),
                            wl_shm::Format::Argb8888,
                        );
                        match client.create_resource::<wl_buffer::WlBuffer, BufferData, State>(
                            handle, 1, bd,
                        ) {
                            Ok(buffer) => {
                                let _ = resource.created(&buffer);
                            }
                            Err(_) => {
                                let _ = resource.failed();
                            }
                        }
                    }
                    None => {
                        let _ = resource.failed();
                    }
                }
            }
            zwp_linux_buffer_params_v1::Request::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                flags,
            } => {
                let flags = match flags {
                    WEnum::Value(f) => f.bits(),
                    WEnum::Unknown(v) => v,
                };
                match data.build(width, height, format, flags) {
                    Some(image) => {
                        data_init.init(
                            buffer_id,
                            BufferData::dmabuf(
                                std::sync::Arc::new(image),
                                wl_shm::Format::Argb8888,
                            ),
                        );
                    }
                    None => {
                        // Protokół pozwala utworzyć „zepsuty” bufor i wysłać `failed`.
                        let _ = resource.failed();
                        data_init.init(buffer_id, BufferData::failed(width.max(1), height.max(1)));
                    }
                }
            }
            zwp_linux_buffer_params_v1::Request::Destroy => {}
            _ => {}
        }
        if std::env::var("SELENE_STATS").is_ok() && data.is_used() {
            // Diagnostyka: kolejne żądanie po `create` powinno być tylko `destroy`.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(format: u32, stride: u32, modifier: u64) -> Params {
        let p = Params::default();
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        p.add(
            0,
            Plane {
                fd,
                offset: 0,
                stride,
                modifier,
            },
        );
        p
    }

    #[test]
    fn advertises_at_least_one_of_our_formats_when_gpu_can_import() {
        if gpu::dmabuf_formats().is_empty() {
            eprintln!("pominięto: brak importu dmabuf");
            return;
        }
        assert!(
            !supported().is_empty(),
            "EGL importuje dmabuf, ale żaden z formatów ARGB/XRGB/ABGR/XBGR nie jest dostępny"
        );
    }

    #[test]
    fn accepts_advertised_format_once() {
        if !available() {
            eprintln!("pominięto: brak EGL");
            return;
        }
        let (format, mods) = supported()[0].clone();
        let p = params(format, 4 * 64, mods[0]);
        assert!(p.build(64, 64, format, 0).is_some());
        // Obiekt parametrów można użyć tylko raz.
        assert!(p.build(64, 64, format, 0).is_none());
    }

    #[test]
    fn rejects_invalid_arguments() {
        if !available() {
            return;
        }
        let (format, mods) = supported()[0].clone();
        // Zły format.
        assert!(params(0xdead_beef, 256, mods[0])
            .build(64, 64, 0xdead_beef, 0)
            .is_none());
        // Zły stride.
        assert!(params(format, 0, mods[0])
            .build(64, 64, format, 0)
            .is_none());
        // Niepoprawne wymiary.
        assert!(params(format, 256, mods[0])
            .build(0, 64, format, 0)
            .is_none());
        // Flagi (y_invert/interlaced) nie są obsługiwane w etapie 2.
        assert!(params(format, 256, mods[0])
            .build(64, 64, format, 1)
            .is_none());
        // Modyfikator spoza listy.
        if !mods.contains(&0x1234_5678_9abc_def0) {
            assert!(params(format, 256, 0x1234_5678_9abc_def0)
                .build(64, 64, format, 0)
                .is_none());
        }
    }

    #[test]
    fn rejects_duplicate_plane() {
        if !available() {
            return;
        }
        let (format, mods) = supported()[0].clone();
        let p = Params::default();
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        p.add(
            0,
            Plane {
                fd,
                offset: 0,
                stride: 256,
                modifier: mods[0],
            },
        );
        let fd2: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        p.add(
            0,
            Plane {
                fd: fd2,
                offset: 0,
                stride: 256,
                modifier: mods[0],
            },
        );
        // Druga próba na tym samym indeksie jest ignorowana, więc nadal jest plan 0.
        assert!(p.build(64, 64, format, 0).is_some());
    }
}
