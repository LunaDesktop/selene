//! `wl_shm` — współdzielona pamięć klienta (bufory ARGB8888 → RGBA).

use std::ffi::c_void;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use wayland_server::{
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use libluna_render::DmabufImage;

use crate::state::State;
use crate::sync::LockExt;

/// Górny limit pojedynczego obrazu (RGBA) — chroni przed OOM wymuszonym przez klienta.
/// 256 MiB pokrywa 8K na kilku monitorach, a odcina absurdalne żądania.
const MAX_IMAGE_BYTES: usize = 256 * 1024 * 1024;

/// Zmapowany region puli. Odświeżany przy `wl_shm_pool.resize`.
struct Mapping {
    ptr: *mut c_void,
    size: usize,
}

// SAFETY: mapa jest tylko do odczytu; dostęp do `ptr` serializuje `Mutex`
// (i tak dzieje się to w wątku protokołu).
unsafe impl Send for Mapping {}

/// Dane puli pamięci (`wl_shm_pool`): deskryptor + zmapowany region.
///
/// Klient może tworzyć pulę małą i **rozszerzać** ją (`wl_shm_pool.resize`) —
/// dlatego mapa i rozmiar żyją za `Mutex` i są powiększane na żądanie.
pub struct ShmPoolData {
    pub fd: OwnedFd,
    map: Mutex<Mapping>,
}

impl ShmPoolData {
    /// Mapuje początkowy zakres puli (tylko odczyt). Błąd mapowania = pusta pula.
    pub fn new(fd: OwnedFd, declared_size: usize) -> Self {
        // Faktyczny rozmiar obiektu shm: klient może zadeklarować więcej, niż istnieje,
        // a odczyt poza obiekt kończy się SIGBUS (nie do złapania). Przycinamy rozmiar.
        let real = real_size(&fd);
        let size = declared_size.min(real);
        let ptr = map_region(&fd, size);
        if debug() {
            eprintln!("selene/shm: create_pool declared={declared_size} real={real} used={size}");
        }
        Self {
            fd,
            map: Mutex::new(Mapping { ptr, size }),
        }
    }

    /// Powiększa mapę po `wl_shm_pool.resize` (klient wcześniej powiększył `fd`).
    ///
    /// Rozmiar bywa deklarowany większy niż realny obiekt — przycinamy do realnego,
    /// żeby nie czytać poza obiekt (SIGBUS).
    pub fn resize(&self, declared_size: usize) {
        let real = real_size(&self.fd);
        let size = declared_size.min(real);
        let mut map = self.map.lock_or_recover();
        if size <= map.size {
            return; // tylko rośnie
        }
        if !map.ptr.is_null() {
            // SAFETY: odmapowujemy poprzedni zakres.
            unsafe {
                let _ = rustix::mm::munmap(map.ptr, map.size);
            }
        }
        map.ptr = map_region(&self.fd, size);
        map.size = size;
        if debug() {
            eprintln!("selene/shm: pool resize -> used={size}");
        }
    }
}

impl Drop for ShmPoolData {
    fn drop(&mut self) {
        let map = self.map.get_mut().unwrap_or_else(|e| e.into_inner());
        if map.ptr.is_null() {
            return;
        }
        // SAFETY: odmapowujemy ten sam zakres, który zmapowaliśmy.
        unsafe {
            let _ = rustix::mm::munmap(map.ptr, map.size);
        }
    }
}

/// Realny rozmiar obiektu shm (bez deklaracji klienta).
fn real_size(fd: &OwnedFd) -> usize {
    rustix::fs::fstat(fd)
        .map(|st| st.st_size.max(0) as usize)
        .unwrap_or(0)
}

/// Mapuje `size` bajtów pliku `fd` (tylko odczyt). `null` przy niepowodzeniu.
fn map_region(fd: &OwnedFd, size: usize) -> *mut c_void {
    if size == 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: `fd` żyje w strukturze, mapujemy tylko do odczytu.
    unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut::<c_void>(),
            size,
            rustix::mm::ProtFlags::READ,
            rustix::mm::MapFlags::PRIVATE,
            fd,
            0,
        )
    }
    .unwrap_or(std::ptr::null_mut())
}

