# iZotay — Editor Restorasi Audio Multi-Track Mini Equalizer

iZotay adalah prototipe editor desktop lokal untuk dialog, podcast, wawancara, dan voice-over. Sumber media tidak ditimpa; edit klip dan parameter restorasi disimpan sebagai instruksi di sesi `.izotay`.

- **Panduan penggunaan:** [PANDUAN-PENGGUNA.md](PANDUAN-PENGGUNA.md)
- **Menjalankan paket preview:** [PANDUAN-PAKET.md](PANDUAN-PAKET.md)

## Fitur yang tersedia

- Impor beberapa WAV, format audio lain, atau video dengan audio sebagai track. Preview frame mengikuti playhead; audio video dapat diedit dan diekspor ke MP4 baru tanpa mengubah video sumber.
- Timeline multi-track dengan zoom, waveform multi-resolusi, playhead, marker, seleksi waktu, loop preview, snap, split, trim, pindah/duplikasi klip, fade, crossfade, hapus, undo, dan redo.
- Tampilan waveform atau spectrogram per track; spectrogram memakai 64 pita frekuensi dan cache ringkas untuk sumber panjang.
- Equalizer parametrik 6-band per track dengan titik kurva yang dapat digeser, kontrol frekuensi 20 Hz–20 kHz, gain ±18 dB, Q 0,25–8, bypass, dan penyimpanan sesi/preset.
- Restorasi per track: high-pass rumble, notch hum 50/60 Hz, expander, denoise FFmpeg `afftdn`, de-click, de-clip, de-esser, dan penekan tail reverb ringan. Kontrol menyediakan bypass, A/B, dan profil noise dari seleksi.
- Otomasi gain per track dengan titik pada playhead, interpolasi linear, tampilan kurva di klip, undo/redo, dan penyimpanan sesi.
- Bleed reduction per track memakai track referensi, reduksi 0–24 dB, offset manual ±500 ms, dan alignment otomatis berbasis korelasi hingga ±500 ms.
- Preset restorasi portabel (`.izopreset`) dapat disimpan dari track dan diterapkan ke track lain. Batch processing memakai satu preset untuk banyak sumber audio/video dan membuat WAV PCM 24-bit.
- Konsolidasi proyek menyalin sumber media ke folder `Media` bersama sesi `.izotay` dengan path relatif agar folder mudah dipindahkan; file asli tetap utuh.
- Aksesibilitas menyediakan dukungan AccessKit untuk teknologi bantu, kontras tinggi, skala UI 80–160%, target kontrol yang lebih besar, serta shortcut yang tidak mengambil alih pengetikan di kolom teks.
- Preview dapat dibatasi ke seleksi; ekspor menerapkan efek aktif ke seluruh rentang ekspor.
- Playback, meter peak, ekspor mix atau track ke WAV PCM 16/24/32-bit, normalisasi peak, dan ekspor audio hasil edit ke MP4.
- Sesi versi 18 dapat menyimpan pengaturan timeline, klip, marker, efek termasuk EQ, profil noise, otomasi gain, referensi bleed, alignment, preferensi aksesibilitas, dan pengaturan ekspor. Sesi versi 1–17 tetap dibaca; EQ pada sesi lama mulai dengan enam band datar dan bypass.
- Sumber media, waveform, dan hasil DSP memakai cache file-backed. Pemrosesan full-track berjalan bertahap; cache sampel dibatasi sekitar 4 GiB secara best-effort.

## Batasan saat ini

- De-click/de-clip memakai rekonstruksi sampel terisolasi, de-esser memakai deteksi energi high-band, dan de-reverb menekan level tail yang pelan. Dengarkan hasilnya pada materi sasaran sebelum produksi.
- Bleed reduction paling efektif bila referensi berisi sumber bleed yang sama dengan timing dekat. Periksa hasilnya karena komponen suara yang berkorelasi memang akan dikurangi dari track target.
- Batch processing mengeluarkan WAV PCM 24-bit per sumber, menggunakan efek preset tanpa relasi track referensi atau profil noise berbasis seleksi. Konsolidasi menyalin media dan sesi; FFmpeg dan cache sementara tetap dikelola terpisah.
- Preview efek untuk seleksi yang sangat panjang masih menyimpan hasil seleksi di memori.
- Cache berada di direktori sementara dan dapat dibangun ulang; gunakan file sesi dan media sumber sebagai data proyek utama.
- FFmpeg harus tersedia di `PATH` untuk format non-WAV, audio video, denoise, dan ekspor MP4.
- Aplikasi masih prototipe/preview. Paket portable tersedia bersama installer Windows, paket `.deb` Linux, dan disk image macOS. Uji perangkat audio/latensi, rekaman dialog nyata, pembaca layar, lisensi dependency, dan penandatanganan/notarization macOS tetap perlu dilakukan sebelum rilis produksi.

## Persiapan dan menjalankan

Pasang Rust stable. Instal FFmpeg untuk semua fitur media non-WAV dan restorasi denoise.

Di Linux, pasang juga pustaka development desktop/audio:

```sh
sudo apt-get install ffmpeg libasound2-dev libgl1-mesa-dev libwayland-dev libx11-dev libxkbcommon-dev pkg-config
```

Jalankan aplikasi:

```sh
cargo run
```

Validasi lokal:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Workflow GitHub Actions menjalankan tes, Clippy, build release, dan pembuatan paket pada Linux, Windows, serta macOS. Tag `v*` membuat GitHub Release preview dengan arsip portable dan paket instalasi tiap platform. FFmpeg tetap menjadi dependensi terpisah. CI tidak menguji perangkat audio fisik, latensi playback, screen reader, ataupun kualitas restorasi pada rekaman produksi.

## Arsitektur

Proyek saat ini berupa satu binary Rust dengan `eframe/egui` untuk UI, `cpal` untuk output audio, `hound` untuk WAV, `memmap2` untuk cache sampel file-backed, serta FFmpeg untuk format non-WAV, denoise, dan muxing video. Impor, pembuatan cache, pemrosesan mix, dan ekspor berjalan di worker agar tidak mengunci UI.

Keputusan teknis dan batas yang diketahui ada di [KEPUTUSAN-TEKNIS.md](KEPUTUSAN-TEKNIS.md).

## Roadmap

- [x] Tahap 0 — Validasi fondasi playback, waveform timeline, dan format sesi.
- [x] Tahap 1 — Editor multi-track dasar, sesi, undo/redo, dan ekspor WAV.
- [x] Tahap 2 — Restorasi inti, preview A/B, meter, cache waveform dan media panjang, pemrosesan DSP bertahap.
- [x] Tahap 3 — Spectrogram, de-click, de-clip, de-esser, de-reverb, dan otomasi gain.
- [x] Tahap 4 — Bleed reduction dengan track referensi dan alignment.
- [x] Tahap 5 — Preset produksi, batch processing, konsolidasi aset, dan aksesibilitas.

## Lisensi

Kode proyek menggunakan lisensi [MIT](LICENSE). Lisensi dependency pihak ketiga tetap mengikuti lisensi masing-masing.
