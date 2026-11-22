# Rancangan Editor Restorasi Audio Multi-track

**Status:** Rancangan produk awal  
**Platform:** Windows, macOS, Linux  
**Fokus:** Podcast, wawancara, voice-over, dan dialog  
**Pemrosesan:** Lokal di perangkat pengguna

## 1. Ringkasan

Produk ini adalah editor audio desktop multi-track untuk menyunting dialog dan memperbaiki masalah rekaman. Pengguna dapat mengimpor beberapa mikrofon, menyelaraskan dan menyunting klip, membersihkan suara, mengurangi bleed dengan bantuan track referensi, lalu mengekspor track atau mix akhir.

Prinsip utamanya adalah membuat perbaikan mudah dicoba, mudah dibandingkan, dan mudah dibatalkan. File sumber tidak pernah ditimpa. Semua perubahan disimpan sebagai instruksi edit di dalam sesi; ekspor membuat file baru.

Aplikasi dirancang sebagai perangkat lunak restorasi dialog yang bertahap. Tahap awal harus berguna untuk produksi podcast biasa, sedangkan fitur restorasi lebih canggih dibangun di atas model sesi dan audio engine yang sama.

## 2. Tujuan produk

### 2.1 Tujuan

- Memudahkan pengguna menemukan, memilih, dan memperbaiki bagian audio bermasalah.
- Mendukung workflow banyak mikrofon dan banyak track sejak fondasi.
- Menjaga hasil edit non-destruktif, dapat dipreview, dibandingkan, dan dibatalkan.
- Menjalankan pemrosesan audio secara lokal tanpa mengharuskan audio diunggah.
- Menyediakan jalur dari edit sederhana sampai kontrol restorasi yang lebih rinci.
- Menghasilkan file ekspor yang dapat dipakai pada workflow podcast umum.

### 2.2 Bukan tujuan MVP

- Menggantikan DAW profesional untuk produksi musik kompleks.
- Menjanjikan pemisahan sumber yang sempurna dari satu rekaman mono.
- Menyediakan model AI atau pemrosesan cloud.
- Mendukung semua format media dan semua plugin pihak ketiga sejak versi pertama.
- Mengubah atau menimpa file sumber secara langsung.

## 3. Pengguna dan kasus penggunaan

### 3.1 Pengguna utama

- Podcaster yang merekam host dan tamu ke mikrofon terpisah.
- Editor dialog yang membersihkan wawancara atau voice-over.
- Kreator mandiri yang memerlukan edit dasar dan restorasi tanpa harus belajar DAW besar.

### 3.2 Kasus penggunaan utama

1. Mengimpor rekaman tiap mikrofon dan menyelaraskannya.
2. Menghapus jeda, salah ucap, dan bagian yang tidak diinginkan.
3. Mengurangi kipas, AC, hum listrik, rumble, klik, atau desis.
4. Mengurangi suara host yang bocor ke mikrofon tamu, atau sebaliknya.
5. Menyamakan level dialog dan mengekspor mix serta track terpisah.
6. Membandingkan hasil restorasi dengan sumber sebelum memutuskan menerapkannya.

## 4. Prinsip pengalaman pengguna

- **Sumber aman:** media asli hanya dibaca; semua render disimpan sebagai output baru.
- **Preview sebelum komit:** alat restorasi menyediakan preview pada rentang yang dipilih.
- **Perbandingan mudah:** bypass dan A/B tersedia tanpa menutup panel alat.
- **Bahasa berdasarkan tugas:** panel utama mengelompokkan alat berdasarkan masalah, bukan nama algoritma.
- **Kontrol bertingkat:** pengguna awam melihat kontrol inti; parameter lanjutan dapat dibuka.
- **Konteks jelas:** pengguna tahu apakah perubahan memengaruhi klip, track, seleksi, atau seluruh mix.
- **Responsif:** playback tidak tersendat karena waveform, spectrogram, atau render sedang dihitung.
- **Dapat dipulihkan:** undo/redo, autosave sesi, dan pemulihan setelah crash menjadi bagian rancangan.

## 5. Model objek sesi

### 5.1 Sesi

Sesi menyimpan:

- ID dan versi format sesi.
- Sample rate kerja dan pengaturan tampilan.
- Track, klip, grup, bus, marker, dan routing.
- Rangkaian edit dan efek.
- Referensi media sumber dan metadata yang diperlukan.
- Pengaturan ekspor terakhir.

