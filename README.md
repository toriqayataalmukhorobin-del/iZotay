🎙️ Audio Dialog & Restoration Editor (Multi-track)

Editor Restorasi Audio Multi-track adalah aplikasi desktop lintas platform yang dirancang khusus untuk menyunting dialog, podcast, wawancara, dan voice-over. Aplikasi ini berfokus pada kemudahan memperbaiki masalah rekaman audio secara lokal, non-destruktif, serta mendukung workflow multi-mikrofon sejak fondasi.

✨ Fitur Utama

🎚️ Editing Multi-track & Manajemen Sesi

Workflow Multi-Mikrofon: Impor banyak rekaman sekaligus (termasuk ekstraksi audio dari file video via FFmpeg lokal) dan selaraskan antar-track dengan mudah.

Non-Destruktif & Aman: File sumber asli tidak pernah ditimpa. Semua pemotongan (split), pergeseran, fade, dan efek disimpan sebagai instruksi edit di dalam file sesi.

Tampilan Visual Fleksibel: Navigasi timeline responsif dengan tampilan Waveform (default) dan Spectrogram untuk mendeteksi gangguan frekuensi.

Keandalan Sesi: Mendukung Undo/Redo berurutan, autosave dengan checkpoint pemulihan pasca-crash, serta fitur relink jika file media sumber berpindah lokasi.

🛠️ Alat Restorasi Dialog

Semua alat restorasi dilengkapi dengan Preview Seleksi (Loop), Tombol Bypass, dan Perbandingan A/B sebelum diterapkan ke sesi:

Denoise: Mengurangi noise latar stabil (kipas, AC) berdasarkan profil noise-only atau estimasi adaptif.

Gate / Expander: Menekan suara latar saat pembicara berhenti tanpa memotong konsonan akhir kata.

Hum & Rumble Removal: Menghilangkan dengung listrik periodik serta high-pass filter aman untuk meredam getaran rendah.

De-click & De-clip: Memperbaiki transien singkat dan memulihkan puncak sinyal yang terpotong (clipping) secara konservatif.

De-esser & De-reverb: Mengurangi desis sibilan yang tajam dan meredam gema ruangan ringan.

Leveling Dialog: Pengaturan gain, kompresi ringan, limiter, dan normalisasi loudness/peak untuk standar ekspor podcast.

🔇 Pengurangan Bocoran Suara (Bleed Reduction)

Mode Dengan Track Referensi: Menggunakan analisis korelasi dan time-alignment untuk mengurangi suara pembicara lain yang bocor ke mikrofon target (dilengkapi proteksi saat kedua orang berbicara bersamaan).

Mode Tanpa Referensi: Mengombinasikan gate/expander, denoise, dan otomasi volume pada jeda dialog.

📤 Ekspor Fleksibel

Ekspor mix keseluruhan atau stem/track terpisah ke format WAV PCM.

Dukungan ekspor ulang ke format MP4 baru (menggabungkan stream video asli dengan audio hasil restorasi tanpa mengubah video sumber).

🏗️ Arsitektur Perangkat Lunak (Rust)

Aplikasi ini dibangun menggunakan ekosistem Rust dengan pemisahan tanggung jawab (separation of concerns) agar real-time audio playback tidak pernah tersendat oleh proses UI maupun komputasi DSP berat:

workspace/
├── app-ui/           # Antarmuka desktop, panel alat, & visualisasi (kandidat: egui/eframe)
├── session-core/     # Model proyek, track/klip, riwayat undo/redo, autosave, & serialisasi
├── audio-engine/     # Transport, sinkronisasi track, mixing real-time, & audio callback (cpal)
├── dsp/              # Algoritma restorasi dialog, analisis korelasi, & FFT (rustfft)
├── media-io/         # Decode/encode audio (symphonia), manajemen cache, & integrasi FFmpeg
└── waveform-cache/   # Pembuatan data peak waveform dan tile spectrogram secara bertahap


Prinsip Teknis Utama

Real-Time Audio Deterministik: Audio callback bebas dari alokasi memori tak terkendali dan I/O disk. Proses decode dan render DSP berjalan secara asinkron di worker thread.

Cache Independen: Data waveform dan spectrogram disimpan sebagai cache terpisah yang dapat dibangun ulang kapan saja tanpa memengaruhi integritas file sesi.

🗺️ Roadmap Pengembangan

[X] Tahap 0: Validasi Fondasi — Prototipe waveform timeline, playback satu sumber, pengujian latensi cpal lintas OS, dan struktur format sesi.

[X] Tahap 1: Editor Multi-track MVP — Impor multi-track WAV, playback sinkron, edit dasar (split, trim, fade, marker), undo/redo, simpan/buka sesi, dan ekspor WAV.

[ ] Tahap 2: Restorasi Inti — Sistem Preview A/B & Bypass, Denoise, Gate/Expander, Hum/Rumble Filter, meter level, dan normalisasi ekspor.

[ ] Tahap 3: Restorasi Detail — Tampilan Spectrogram, De-click, De-clip, De-esser, pengurangan gema ringan, dan otomasi gain.

[ ] Tahap 4: Bleed Reduction — Penetapan track referensi, analisis alignment & korelasi, serta pengurangan bocoran konservatif.

[ ] Tahap 5: Workflow Produksi — Preset sesi podcast, batch processing, konsolidasi aset proyek, dan penyempurnaan aksesibilitas.

🎯 Batasan Scope (Non-Goals MVP)

Untuk menjaga fokus pada kecepatan dan keandalan restorasi dialog, versi awal tidak ditujukan untuk:

Menggantikan DAW produksi musik skala penuh (MIDI, instrumen virtual, dll.).

Pemisahan sumber suara berbasis AI (AI source separation) atau pemrosesan berbasis cloud.

Penyuntingan visual/gambar video (video hanya didukung untuk ekstraksi dan muxing ulang audio).

📄 Lisensi & Kontribusi

Informasi mengenai lisensi proyek, panduan kontribusi, dan cara kompilasi dari source code akan ditambahkan setelah Tahap 0 (Validasi Fondasi) selesai.
