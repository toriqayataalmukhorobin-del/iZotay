# Mencoba paket build iZotay

Paket GitHub Release adalah build preview publik untuk mencoba iZotay. Setiap platform menyediakan arsip portable dan paket instalasi: Windows Setup (`.exe`), Linux Debian (`.deb`), serta macOS disk image (`.dmg`). FFmpeg tidak disertakan.

## Persyaratan

- Jalankan pada sesi desktop aktif dengan perangkat audio output yang berfungsi. Menjalankan aplikasi lewat SSH/headless tanpa display tidak akan membuka jendela.
- Pasang FFmpeg dan pastikan perintah `ffmpeg` tersedia di `PATH` untuk impor format non-WAV, audio video, denoise, dan ekspor MP4.
- Di Linux, pustaka runtime audio dan grafis desktop harus tersedia. Paket development tidak diperlukan untuk menjalankan binary.

## Menjalankan

- **Windows:** jalankan `izotay-Windows-X64-setup.exe`, atau ekstrak ZIP dan jalankan `izotay.exe`. Installer menambahkan shortcut Start Menu dan menawarkan shortcut desktop.
- **macOS:** buka DMG lalu salin `Izotay.app` ke Applications, atau ekstrak tar.gz dan jalankan `./izotay` dari Terminal. Aplikasi preview belum ditandatangani atau notarized; macOS dapat meminta pengguna mengizinkannya melalui Privacy & Security.
- **Linux Debian/Ubuntu:** pasang paket dengan `sudo apt install ./izotay-Linux-X64.deb`; paket portable `.tar.gz` juga tersedia. Distribusi Linux lain dapat menjalankan `./izotay` dari arsip setelah `chmod +x izotay`.

> Aplikasi menyimpan edit dalam sesi dan membuat file output baru. File media sumber tidak ditimpa.

Paket instalasi mengatur binary dan dokumentasi aplikasi, tetapi tidak membawa FFmpeg. Paket preview belum ditandatangani; lisensi dependency dan kompatibilitas runtime pada setiap distribusi tetap perlu ditinjau sebelum distribusi produksi.