Sesi tidak menyalin sumber secara otomatis. Saat media dipindah atau hilang, aplikasi memberi status media offline dan menyediakan relink. Opsi konsolidasi media dapat ditambahkan untuk memindahkan proyek dengan seluruh asetnya.

### 5.2 Track

Track adalah jalur logis seperti “Host”, “Tamu”, “Mic Boom”, atau “Room”. Properti dasar:

- Nama, warna, urutan, dan tipe track.
- Volume/gain, pan, mute, solo, dan arm untuk fitur rekam masa depan.
- Routing ke master atau bus.
- Daftar klip dan automation.
- Rangkaian efek track.
- Penetapan fungsi opsional: sumber utama atau referensi bleed.

### 5.3 Klip

Klip adalah rentang media yang diletakkan pada timeline. Properti:

- Referensi sumber, titik awal dan akhir sumber, posisi timeline.
- Gain klip, fade masuk/keluar, dan batas edit.
- Daftar efek klip.
- Tautan stereo/kanal bila berlaku.
- Metadata waveform cache dan status render.

Klip dapat dipindah, dipotong, split, diduplikasi, dihapus, dan diseret di antara track. Split tidak memodifikasi sumber; ia membentuk dua referensi rentang media.

### 5.4 Grup dan bus

Grup membantu mengelola track terkait, misalnya semua mikrofon. Bus menerima beberapa track untuk pemrosesan atau kontrol bersama. Dalam MVP, grup dapat berfungsi sebagai pengelompokan visual dan bus mix sederhana menjadi tujuan routing. Automasi dan routing kompleks menyusul.

### 5.5 Edit dan riwayat

Setiap tindakan edit direpresentasikan sebagai operasi yang dapat dibalik. Undo/redo berlaku berurutan dan mencakup edit timeline serta perubahan parameter efek. Riwayat sesi menyimpan checkpoint untuk pemulihan; pengguna tidak perlu memahami detail implementasi operasi.

## 6. Ruang kerja dan interaksi

### 6.1 Area utama

- **Toolbar:** buka/simpan sesi, undo/redo, alat seleksi, snap, zoom, dan ekspor.
- **Timeline:** track, klip, waveform, playhead, seleksi, marker, dan ruler waktu.
- **Transport:** play, stop, kembali ke awal, loop seleksi, dan indikator posisi.
- **Inspector:** properti track atau klip yang sedang dipilih.
- **Panel restorasi:** alat menurut kategori masalah.
- **Meter:** level tiap track dan master, dengan indikasi clipping.

### 6.2 Navigasi timeline

- Zoom horizontal berpusat pada playhead atau seleksi.
- Scroll horizontal dan vertikal tidak saling membingungkan.
- Tinggi track dapat diubah dan track dapat diciutkan.
- Pengguna dapat memilih klip, rentang waktu, atau rentang waktu lintas track.
- Snap dapat diarahkan ke playhead, batas klip, marker, atau grid waktu.
- Loop playback mengulang seleksi untuk mendengar artefak atau hasil proses.

### 6.3 Tampilan waveform dan spectrogram

Waveform menjadi tampilan awal karena paling mudah dipahami untuk dialog. Spectrogram tersedia sebagai tampilan alternatif atau panel detail. Spectrogram mendukung zoom frekuensi/waktu dan palet kontras yang terbaca. Pengguna bisa memilih rentang waktu; seleksi frekuensi lanjutan direncanakan setelah fondasi seleksi stabil.

### 6.4 Preview dan A/B

Panel alat menyediakan:

- Preview seleksi dengan loop opsional.
- Tombol bypass untuk membandingkan sumber dan hasil.
- A/B state untuk membandingkan dua set parameter.
- Kontrol intensitas utama yang mudah dipahami.
- Tombol Apply dan Cancel.
- Pesan jika preview belum siap atau parameter menimbulkan risiko artefak.

Preview tidak boleh mengubah sesi. Apply menambahkan operasi non-destruktif yang dapat di-undo.

## 7. Fitur produk

### 7.1 Manajemen media dan sesi

- Membuat, membuka, menyimpan, dan autosave sesi.
- Impor beberapa file audio sekaligus, termasuk ekstraksi audio dari video untuk disunting sebagai track. Preview gambar video dan penyuntingan gambar tidak termasuk scope.
- Menempatkan file sebagai track baru atau klip pada track terpilih.
- Relink media yang hilang.
- Menampilkan durasi, sample rate, jumlah kanal, dan format sumber.
- Menangani file panjang melalui pembacaan bertahap dan cache.

### 7.2 Edit dialog