fn debug() -> bool {
    std::env::var("SELENE_STATS").is_ok()
}

/// Dane bufora (`wl_buffer`): albo pula shm, albo bufor `dmabuf` klienta GPU.
pub struct BufferData {
    /// `Some` dla `wl_shm`; `None` dla `dmabuf`.
    pub pool: Option<Arc<ShmPoolData>>,
    pub offset: usize,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: wl_shm::Format,
    /// Bufor GPU; import wykonuje backend rysujący (`GpuRenderer`).
    pub dmabuf: Option<Arc<DmabufImage>>,
}

impl BufferData {
    /// Bufor bez puli shm — tylko metadane `dmabuf`.
    pub fn dmabuf(image: Arc<DmabufImage>, format: wl_shm::Format) -> Self {
        Self {
            pool: None,
            offset: 0,
            width: image.width as i32,
            height: image.height as i32,
            stride: 0,
            format,
            dmabuf: Some(image),
        }
    }

    /// Bufor, którego import się nie udał — istnieje dla protokołu, nic nie rysuje.
    pub fn failed(width: i32, height: i32) -> Self {
        Self {
            pool: None,
            offset: 0,
            width,
            height,
            stride: 0,
            format: wl_shm::Format::Argb8888,
            dmabuf: None,
        }
    }
}

impl BufferData {
    /// Odczytuje piksele bufora jako premultiplied RGBA8 (swizzle BGRA→RGBA).
    ///
    /// Dla `dmabuf` zwraca `None` — piksele żyją wyłącznie na GPU.
    ///
    /// Waliduje geometrię względem rozmiaru puli (arytmetyka bez przepełnień) i ogranicza
    /// rozmiar alokacji, żeby klient nie wywołał paniki ani OOM.
    pub fn to_rgba(&self) -> Option<Vec<u8>> {
        self.to_rgba_with_opacity().map(|(rgba, _)| rgba)
    }

