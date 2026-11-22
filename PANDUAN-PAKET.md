# Mencoba paket build iZotay

Paket dari GitHub Actions ditujukan untuk uji internal prototipe, bukan installer final. Isinya executable iZotay dan panduan ini. FFmpeg tidak disertakan.

## Persyaratan

- Jalankan pada sesi desktop aktif dengan perangkat audio output yang berfungsi. Menjalankan aplikasi lewat SSH/headless tanpa display tidak akan membuka jendela.
- Pasang FFmpeg dan pastikan perintah `ffmpeg` tersedia di `PATH` untuk impor format non-WAV, audio video, denoise, dan ekspor MP4.
- Di Linux, pustaka runtime audio dan grafis desktop harus tersedia. Paket development tidak diperlukan untuk menjalankan binary.

## Menjalankan

- **Windows:** ekstrak ZIP, lalu jalankan `izotay.exe`.
- **macOS:** ekstrak arsip, buka Terminal di folder hasil ekstrak, lalu jalankan `./izotay`. Binary preview ini belum ditandatangani atau notarized.
- **Linux:** ekstrak arsip lalu jalankan `./izotay`. Jika perlu, beri izin executable dengan `chmod +x izotay`.

> Aplikasi menyimpan edit dalam sesi dan membuat file output baru. File media sumber tidak ditimpa.

Paket ini belum berupa installer dan belum untuk distribusi publik. Lisensi dependency, penandatanganan kode, dan packaging final masih perlu ditinjau.
