# iZotay v0.2.0 — Preview

Rilis preview iZotay dengan equalizer parametrik per track, preview frame video di Inspector, dan paket pemasangan untuk tiga platform.

## Perubahan

- Equalizer 6-band dapat diatur lewat kurva drag atau slider frekuensi, gain, dan Q. Pengaturan EQ ikut ke playback, ekspor, undo, perbandingan A/B, sesi, preset, dan cache efek.
- Inspector menampilkan still frame video yang mengikuti playhead; FFmpeg mengekstrak frame di latar belakang. Audio video tetap dapat diedit dan diekspor ke MP4 baru tanpa mengubah sumber.
- Format sesi versi 18 menyimpan EQ dan tetap membuka sesi versi 1–17 dengan nilai default aman.
- GitHub Actions menghasilkan paket portable dan paket pemasangan Windows Setup, Linux `.deb`, serta macOS `.dmg`.
- [Panduan Pengguna](PANDUAN-PENGGUNA.md), [Panduan Paket](PANDUAN-PAKET.md), dan [README](README.md) telah diperbarui.

## Sebelum mencoba

- Pasang FFmpeg secara terpisah untuk format non-WAV, preview dan audio video, denoise, batch, dan ekspor MP4.
- macOS build preview belum ditandatangani atau notarized. Sistem mungkin meminta izin saat pertama kali dibuka.
- Tetap simpan edit dalam sesi dan ekspor sebagai berkas baru; media sumber tidak ditimpa.
- Uji perangkat audio/latensi, pembaca layar, lisensi dependency, dan hasil restorasi pada materi produksi masih diperlukan.

Lihat [README](README.md) untuk fitur dan batasan lengkap.
