# Editor Multi-track — Keputusan Teknis

Status: prototipe editor multi-track tahap 1 dengan beberapa fondasi tahap 2; belum siap dibekukan sebagai teknologi produksi.

## Pilihan prototipe

- **UI:** `egui`/`eframe` untuk jendela desktop native lintas platform.
- **Output audio:** `cpal` memakai perangkat output default; callback mencampur frame dari sumber yang sudah didekode dan diproses, tanpa alokasi memori.
- **Media:** `hound` untuk WAV; FFmpeg lokal untuk mengekstrak audio dari video dan membaca format audio lain.
- **Pemilih file:** `rfd` untuk dialog native.
- **Sesi:** JSON berversi dengan `serde`; sumber media direferensikan lewat path, bukan disalin ke sesi.
- **Struktur:** satu binary untuk validasi awal; pemisahan crate dilakukan setelah kontrak model sesi dan engine stabil.

## Kemampuan prototipe saat ini

- Impor beberapa berkas audio atau video sebagai track; audio video ditampilkan sebagai waveform tanpa mengubah gambar atau sumber aslinya. Nama track dapat diubah, urutannya dapat dipindah, dan waveform, mute/solo, gain, serta pan tersedia.
- Timeline dasar dengan playhead, pindah/duplikasi klip, split pada playhead, trim awal/akhir, hapus klip/track, serta undo/redo sampai 100 langkah.
- Zoom timeline horizontal dapat diatur dari 25 hingga 400 piksel per detik; posisi playhead, marker, seleksi, dan drag klip mengikuti skala yang sama. Pengaturan zoom disimpan per sesi.
- Snap opsional saat memindahkan klip mengunci tepi klip ke playhead, marker, atau tepi klip lain dalam jarak 8 piksel; status snap disimpan per sesi.
- Gain klip 0–4× dan fade in/out linear non-destruktif; parameter tersimpan di sesi, dapat di-undo, serta dipakai untuk playback, meter track, dan ekspor. Klip hasil split mewarisi gain.
- Seleksi rentang waktu lewat seret pada area kosong timeline atau Shift+seret klip; rentang dapat di-loop untuk preview dan dipakai untuk ekspor mix atau track.
- Shortcut Space untuk playback, S untuk split, Ctrl/Cmd+D untuk duplikasi, dan Ctrl/Cmd+Z/Y untuk undo/redo.
- Marker timeline dengan label otomatis; klik marker untuk memindahkan playhead. Marker ikut tersimpan dan dipulihkan oleh undo/redo.
- Playback mencampur frame dari track yang telah diproses di callback audio dan tetap berjalan memakai mix terakhir saat render baru berlangsung; mix terbaru melanjutkan dari posisi playhead. Mix stereo/track diekspor bertahap tanpa membuat buffer output berdurasi penuh. Ekspor WAV mendukung PCM 16/24/32-bit, pilihan sample rate 44,1/48/96/192 kHz atau mengikuti sumber, normalisasi peak ke −1 dBFS dengan batas boost +40 dB, progres, dan pembatalan. File tujuan baru diganti setelah file sementara selesai ditulis.
- Track dari video dapat diekspor sebagai MP4 baru dengan audio hasil edit; stream video disalin tanpa re-encode dan audio dikodekan AAC. FFmpeg diperlukan untuk impor maupun ekspor video.
- Impor/decode WAV, pembukaan sesi, penghitungan filter/mix, dan render ekspor berjalan di worker latar belakang. Antrean mix hanya menyimpan permintaan terbaru dan membatalkan render lama saat pengaturan berubah.
- High-pass rumble per track dengan rentang 0–180 Hz dan bypass untuk perbandingan cepat. Filter satu pole diproses dari salinan buffer, bukan sumber.
- Hum removal per track dengan pilihan listrik 50/60 Hz, notch sempit pada fundamental dan sampai delapan harmonik, serta bypass.
- Perbandingan A/B untuk parameter rumble, hum, dan expander per track; set A tersimpan dalam sesi sementara kontrol tetap mengedit set B.
- Expander per track dengan threshold -60 hingga -20 dBFS, bypass, penurunan level konservatif sekitar 15 dB, attack 10 ms, dan release 120 ms.
- Meter peak master live menampilkan level linear dan dBFS dari callback output; meter per track menunjukkan peak pada jendela 10 ms di sekitar playhead.
- Simpan/buka sesi `.izotay` versi 10; versi 1–9 tetap dibaca. Gain klip baru memakai default 1×, zoom memakai nilai default, dan snap nonaktif pada sesi lama. Pengaturan ekspor dan set A ikut disimpan. Sumber media direferensikan lewat path, bukan disalin ke sesi.
- Sesi tetap dapat dibuka saat sumber tidak tersedia; track ditandai offline dan dapat ditautkan ulang lewat dialog file. Klip yang melewati durasi sumber baru dipangkas ke rentang valid.
- Mix menggunakan sample rate track pertama; sumber dan playback perangkat memakai interpolasi linear saat menyamakan laju sampel.

