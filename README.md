# Bittice: High-Performance Data Engine

**Bittice** is a high-performance read-layer engine written in Rust, designed to bridge the gap between heavy transactional databases and instant data availability. 

It provides an ultra-fast, columnar-based storage model that acts as a real-time replica, offloading massive search and analytical workloads from your primary databases to save infrastructure costs and maximize performance.

> **Note:** Bittice is licensed under the **Elastic License v2.0**. It is free to use for personal, internal, and commercial purposes (e.g., to save infrastructure costs). However, you may not provide it to others as a managed service.

## 🚀 The Vision: Multi-Source Synchronization
While currently featuring a robust **MySQL** connector via CDC (Change Data Capture), Bittice is architected to be source-agnostic. Our roadmap includes native synchronization for:
*   🐘 **PostgreSQL** (Coming Soon)
*   🗄️ **SQL Server** (Coming Soon)
*   🍃 **MongoDB** (Coming Soon)

## 🦀 Technical Foundation
Bittice is built for extreme efficiency using:
*   **Rust:** For memory safety and zero-cost abstractions.
*   **Tri-File Columnar Mapping (TFCM):** Proprietary storage logic for O(1) retrieval.
*   **Memory-Mapped Files (mmap):** Leveraging the OS page cache for near-instant data access.
*   **Roaring Bitmaps:** For high-speed logical filtering across massive datasets.

---

## 🛠 For Developers: Building from Source

### Prerequisites
*   **Rust & Cargo:** Latest stable version.
*   **Protobuf Compiler:** Required for gRPC interface compilation.

### Build and Run
```bash
# Clone the repository
git clone https://github.com/julianrodelo/bittice.git
cd bittice

# Build and run the interactive CLI
cargo run
```

---

## 📜 Documentation & License
For full documentation, API reference, and installation guides, please visit our documentation portal (Coming Soon).

### License
Bittice is licensed under the **Elastic License v2.0**. 

*   **Permitted:** Free for personal use and internal production use within any organization to optimize their own data infrastructure. You can modify and redistribute the code for internal purposes.
*   **Prohibited:** You may not provide Bittice to third parties as a hosted or managed service (SaaS), and you may not sell the software itself or remove licensing/copyright notices.

See [LICENSE](LICENSE) for full details.

---
*Built with passion by Julian Rodelo.*
