# iZotay v0.2.1 — Preview

Perbaikan respons editor setelah umpan balik penggunaan iZotay v0.2.0.

## Perubahan

- Preview video hanya memuat frame setelah playhead berhenti 300 ms. Render frame dijeda selama playback, memakai resolusi 480×270 dan menyimpan frame yang baru dikunjungi agar scrubbing tidak memicu proses FFmpeg berulang.
- Equalizer 6-band kini selalu terlihat di Inspector. Equalizer mulai aktif dengan kurva datar, menyediakan preset **Dialog jelas** dan **Hangat**, serta tetap mendukung drag dan slider per band.
- Render mix EQ menunggu 180 ms setelah perubahan terakhir saat slider digeser agar cache track tidak dibuat ulang berkali-kali.
- Panduan penggunaan dan batasan preview telah diperbarui.

## Catatan

- FFmpeg tetap dibutuhkan untuk preview gambar dan fitur media non-WAV/video.
- Preview video masih berupa still frame, bukan monitor real-time. macOS preview belum ditandatangani atau notarized.
- Hasil EQ bergantung pada sumber; dengarkan dengan A/B dan sesuaikan preset sebelum ekspor.
