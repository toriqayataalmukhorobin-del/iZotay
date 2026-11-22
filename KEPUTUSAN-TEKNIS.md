# Editor Multi-track — Keputusan Teknis

Status: implementasi Tahap 3 selesai: spectrogram timeline, perbaikan klik/clipping terisolasi, de-esser, penekan tail reverb, dan otomasi gain tersimpan dalam sesi versi 15. Tahap ini menambah kemampuan prototipe; validasi hasil pada rekaman dialog nyata serta Windows/macOS masih diperlukan sebelum rilis.

## Pilihan prototipe

- **UI:** `egui`/`eframe` untuk jendela desktop native lintas platform.
- **Output audio:** `cpal` memakai perangkat output default; callback mencampur frame dari sumber yang sudah didekode dan diproses, tanpa alokasi memori.
- **Media:** `hound` untuk WAV; FFmpeg lokal untuk mengekstrak audio dari video dan membaca format audio lain.
- **Penyimpanan sumber:** hasil decode ditulis bertahap sebagai PCM float ke cache sementara, lalu dibaca lewat pemetaan file read-only; cache dikenali lewat path dan metadata sumber.
- **Pemilih file:** `rfd` untuk dialog native.
- **Sesi:** JSON berversi dengan `serde`; sumber media direferensikan lewat path, bukan disalin ke sesi.
- **Struktur:** satu binary untuk validasi awal; pemisahan crate dilakukan setelah kontrak model sesi dan engine stabil.

## Kemampuan prototipe saat ini

