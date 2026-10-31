# Selene

Kompozytor Wayland środowiska Luna. Okna xdg-shell, panele wlr-layer-shell, wejście evdev, DRM/KMS, XWayland i multi-monitor. Napisany w Rust bez smithay.

> **Języki:** [Polski](README.pl.md) · [English](README.en.md) · [Deutsch](README.de.md)

## Część projektu Luna

Repozytorium jest częścią organizacji [LunaDesktop](https://github.com/LunaDesktop) i projektu **Luna** — pulpitu pisanego w Rust, bez GTK, Qt i GObject.

## Budowanie

```bash
git clone https://github.com/LunaDesktop/selene.git
cd selene
cargo build --release
```

Wymagania: Linux, Rust 1.80+, Wayland (dla aplikacji okienkowych).

## Uruchomienie

```bash
cargo run --release
```

## Dokumentacja

- Strona projektu: <https://luna.paganlinux.eu/>
- Dokumentacja deweloperska: <https://luna.paganlinux.eu/docs/>
- Wersja angielska: <https://luna.paganlinux.eu/en/docs/>
- Indeks API: <https://luna.paganlinux.eu/en/docs/api.html>

## Licencja

Kod jest dostępny na licencji **GPL-3.0-or-later**. Pełny tekst znajduje się w pliku [LICENSE](LICENSE).