    /// Jak [`BufferData::to_rgba`], ale dodatkowo mówi, czy **każdy** piksel ma
    /// `alpha == 255`. Dzięki temu renderer może skopiować obraz zamiast go
    /// blendować (okna `Xrgb8888` i bufory ARGB bez przezroczystości).
    pub fn to_rgba_with_opacity(&self) -> Option<(Vec<u8>, bool)> {
        if self.dmabuf.is_some() {
            return None;
        }
        let pool = self.pool.as_ref()?;
        if self.width <= 0 || self.height <= 0 || self.stride <= 0 {
            return None;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = self.stride as usize;
        // Wiersz musi pomieścić `w` pikseli po 4 bajty (inaczej odczyt wyszedłby poza wiersz).
        if stride < w.checked_mul(4)? {
            return None;
        }
        // Cały zakres danych musi mieścić się w puli — na sprawdzonej arytmetyce.
        let end = self.offset.checked_add(stride.checked_mul(h)?)?;
        // Limit alokacji chroniący przed OOM wymuszonym przez klienta.
        let out_len = w.checked_mul(h)?.checked_mul(4)?;
        if out_len > MAX_IMAGE_BYTES {
            return None;
        }

        // Trzymamy blokadę mapy przez cały odczyt (mapa może rosnąć w `resize`).
        let map = pool.map.lock_or_recover();
        if map.ptr.is_null() || end > map.size {
            return None;
        }
        let base = map.ptr as *const u8;
        let swizzle = matches!(
            self.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        );
        // `Abgr8888`/`Xbgr8888` mają w pamięci kolejność RGBA — dokładnie taką, jakiej
        // używa renderer, więc wystarczy `memcpy` (bez podmiany R↔B).
        let direct = matches!(
            self.format,
            wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888
        );
        if !swizzle && !direct {
            return None;
        }
        let force_alpha = matches!(
            self.format,
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888
        );
        let mut opaque = true;
        let mut out = vec![0u8; out_len];
        #[cfg(target_endian = "little")]
        {
            // Piksele przetwarzamy po cztery bajty naraz (u32). Podmiana R↔B to kilka
            // operacji bitowych — wcześniejszy pętla bajt-po-bajcie na buforze
            // 1920×1080 zajmowała kilka milisekund **w wątku protokołu**, opóźniając
            // obsługę wejścia (kliknięcia i ruch kursora).
            for y in 0..h {
                let row = self.offset + y * stride;
                let row_dst = &mut out[y * w * 4..(y + 1) * w * 4];
                let row_src = unsafe { std::slice::from_raw_parts(base.add(row), w * 4) };
                if direct {
                    row_dst.copy_from_slice(row_src);
                    if force_alpha {
                        for px in row_dst.chunks_exact_mut(4) {
                            px[3] = 255;
                        }
                    } else {
                        opaque &= row_dst.chunks_exact(4).all(|px| px[3] == 255);
                    }
                    continue;
                }
                let dst32 =
                    unsafe { std::slice::from_raw_parts_mut(row_dst.as_mut_ptr() as *mut u32, w) };
                let src32 =
                    unsafe { std::slice::from_raw_parts(row_src.as_ptr() as *const u32, w) };
                for (d, &s) in dst32.iter_mut().zip(src32.iter()) {
                    let mut v =
                        (s & 0xFF00_FF00) | ((s & 0x00FF_0000) >> 16) | ((s & 0x0000_00FF) << 16);
                    if force_alpha {
                        v |= 0xFF00_0000;
                    } else if s & 0xFF00_0000 != 0xFF00_0000 {
                        opaque = false;
                    }
                    *d = v;
                }
            }
        }
        #[cfg(not(target_endian = "little"))]
        {
            for y in 0..h {
                let row = self.offset + y * stride;
                let row_dst = &mut out[y * w * 4..(y + 1) * w * 4];
                let row_src = unsafe { std::slice::from_raw_parts(base.add(row), w * 4) };
                if direct {
                    row_dst.copy_from_slice(row_src);
                    if force_alpha {
                        for px in row_dst.chunks_exact_mut(4) {
                            px[3] = 255;
                        }
                    } else {
                        opaque &= row_dst.chunks_exact(4).all(|px| px[3] == 255);
                    }
                    continue;
                }
                for (d, s) in row_dst.chunks_exact_mut(4).zip(row_src.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = if force_alpha { 255 } else { s[3] };
                    if !force_alpha && s[3] != 255 {
                        opaque = false;
                    }
                }
            }
        }
        Some((out, opaque))
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_shm::WlShm>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        // Ogłaszamy **wszystkie** formaty, które realnie obsługujemy. Część toolkitów (Qt,
        // część aplikacji GTK/Electron) domyślnie tworzy bufor `Xrgb8888` i bez niego w ogóle
        // nie dochodzi do `commit`. `Abgr8888`/`Xbgr8888` mają w pamięci kolejność RGBA
        // (natywną dla naszego renderera), więc klient unika podmiany R↔B co klatkę.
        for format in [
            wl_shm::Format::Argb8888,
            wl_shm::Format::Xrgb8888,
            wl_shm::Format::Abgr8888,
            wl_shm::Format::Xbgr8888,
        ] {
            let _ = shm.send_event(wl_shm::Event::Format {
                format: wayland_server::WEnum::Value(format),
            });
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_shm::WlShm,
        request: wl_shm::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            data_init.init(id, Arc::new(ShmPoolData::new(fd, size.max(0) as usize)));
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, Arc<ShmPoolData>> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &Arc<ShmPoolData>,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                data_init.init(
                    id,
                    BufferData {
                        pool: Some(Arc::clone(data)),
                        offset: offset.max(0) as usize,
                        width,
                        height,
                        stride,
                        format: format.into_result().unwrap_or(wl_shm::Format::Argb8888),
                        dmabuf: None,
                    },
                );
            }
            wl_shm_pool::Request::Resize { size } => {
                data.resize(size.max(0) as usize);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn memfd(bytes: &[u8]) -> OwnedFd {
        let fd =
            rustix::fs::memfd_create("selene-shm-test", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes).unwrap();
        file.into()
    }

    fn pool(bytes: &[u8]) -> Arc<ShmPoolData> {
        Arc::new(ShmPoolData::new(memfd(bytes), bytes.len()))
    }

    fn buffer(pool: Arc<ShmPoolData>, w: i32, h: i32, stride: i32) -> BufferData {
        BufferData {
            pool: Some(pool),
            offset: 0,
            width: w,
            height: h,
            stride,
            format: wl_shm::Format::Argb8888,
            dmabuf: None,
        }
    }

    #[test]
    fn to_rgba_swizzles_bgra_to_rgba() {
        // 1x1: pamięć B,G,R,A = (10,20,30,40) → RGBA (30,20,10,40).
        let rgba = buffer(pool(&[10, 20, 30, 40]), 1, 1, 4).to_rgba().unwrap();
        assert_eq!(rgba, vec![30, 20, 10, 40]);
    }

    #[test]
    fn xrgb_forces_opaque_alpha() {
        let mut b = buffer(pool(&[1, 2, 3, 0]), 1, 1, 4);
        b.format = wl_shm::Format::Xrgb8888;
        assert_eq!(b.to_rgba().unwrap(), vec![3, 2, 1, 255]);
        assert!(b.to_rgba_with_opacity().unwrap().1);
    }

    #[test]
    fn argb_opacity_follows_alpha_bytes() {
        // Wszystkie piksele kryjące → obraz można skopiować zamiast blendować.
        let b = buffer(pool(&[10, 20, 30, 255, 40, 50, 60, 255]), 2, 1, 8);
        let got = b.to_rgba_with_opacity().unwrap();
        assert!(
            got.1,
            "rgba={:?} opaque={} format={:?}",
            got.0, got.1, b.format
        );
        // Jeden piksel półprzezroczysty → trzeba blendować.
        let b = buffer(pool(&[10, 20, 30, 255, 40, 50, 60, 128]), 2, 1, 8);
        assert!(!b.to_rgba_with_opacity().unwrap().1);
    }

    #[test]
    fn abgr_is_copied_without_swizzle() {
        let mut b = buffer(pool(&[10, 20, 30, 40]), 1, 1, 4);
        b.format = wl_shm::Format::Abgr8888;
        assert_eq!(b.to_rgba().unwrap(), vec![10, 20, 30, 40]);
    }

    #[test]
    fn xbgr_forces_opaque_alpha_without_swizzle() {
        let mut b = buffer(pool(&[1, 2, 3, 0]), 1, 1, 4);
        b.format = wl_shm::Format::Xbgr8888;
        assert_eq!(b.to_rgba().unwrap(), vec![1, 2, 3, 255]);
    }

    #[test]
    fn rejects_stride_smaller_than_row() {
        // stride 2 < width*4 → odrzucone, bez paniki/OOB.
        assert!(buffer(pool(&[0u8; 64]), 4, 1, 2).to_rgba().is_none());
    }

    #[test]
    fn rejects_buffer_outside_pool() {
        // 2x2 ARGB = 16 bajtów > pula 8 → odrzucone.
        assert!(buffer(pool(&[0u8; 8]), 2, 2, 8).to_rgba().is_none());
    }

    #[test]
    fn resize_extends_mapping() {
        // W pliku jest 8 B, ale deklarujemy na starcie 4 B (jak sctk: mała pula + resize).
        let pool = Arc::new(ShmPoolData::new(memfd(&[1, 2, 3, 4, 5, 6, 7, 8]), 4));
        // Bufor 2x1 (8 B) nie mieści się w zmapowanych 4 B.
        assert!(buffer(Arc::clone(&pool), 2, 1, 8).to_rgba().is_none());
        // Po `resize` mapa rośnie i bufor się mieści.
        pool.resize(8);
        let rgba = buffer(Arc::clone(&pool), 2, 1, 8).to_rgba().unwrap();
        assert_eq!(rgba, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }
}
