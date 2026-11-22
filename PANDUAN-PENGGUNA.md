# Panduan Pengguna iZotay

iZotay adalah editor desktop lokal untuk menyunting dialog, podcast, wawancara, voice-over, dan audio dari video. Proses edit bersifat non-destruktif: media sumber tidak ditimpa. Simpan pekerjaan sebagai sesi `.izotay`, lalu ekspor hasilnya ke berkas baru.

## 1. Persiapan

- Jalankan iZotay pada sesi desktop aktif dengan perangkat output audio yang berfungsi.
- WAV dapat dibuka langsung. Pasang FFmpeg dan pastikan perintah `ffmpeg` tersedia di `PATH` untuk format audio/video lain, denoise, batch processing, dan ekspor MP4.
- Paket GitHub menyediakan arsip portable dan installer preview untuk Windows/Linux/macOS. Lihat [Panduan Paket](PANDUAN-PAKET.md) untuk cara memasangnya.

## 2. Mulai sesi

1. Pilih **Impor media** atau tekan `Ctrl/Cmd+I`.
2. Pilih satu atau beberapa berkas audio/video yang didukung. Setiap berkas muncul sebagai track sendiri; audio dari video menjadi waveform yang dapat diedit.
3. Pilih klip untuk menampilkan pengaturan klip di panel **Inspector**. Pilih track untuk mengatur level dan pemrosesan track.
4. Simpan sesi dengan **Simpan sesi** atau `Ctrl/Cmd+S`. Sesi menyimpan referensi lokasi media dan instruksi edit; sesi biasa tidak menyalin media.

Jika media sumber dipindah atau dihapus, track akan berstatus offline. Gunakan **Tautkan ulang…** pada track itu untuk memilih berkas sumber yang benar. Untuk memindahkan proyek ke komputer atau folder lain, gunakan **Proyek → Konsolidasikan aset ke folder…** agar media disalin ke folder proyek bersama sesi.

## 3. Mengenali area kerja

- **Toolbar atas:** impor/buka/simpan sesi dan ekspor mix. Menu **Sunting**, **Klip**, **Track**, **Marker**, **Efek**, **Proyek**, **Aksesibilitas**, **Batch**, dan **Ekspor** berisi perintah tambahan.
- **Timeline:** setiap track memiliki lane dan klip. Header ruler menunjukkan waktu. Klip video ditandai `VIDEO · AUDIO`; preview gambar video belum tersedia.
- **Inspector kanan:** pengaturan klip atau track yang sedang dipilih, termasuk gain, fade, pan, dan otomasi.
- **Transport bawah:** posisi playhead, seek, putar/jeda, stop, dan meter master.
- **Waveform mix:** ringkasan output mix di bawah daftar track.

## 4. Navigasi dan edit timeline

- Klik lane untuk memindahkan playhead. Pegangan kuning pada ruler dapat diseret untuk menggeser playhead; posisi ini juga menjadi titik split dan otomasi.
- Seret area kosong pada lane untuk membuat seleksi waktu. Di dalam klip, seret klip untuk memindahkannya. Tahan `Shift` sambil menyeret klip untuk membuat seleksi waktu.
- Klik klip untuk memilihnya. Pengaturan **Gain klip**, **Fade in**, **Fade out**, dan **Crossfade** muncul di Inspector.
- `Space` memutar atau menjeda. Gunakan **Klip → Split di playhead** atau `S` untuk membagi klip pada playhead.
- Menu **Klip** menyediakan duplikasi (`Ctrl/Cmd+D`), trim kiri/kanan 0,1 detik, dan hapus klip. Menu **Sunting** menyediakan undo (`Ctrl/Cmd+Z`) dan redo (`Ctrl/Cmd+Y`).
- Gunakan menu **Marker** untuk menambah marker pada playhead, memilih marker di timeline, atau menghapus marker terpilih.
- **Zoom**, tinggi track, tampilan spectrogram, **Snap**, dan grid di toolbar timeline mengatur cara timeline ditampilkan dan bagaimana klip mengikuti titik waktu.
- Seleksi waktu dapat diputar berulang dengan **Loop seleksi**. **Ekspor seleksi saja** membatasi rentang ekspor. Pilihan **Efek di seleksi saja** hanya membatasi preview restorasi; ekspor tetap merender efek pada seluruh rentang ekspor.

## 5. Mengatur track

1. Pilih nama/klip track atau tombol **Pilih** di lane.
2. Di Inspector, ubah **Gain track**, **Pan**, **Mute**, atau **Solo**.
3. Buka **Equalizer 6-band** di Inspector untuk membentuk tonalitas track. Seret titik pada kurva, atau pilih band lalu atur frekuensi, gain, dan Q dengan slider. Gunakan bypass untuk membandingkan sinyal asli.
4. Buka bagian **Restorasi** pada lane track untuk kontrol efek. Perubahan diterapkan ke playback dan ekspor tanpa mengubah file sumber.
5. Gain otomasi ditambahkan pada waktu playhead. Tambah titik, atur nilai gain, dan ulangi untuk membentuk kurva. Titik mengikuti waktu timeline dengan interpolasi linear.

Jika track berasal dari video, Inspector menampilkan frame preview yang mengikuti playhead (diperbarui dua kali per detik saat digeser/diputar). FFmpeg harus tersedia di `PATH`; preview hanya untuk melihat gambar dan tidak mengubah video sumber.

