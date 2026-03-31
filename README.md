# ⚡ Matrix Telemetry (Rust)

Rust ile geliştirilmiş, çok çekirdekli işlemci desteğine sahip yüksek performanslı Matrix yağmuru ve sistem monitörü.

## ✨ Özellikler
- **Performans:** `Rayon` ile her sütun farklı bir thread'de hesaplanır.
- **Görsel:** `Ratatui` ile 60+ FPS akıcı görüntü.
- **Telemetri:** `sysinfo` ile gerçek zamanlı CPU sıcaklık ve RAM takibi.
- **Dinamik Renk:** Sistem ısındıkça yağmur rengi yeşilden kırmızıya döner.

## 🚀 Çalıştırma
```bash
cargo run --release