- Seleksi waktu dan seleksi lintas track.
- Trim, split, pindah, hapus, duplikasi, dan gabung klip.
- Fade in/out dan crossfade sederhana.
- Gain klip dan track.
- Mute, solo, pan, serta kontrol master.
- Marker untuk catatan, bagian penting, dan titik edit.
- Undo/redo dan autosave.

### 7.3 Playback dan mix

- Playback multi-track tersinkronisasi.
- Monitor level track dan master.
- Loop seleksi.
- Mixdown real-time untuk mendengarkan sesi.
- Indikator clipping dan overload.
- Playback tidak menghentikan editing selama proses latar belakang berlangsung.

### 7.4 Restorasi dialog

**Denoise:** mengambil profil dari bagian noise-only atau memakai estimasi adaptif sederhana. Kontrol utama: kekuatan, pelestarian suara, dan preview. Tujuannya mengurangi noise stabil; bukan menjanjikan penghapusan total tanpa artefak.

**Gate/expander:** mengurangi level ketika dialog berhenti. Kontrol utama: threshold, range, attack, hold, dan release; mode sederhana menyajikan threshold dan kekuatan saja. Perlu menghindari pemotongan konsonan pelan dan akhir kata.

**Hum removal:** menargetkan dengung periodik dan harmonik. Pengguna memilih frekuensi dasar atau memakai deteksi awal yang dapat dikoreksi.

**Filter rumble:** high-pass dengan frekuensi cutoff dan kemiringan yang aman untuk suara. Preset membantu pengguna memulai tanpa menghilangkan bobot suara.

**De-click:** mendeteksi transien singkat dan mengganti atau menginterpolasi bagian kecil. Preview wajib untuk menghindari suara konsonan yang ikut dihapus.

**De-clip:** memperbaiki puncak terpotong secara konservatif. Alat menandai bagian dengan clipping dan memberi preview karena detail yang hilang mungkin tidak dapat dipulihkan sepenuhnya.

**De-esser:** mengurangi energi sibilan pada rentang frekuensi tertentu sambil menjaga kejelasan artikulasi.

**Pengurangan gema:** mengurangi komponen reverberasi secara ringan; kontrol utama berupa jumlah pengurangan dan pelestarian suara langsung. Hasil bergantung pada rekaman dan perlu diperiksa dengan preview.

**Level dialog:** gain, kompresi sederhana, normalisasi peak/loudness, dan limiter ringan untuk ekspor. Pengaturan target harus ditampilkan secara eksplisit dan tidak mengubah sumber.

### 7.5 Ekspor

- Ekspor mix sesi.
- Ekspor track terpilih secara terpisah.
- Untuk track yang berasal dari video, buat MP4 baru dengan stream video asli dan audio hasil edit.
- Format awal: WAV PCM dengan pilihan sample rate, bit depth, dan kanal yang didukung implementasi.
- Ekstraksi dan mux audio video memakai FFmpeg lokal; sumber video tidak ditimpa.
- Pilihan rentang ekspor: seluruh sesi atau rentang seleksi.
- Nama file, lokasi, dan ringkasan pengaturan sebelum render.
- Render ke file baru, dengan progres, pembatalan, dan pesan jika disk tidak cukup atau render gagal.
- MP3/AAC dapat ditambahkan setelah format dan dependensi lisensinya diputuskan.

## 8. Rancangan bleed reduction

### 8.1 Definisi masalah

Bleed adalah suara yang diinginkan dari satu sumber ikut terekam pada mikrofon lain. Contohnya suara tamu masuk ke track host. Perbedaan jarak mikrofon dan pantulan ruangan membuat sinyal bocor memiliki waktu, level, dan spektrum yang berbeda dari track sumber.

### 8.2 Mode tanpa referensi

Jika tidak ada track referensi, aplikasi menyediakan pengurangan yang bekerja berdasarkan jeda dan karakter noise: gate/expander, automation volume, dan denoise. Ini membantu ketika suara bocor terjadi saat track utama diam atau suara latar stabil. Mode ini bukan pemisahan dua orang yang berbicara bersamaan.

### 8.3 Mode dengan track referensi

Alur:

1. Pilih track target dan track referensi.
2. Tentukan rentang yang memiliki sumber suara referensi dengan sedikit atau tanpa suara target.
3. Analisis korelasi dan keterlambatan waktu awal.
4. Tawarkan koreksi alignment yang dapat didengar dan dibatalkan.
5. Pratinjau pengurangan dengan kontrol kekuatan.
6. Terapkan sebagai efek track/klip yang tetap dapat diedit.