### Kontrol restorasi

- **High-pass rumble:** kurangi getaran frekuensi rendah; pilih cutoff secukupnya agar suara dasar tidak menipis.
- **Hum 50/60 Hz:** notch untuk dengung listrik pada frekuensi yang dipilih. Dengarkan hasil karena sebagian suara yang diinginkan bisa berada di sekitar frekuensi tersebut.
- **Expander:** menurunkan noise di sela dialog. Ambang yang terlalu tinggi dapat memotong kata pelan.
- **Denoise:** reduksi noise FFmpeg. Bila bagian terpilih memuat setidaknya 0,5 detik noise saja, gunakan **Ambil profil noise dari seleksi**. Pilih bagian tanpa dialog atau musik. Tanpa profil, filter bekerja adaptif.
- **De-click / De-clip:** rekonstruksi transien klik dan clipping terisolasi. Hasilnya paling baik untuk kerusakan singkat.
- **De-esser:** kendalikan energi sibilan berlebih.
- **De-reverb:** tekan tail reverb yang pelan; setelan kuat dapat ikut menurunkan dialog pelan.
- **Equalizer 6-band:** aktifkan band yang diinginkan. Seret titik pada kurva frekuensi logaritmik untuk mengubah frekuensi dan gain; slider memberi kontrol yang lebih presisi. Gain bergerak dari −18 hingga +18 dB, Q dari 0,25 (lebar) hingga 8 (sempit). Band dapat dipilih, dinonaktifkan, atau di-bypass bersama. Boost besar dapat membuat output mencapai clipping; kurangi gain track/master bila perlu.
- **Kurangi bleed:** kurangi suara yang berkorelasi dengan track referensi. Pilih referensi yang tepat, atur reduksi/offset, lalu gunakan **Align otomatis** bila perlu. Bahan audio utama yang berkorelasi juga akan ikut berkurang.

Gunakan bypass untuk membandingkan efek, dan dengarkan dengan headphone/speaker yang wajar sebelum mengekspor hasil penting. Pemrosesan restorasi bukan pengganti pemeriksaan telinga.

## 6. Perbandingan A/B dan preset

- Dari **Efek → Simpan set A**, ambil snapshot efek track sebagai pembanding.
- Pilih **Bandingkan A/B** untuk berganti antara set A dan pengaturan aktif B.
- **Simpan preset dari track…** membuat berkas `.izopreset`; **Terapkan preset ke track…** menerapkannya ke track terpilih.
- Preset dapat dipakai untuk batch. Batch memproses banyak berkas satu per satu dengan preset dan menghasilkan WAV PCM 24-bit per sumber. Batch tidak menggabungkan sumber ke satu timeline.

## 7. Ekspor

- **Ekspor mix…** membuat berkas mix baru. Pada menu **Ekspor**, atur sample rate (ikuti sumber, 44,1/48/96/192 kHz), PCM 16/24/32-bit, normalisasi peak −1 dBFS, dan ekspor seleksi saja.
- **Ekspor track terpilih…** membuat WAV baru dari track beserta efek aktif.
- Untuk track yang berasal dari video, pilih **Buat MP4 dengan audio hasil edit…**. Video sumber dipertahankan dan audio hasil edit dimux ke berkas MP4 baru.
- Ekspor menerapkan efek aktif. File sumber tetap utuh.

Normalisasi mengatur peak sample, bukan loudness terintegrasi LUFS. Periksa hasil ekspor sebelum publikasi.

## 8. Sesi, cache, dan pemecahan masalah

- **Simpan sesi** menyimpan klip, marker, efek, otomasi, referensi bleed, dan preferensi proyek ke `.izotay`. Format sesi lama tetap dibaca oleh versi yang lebih baru.
- Cache waveform/DSP bersifat sementara dan bisa dibangun ulang. Gunakan berkas sesi dan media sebagai data proyek utama.
- Jika format non-WAV gagal dibuka atau tombol denoise/MP4 tidak bekerja, pastikan FFmpeg terpasang dan terdeteksi dengan `ffmpeg -version`.
- Jika tidak ada suara saat playback, periksa output audio sistem, mute/solo track, level track, dan peak master.
- Jika sumber offline, tautkan ulang atau buka sesi dari komputer tempat sumber berada. Konsolidasikan aset sebelum memindahkan proyek.
- Jika hasil edit mengalami clipping, turunkan gain track atau klip sampai meter master tidak mencapai batas.

## 9. Pintasan

| Pintasan | Aksi |
| --- | --- |
| `Space` | Putar/jeda |
| `S` | Split klip terpilih pada playhead |
| `Ctrl/Cmd+I` | Impor media |
| `Ctrl/Cmd+O` | Buka sesi |
| `Ctrl/Cmd+S` | Simpan sesi |
| `Ctrl/Cmd+Z` | Undo |
| `Ctrl/Cmd+Y` | Redo |
| `Ctrl/Cmd+D` | Duplikasi klip |
| `Shift` + seret klip | Buat seleksi waktu alih-alih memindahkan klip |

## 10. Batasan preview

iZotay masih prototipe/preview. Preview video hanya still frame 640×360 hingga dua kali per detik, bukan monitor video real-time. Perangkat output, latensi, pembaca layar, lisensi dependency, kualitas restorasi pada rekaman produksi, serta penandatanganan/notarization macOS perlu diuji sebelum penggunaan produksi.