## Batas yang diketahui

- Sumber audio dibaca seluruhnya ke memori, dan versi DSP aktif dapat menambah salinan sumber per track; file panjang tetap dapat menghabiskan memori. Impor dan ekspor video memerlukan executable FFmpeg di PATH.
- Klip hasil impor dimulai di waktu nol; pemindahan, split, trim, dan hapus tersedia.
- Undo/redo mencakup edit dan pengelolaan track sampai 100 langkah. Setelah sesi disimpan dan diedit, autosave dibuat setelah jeda 2 detik; saat membuka sesi, autosave yang lebih baru dipulihkan ke memori tanpa menimpa file sesi.
- Ekspor track menerapkan efek yang sedang aktif. Pembatalan diperiksa selama filter dan penulisan, tetapi operasi sistem berkas saat mengganti file tidak dapat dihentikan di tengah.
- Normalisasi mengukur peak sample pada rentang ekspor, bukan loudness LUFS; boost dibatasi 40 dB agar rekaman sangat pelan tidak dinaikkan tanpa batas.
- Notch hum dapat mengurangi komponen suara yang kebetulan berada dekat harmoniknya; bypass tersedia, dan hasil tetap perlu didengar pada rekaman sasaran.
- Preview DSP yang dibatasi ke seleksi, denoise, dan kompensasi artefak belum tersedia; expander perlu diuji pada dialog pelan sebelum dipakai di produksi.
- Playback menghitung mix per frame di callback audio dan tidak menyimpan buffer mix penuh. Sumber masih dimuat seluruhnya di memori; daftar klip yang sangat padat dapat menambah beban callback.
- Snap ke grid waktu, preview gambar video, dan pengubahan tinggi track belum tersedia. Loop hanya mengulang rentang waktu yang dipilih.
- Meter live master dibaca dari callback output; meter track menampilkan peak pada jendela 10 ms di sekitar playhead.
- Batas loop belum memakai crossfade; pilih rentang dengan transisi yang bersih untuk menghindari klik.
- Relink memangkas klip yang sumbernya melebihi durasi berkas yang dipilih; kecocokan identitas media belum diverifikasi otomatis.
- Penggantian file sesi memakai file sementara dan cadangan agar dapat berjalan pada sistem yang tidak mendukung rename langsung di atas file yang ada.
- `cargo check`, build debug, dan Clippy dengan warning sebagai error bersih setelah perubahan media. Unit test tidak dijalankan ulang. UI berhasil dibuka pada desktop; uji alur impor/ekspor media dan validasi lintas Windows/macOS masih diperlukan.

## Langkah validasi berikutnya

1. Uji impor, sesi, playback, dan ekspor pada aplikasi berjalan dengan WAV mono/stereo, PCM 16/24/32-bit, float, serta sample rate berbeda.
2. Tambahkan pembacaan media bertahap dan cache waveform agar sesi panjang tidak memerlukan seluruh sumber di memori.
3. Tambahkan preview A/B seleksi dan denoise.
4. Uji sistem operasi sasaran, latensi, lisensi, dan packaging sebelum teknologi dianggap final.