- Impor beberapa berkas audio atau video sebagai track; audio video ditampilkan sebagai waveform tanpa mengubah gambar atau sumber aslinya. Nama track dapat diubah, urutannya dapat dipindah, dan waveform, mute/solo, gain, serta pan tersedia.
- Timeline dasar dengan playhead, pindah/duplikasi klip, split pada playhead, trim awal/akhir, hapus klip/track, serta undo/redo sampai 100 langkah.
- Zoom timeline horizontal dapat diatur dari 25 hingga 400 piksel per detik; posisi playhead, marker, seleksi, dan drag klip mengikuti skala yang sama. Pengaturan zoom disimpan per sesi.
- Tinggi lane waveform dapat diatur dari 52 hingga 220 piksel; pilihan disimpan per sesi.
- Waveform memakai cache puncak multi-resolusi berbasis jendela 256 frame, dengan level kasar berlipat dua; cache disimpan di direktori sementara berdasarkan path, ukuran, waktu modifikasi, sample rate, dan kanal media. Level serta area gambar dipilih menurut zoom dan viewport agar klip panjang tidak menggambar titik di luar layar.
- Spectrogram memakai FFT radix-2 512 sampel, Hann window, 64 pita hingga sekitar 6 kHz pada sumber 48 kHz, dan maksimum 30.000 kolom per track. Hasil disimpan ringkas di memori saat impor/pembukaan sesi dan dapat dipilih sebagai tampilan timeline.
- Ruler waktu menampilkan label dan garis panduan dengan interval yang menyesuaikan zoom horizontal.
- Snap opsional saat memindahkan klip mengunci tepi klip ke playhead, marker, tepi klip lain, atau grid waktu 0,1/0,25/0,5/1/2 detik dalam jarak 8 piksel; pengaturan disimpan per sesi.
- Gain klip 0–4× dan fade in/out linear non-destruktif; parameter tersimpan di sesi, dapat di-undo, serta dipakai untuk playback, meter track, dan ekspor. Klip hasil split mewarisi gain.
- Otomasi gain memakai titik waktu timeline dengan rentang gain 0–4×, interpolasi linear, default 1× sebelum titik pertama, dan nilai titik terakhir setelahnya. Titik dapat ditambah pada playhead atau dihapus dari titik terdekat; garis dan titik tampak di klip, tersimpan dalam sesi, di-undo, dan dipakai playback/meter/ekspor.
- Crossfade dapat dibuat untuk klip terpilih yang overlap dengan klip setelahnya; fade out/in diselaraskan ke rentang overlap dan dapat di-undo.
- Seleksi rentang waktu lewat seret pada area kosong timeline atau Shift+seret klip; rentang dapat di-loop untuk preview dan dipakai untuk ekspor mix atau track.
- Mode preview seleksi opsional membatasi render efek pada sumber yang beririsan dengan rentang timeline terpilih; rentang lain tetap memakai audio asli dan ekspor tetap memproses efek seluruh track.
- Shortcut Space untuk playback, S untuk split, Ctrl/Cmd+D untuk duplikasi, dan Ctrl/Cmd+Z/Y untuk undo/redo.
- Marker timeline dengan label otomatis; klik marker untuk memindahkan playhead. Marker ikut tersimpan dan dipulihkan oleh undo/redo.
- Playback mencampur frame dari track yang telah diproses di callback audio dan tetap berjalan memakai mix terakhir saat render baru berlangsung; mix terbaru melanjutkan dari posisi playhead. Mix stereo/track diekspor bertahap tanpa membuat buffer output berdurasi penuh. Ekspor WAV mendukung PCM 16/24/32-bit, pilihan sample rate 44,1/48/96/192 kHz atau mengikuti sumber, normalisasi peak ke −1 dBFS dengan batas boost +40 dB, progres, dan pembatalan. File tujuan baru diganti setelah file sementara selesai ditulis.
- Track dari video dapat diekspor sebagai MP4 baru dengan audio hasil edit; stream video disalin tanpa re-encode dan audio dikodekan AAC. FFmpeg diperlukan untuk impor maupun ekspor video.
- Impor media berjalan di worker dan dapat dibatalkan; decode WAV, keluaran FFmpeg, penulisan cache sumber, serta pembuatan waveform berjalan bertahap dan memeriksa pembatalan. Track yang selesai tetap dipertahankan. Cache sampel yang cocok dipetakan ulang tanpa decode media. Filter aktif berjalan per frame dan menulis hasilnya ke cache file-backed; mix tanpa efek memakai sumber langsung tanpa salinan penuh. Pembukaan sesi, penghitungan filter/mix, dan render ekspor juga berjalan di worker latar belakang. Antrean mix hanya menyimpan permintaan terbaru dan membatalkan render lama saat pengaturan berubah.
- High-pass rumble per track dengan rentang 0–180 Hz dan bypass untuk perbandingan cepat; pemrosesan per frame memakai state filter berkelanjutan dan cache hasil.
- Hum removal per track dengan pilihan listrik 50/60 Hz, notch sempit pada fundamental dan sampai delapan harmonik, serta bypass.
- Denoise spektral per track melalui FFmpeg `afftdn`, pengurangan 0–30 dB, bypass, A/B, dan rentang profil noise dari bagian seleksi yang hanya berisi noise latar; input serta keluaran full-track dialirkan bertahap, sumber tetap utuh.
- Perbandingan A/B untuk parameter rumble, hum, expander, denoise, de-click, de-clip, de-esser, dan de-reverb per track; set A tersimpan dalam sesi sementara kontrol tetap mengedit set B.
- Expander per track dengan threshold -60 hingga -20 dBFS, bypass, penurunan level konservatif sekitar 15 dB, attack 10 ms, dan release 120 ms; envelope dipertahankan antar-frame saat hasil ditulis ke cache.
- De-click mendeteksi transien satu sampel yang menyimpang kuat dari tetangga lalu menginterpolasi nilainya; de-clip mencari plateau sampel yang mencapai ±0,985 dan menginterpolasi rentang terpotong hingga 64 frame. Keduanya non-destruktif dan ikut cache DSP.
- De-esser memakai high-pass satu kutub sekitar 5,5 kHz, envelope attack 3 ms/release 45 ms, threshold −60–0 dBFS, serta reduksi maksimum 0–18 dB. De-reverb adalah penekan tail berbasis envelope level rendah dengan release 350 ms dan intensitas 0–100%; bukan pemisah reverberasi berbasis model.
- Meter peak master live menampilkan level linear dan dBFS dari callback output; meter per track menunjukkan peak pada jendela 10 ms di sekitar playhead.
- Simpan/buka sesi `.izotay` versi 15; versi 1–14 tetap dibaca. Kolom efek Tahap 3 dan otomasi memakai default aman ketika tidak ada pada sesi lama. Gain klip baru memakai default 1×, zoom memakai nilai default, snap nonaktif, dan tinggi lane memakai nilai default pada sesi lama. Rentang profil noise, pengaturan ekspor, dan set A ikut disimpan. Sumber media direferensikan lewat path, bukan disalin ke sesi.
- Sesi tetap dapat dibuka saat sumber tidak tersedia; track ditandai offline dan dapat ditautkan ulang lewat dialog file. Klip yang melewati durasi sumber baru dipangkas ke rentang valid.
- Mix menggunakan sample rate track pertama; sumber dan playback perangkat memakai interpolasi linear saat menyamakan laju sampel.