Desain DSP awal dapat menggunakan pembatalan adaptif berbasis referensi, dengan alignment waktu dan estimasi hubungan sinyal. Implementasi harus membatasi pengurangan saat kedua pembicara aktif bersamaan agar suara utama tidak ikut hilang. Kontrol pengguna harus memperlihatkan intensitas, area yang terdampak, serta bypass. Jika sinyal referensi tidak cukup berkorelasi atau alignment buruk, aplikasi menjelaskan bahwa referensi itu tidak cocok dan tidak menerapkan pemrosesan diam-diam.

### 8.4 Batas hasil

Bleed reduction paling efektif bila track sumber referensi bersih, sinkron, dan menangkap suara yang bocor dengan hubungan yang dapat diperkirakan. Jika dua orang berbicara bersamaan dan karakter bocornya berubah-ubah, pengurangan penuh mungkin tidak mungkin dilakukan dengan metode lokal sederhana. UX harus mendorong preview dan pengaturan konservatif, bukan menjanjikan isolasi sempurna.

## 9. Arsitektur perangkat lunak Rust

Arsitektur dipisahkan agar UI, sesi, DSP, dan perangkat audio tidak saling mengunci.

### 9.1 Paket konseptual

- **`app-ui`:** jendela, panel, input pengguna, dan visualisasi.
- **`session-core`:** model proyek, track/klip, edit, undo/redo, autosave, dan serialisasi.
- **`audio-engine`:** transport, sinkronisasi track, mixing, routing, dan audio callback.
- **`dsp`:** algoritma restorasi dan utilitas analisis.
- **`media-io`:** decode, encode, metadata, dan manajemen cache.
- **`waveform-cache`:** pembuatan peak data dan tile spectrogram secara bertahap.

Paket dapat berupa crates dalam satu workspace Rust. Batas crate adalah panduan desain; susunan final mengikuti prototipe teknis dan dependensi terpilih.

### 9.2 Kandidat teknologi

- UI desktop: `egui`/`eframe` untuk prototipe lintas platform.
- Audio I/O: `cpal` sebagai kandidat akses perangkat audio.
- Decode/encode: `symphonia` sebagai kandidat pembacaan format; dukungan ekspor dipilih setelah verifikasi format dan lisensi.
- FFmpeg dapat dipakai sebagai executable lokal untuk ekstraksi dan mux audio video; distribusi perlu menjelaskan dependensi dan lisensinya.
- FFT: `rustfft` untuk transformasi spektrum.
- Serialisasi sesi: format versi yang dapat dimigrasikan, misalnya JSON/TOML untuk metadata dan file media eksternal.

Pemilihan akhir harus mempertimbangkan lisensi, kualitas dukungan format, performa, stabilitas lintas platform, dan kebutuhan paket distribusi. Dependency tidak dianggap final hanya berdasarkan nama crate.

### 9.3 Audio real-time

Audio callback harus tetap ringan, deterministik, dan bebas operasi berat seperti alokasi tak terkendali, pembacaan file lambat, atau pemrosesan restorasi panjang. Decode dan DSP offline berjalan di worker thread. Engine mengonsumsi buffer yang sudah siap, menjaga posisi semua track, dan menangani underrun dengan aman.

### 9.4 Pemrosesan offline dan cache

- Decode media dapat dilakukan bertahap.
- Peak waveform dan tile spectrogram disimpan dalam cache yang dapat dibangun ulang.
- Preview DSP menggunakan rentang seleksi dan buffer terpisah dari sumber.
- Render penuh berjalan di latar belakang dan dapat dibatalkan.
- Cache tidak menjadi sumber kebenaran sesi; jika hilang, aplikasi dapat membuat ulang.

### 9.5 Format sesi

Format sesi harus memiliki nomor versi, ID media yang stabil, referensi path relatif bila memungkinkan, serta mekanisme migrasi. Save dilakukan secara aman melalui file sementara dan penggantian atomik bila platform mendukung. Autosave memiliki checkpoint untuk memulihkan sesi setelah aplikasi berhenti tidak terduga.

## 10. Kualitas, aksesibilitas, dan keandalan

- Shortcut keyboard untuk play/stop, split, undo/redo, zoom, dan navigasi seleksi.
- Kontras waveform dan status track yang dapat dibedakan tanpa hanya mengandalkan warna.
- Label dan tooltip untuk kontrol DSP.
- Pengaturan skala UI dan ukuran track.
- Pesan error menyebut tindakan yang bisa dilakukan pengguna.
- Proyek yang rusak atau media hilang tidak boleh menghapus sumber.
- Aplikasi menunjukkan status pemrosesan, sisa pekerjaan, dan opsi membatalkan.
- Pemulihan autosave tidak boleh diam-diam menimpa versi sesi yang disimpan.