## Batas yang diketahui

- Cache sumber, hasil filter, dan denoise disimpan sementara sebagai PCM float serta dipetakan read-only. Cache disentuh saat dipakai dan dibersihkan menurut waktu akses ketika direktori melewati batas 4 GiB; file sementara yatim berumur lebih dari 24 jam juga dibuang. Batas ukuran bersifat upaya terbaik jika satu cache aktif saja melampaui batas atau sistem menolak menghapus file yang sedang dipetakan. Cache waveform menambah sekitar 5,4 MB per jam sumber 48 kHz dan dapat dibangun ulang. Impor dan ekspor video memerlukan executable FFmpeg di PATH.
- Klip hasil impor dimulai di waktu nol; pemindahan, split, trim, dan hapus tersedia.
- Undo/redo mencakup edit dan pengelolaan track sampai 100 langkah. Setelah sesi disimpan dan diedit, autosave dibuat setelah jeda 2 detik; saat membuka sesi, autosave yang lebih baru dipulihkan ke memori tanpa menimpa file sesi.
- Ekspor track menerapkan efek yang sedang aktif. Pembatalan diperiksa selama filter dan penulisan, tetapi operasi sistem berkas saat mengganti file tidak dapat dihentikan di tengah.
- Normalisasi mengukur peak sample pada rentang ekspor, bukan loudness LUFS; boost dibatasi 40 dB agar rekaman sangat pelan tidak dinaikkan tanpa batas.
- Notch hum dapat mengurangi komponen suara yang kebetulan berada dekat harmoniknya; bypass tersedia, dan hasil tetap perlu didengar pada rekaman sasaran.
- Denoise memerlukan build FFmpeg dengan filter `afftdn`; satu rentang noise-only terpanjang dari seleksi dipakai untuk sampling profil setiap kali mix dirender, lalu pemrosesan diterapkan pada seluruh track. Expander perlu diuji pada dialog pelan sebelum dipakai di produksi.
- Spectrogram hanya visual dan belum mendukung seleksi/penghapusan frekuensi dengan kuas. De-click/de-clip menangani transien dan plateau pendek, sedangkan kasus restorasi kompleks dapat memerlukan editor khusus. De-reverb merupakan pengurangan tail berbasis level sehingga setelan terlalu kuat bisa menekan dialog pelan.
- Playback menghitung mix per frame di callback audio dan tidak menyimpan buffer mix penuh. Sumber dan hasil efek full-track memakai pemetaan cache file; daftar klip yang sangat padat dapat menambah beban callback.
- Preview gambar video belum tersedia. Loop hanya mengulang rentang waktu yang dipilih. Mode preview seleksi menyimpan hasil efek pada rentang terpilih beserta konteks 250 ms di memori agar filter stabil; seleksi yang sangat panjang dapat memakai RAM besar. Ekspor selalu merender efek pada seluruh track dan memakai cache file-backed.
- Meter live master dibaca dari callback output; meter track menampilkan peak pada jendela 10 ms di sekitar playhead.
- Batas loop belum memakai crossfade; pilih rentang dengan transisi yang bersih untuk menghindari klik.
- Relink memangkas klip yang sumbernya melebihi durasi berkas yang dipilih; kecocokan identitas media belum diverifikasi otomatis.
- Penggantian file sesi memakai file sementara dan cadangan agar dapat berjalan pada sistem yang tidak mendukung rename langsung di atas file yang ada.
- Tes lokal mencakup WAV PCM 16/24/32-bit dan float, impor FFmpeg non-WAV, alur WAV impor-filter-mix-ekspor-impor, seluruh sample rate/bit depth ekspor, resampling, roundtrip/migrasi sesi, preview/profil noise, pruning cache, kesetaraan DSP bertahap, spectrogram, empat restorasi Tahap 3, dan FFmpeg denoise bila tersedia. Uji alur UI pada rekaman dialog nyata dan hasil CI Windows/macOS masih perlu ditinjau.

## Validasi sebelum rilis

1. Uji impor, sesi, playback, preview, dan ekspor di UI pada rekaman dialog nyata, termasuk transien, noise profile, dan seleksi panjang.
2. Uji Windows/macOS, latensi audio, lisensi dependensi, serta packaging installer sebelum teknologi dianggap final.