## 11. Roadmap dan kriteria selesai

### Tahap 0: validasi fondasi

- Prototipe waveform timeline dan playback satu sumber.
- Uji pemilihan crate, lisensi, latency, dan packaging dasar pada Windows, macOS, Linux.
- Tentukan format sesi awal dan cara menangani sumber eksternal.

**Selesai bila:** keputusan teknologi utama terdokumentasi dan prototipe bisa membuka audio, menampilkan waveform, dan memutarnya pada platform sasaran.

### Tahap 1: editor multi-track MVP

- Sesi dengan banyak track dan klip.
- Impor WAV, waveform, playback sinkron, mute/solo, gain/pan.
- Seleksi, split, trim, pindah klip, marker, undo/redo.
- Simpan/buka sesi, autosave dasar, relink media.
- Ekspor WAV mix dan track.

**Selesai bila:** pengguna dapat mengimpor rekaman host/tamu, menyelaraskan secara manual, mengedit jeda, menyimpan sesi, membuka kembali, dan mengekspor tanpa mengubah sumber.

### Tahap 2: restorasi inti

- Preview A/B dan bypass.
- Denoise sederhana, gate/expander, hum removal, filter rumble.
- Meter track/master dan normalisasi ekspor.
- Render latar belakang dan cache.

**Selesai bila:** alat dapat dipreview pada seleksi, hasil dapat di-undo, dan playback tetap berjalan saat operasi berat berlangsung.

### Tahap 3: restorasi detail

- Spectrogram dan navigasi gangguan.
- De-click, de-clip, de-esser, pengurangan gema awal.
- Automation gain dan workflow perbaikan klip.

**Selesai bila:** pengguna dapat menemukan gangguan, memprosesnya secara selektif, dan membandingkan hasil tanpa merender seluruh sesi untuk setiap percobaan.

### Tahap 4: bleed reduction

- Penetapan track referensi.
- Analisis alignment dan korelasi.
- Preview pengurangan konservatif dan kontrol kekuatan.
- Peringatan kualitas referensi dan bypass mudah.

**Selesai bila:** tool memberi hasil yang dapat dinilai melalui A/B pada dataset rekaman multi-mikrofon, menghindari perubahan destruktif, dan gagal dengan pesan yang jelas ketika referensi tidak layak.

### Tahap 5: workflow produksi

- Preset sesi podcast.
- Batch processing.
- Preset ekspor dan ekspor stem/track.
- Konsolidasi proyek dan pengelolaan aset.
- Penyempurnaan shortcut, onboarding, dan aksesibilitas.

**Selesai bila:** pengguna dapat mengulang workflow produksi yang konsisten, memindahkan proyek, dan mengekspor format yang ditetapkan produk.

## 12. Risiko dan keputusan yang perlu divalidasi

- Dukungan perangkat audio dan latency dapat berbeda antar sistem operasi.
- Performa proyek multi-track bergantung pada sample rate, jumlah track, panjang sesi, dan perangkat.
- Format ekspor tertentu mungkin memerlukan dependensi tambahan dengan lisensi berbeda.
- Denoise, de-reverb, dan bleed reduction dapat menimbulkan artefak; preview bukan sekadar kenyamanan, tetapi bagian inti workflow.
- Alignment dan hubungan fase antar mikrofon memengaruhi hasil bleed reduction.
- Format sesi perlu dirancang agar bisa dimigrasikan ketika fitur baru ditambahkan.
- Nama, ikon, dan tata letak final perlu diuji dengan pengguna podcast sebelum dibekukan.

## 13. Keputusan produk saat ini

- Aplikasi desktop lintas platform.
- Fokus awal pada dialog, podcast, wawancara, dan voice-over.
- Semua pemrosesan berjalan lokal.
- Multi-track adalah fondasi MVP.
- Edit bersifat non-destruktif dan sumber tidak ditimpa.
- MVP mendukung WAV dan audio dari video; ekspor utama WAV serta MP4 baru untuk track audio yang berasal dari video.
- Restorasi dimulai dari alat sederhana yang dapat dipreview.
- Bleed reduction memakai track referensi bila tersedia; tanpa referensi, aplikasi menawarkan kontrol jeda/noise dan tidak menjanjikan pemisahan suara bertumpuk.
