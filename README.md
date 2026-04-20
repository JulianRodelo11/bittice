# Bittice: High-Performance Local Data Engine

[Read in English](README.md) | [Leer en Español](README.es.md)

**Bittice** is a high-performance local data engine designed to synchronize directly with MySQL databases, serving data instantly through an interactive CLI and local APIs (REST & gRPC). It is designed for developers and companies that need ultra-fast read-layers to save cloud costs and improve performance without overloading production databases.

> **Note:** Bittice is **Source Available** under the **Elastic License v2.0**. It is free to use for personal, internal, and commercial purposes (e.g., to save infrastructure costs). However, you may not provide it to others as a managed service.

## ⚡ Key Features

*   **Dynamic Bitmaps:** Uses Roaring Bitmaps for ultra-fast logical operations (`AND`/`OR`) across all fields dynamically.
*   **Columnar Storage:** Only reads the data you need, drastically reducing I/O pressure.
*   **Real-time Sync (CDC):** Acts as a real-time replica of your MySQL database using Binlog, with zero impact on production performance.
*   **Time-Series Ready:** Automatically expands date fields into sub-columns (year, month, day, etc.) for instant lookups.
*   **Multi-Table Joins:** Supports `INNER` and `LEFT` joins in saved operations.
*   **Advanced Aggregations:** Includes `GroupBy`, `TopN`, `Avg`, `Min`, `Max`, and `CountDistinct` with `HAVING` support.
*   **Flexible APIs:** High-performance REST and gRPC interfaces.

---

## 🚀 Getting Started

### 🛠 Prerequisites
*   **Docker & Docker Desktop:** Required for containerizing the engine and sync worker.
*   **Rust (Cargo):** To build and run the interactive CLI.

### 🏃 Quick Start
To start Bittice, run the interactive wizard:

```bash
cargo run
```

The wizard will guide you through:
1.  **Connecting to MySQL:** Just provide your host, port, and credentials.
2.  **Entity Configuration:** Choose the database and tables you want to index.
3.  **Deployment:** Bittice will generate a `docker-compose.yml` to run the Engine and Sync worker.

---

## 🔄 How it Works

1.  **Bootstrap:** Bittice clones your existing MySQL data into highly optimized local columnar indices.
2.  **CDC (Change Data Capture):** It listens to the MySQL Binlog to reflect `INSERT`, `UPDATE`, and `DELETE` operations instantly.
3.  **Querying:** You define "Operations" (queries) via REST or use the interactive REPL to fetch data.

---

## 🛠 Query Management (Operations)

Queries in Bittice are called **Operations**. You manage them via the REST API at `/_config`.

### Example: Creating a Saved Query
```json
{
  "type": "read",
  "details": {
    "name": "recent_sales",
    "entity": "sakila",
    "table": "payment",
    "filters": [
      { "field": "amount", "op": ">", "value": "5.00" }
    ],
    "limit": 10,
    "selected_fields": ["*"]
  }
}
```

### Advanced Query Features
Bittice supports:
*   **Parameterized Queries:** Use `$` (e.g., `"value": "$min_amount"`) and pass values via URL params.
*   **Computed Fields:** Use arithmetic expressions directly in `select`.
*   **Response Grouping:** Group REST responses by keys for hierarchical JSON structures.
*   **Filter Trees:** Build complex nested logical groups (`AND`/`OR`).

---

## 🌐 API Reference

### REST API (Port 3000)
*   `GET /query_name` - Execute a saved query.
*   `GET /query_name?param=value` - Execute with parameters.
*   `POST /_config` - Create/Update an operation.
*   `GET /_config` - List operations.
*   `GET /_entities` - List synchronized entities.

### gRPC API (Port 50051)
*   `Search` / `SearchUnary`: Ad-hoc single-table searches.
*   `ExecuteSavedQuery`: Run pre-configured operations.
*   `SubscribeUpdates`: Stream real-time data changes.

---

## 📜 License

Bittice is licensed under the **Elastic License v2.0**.

*   **Permitted:** Free for personal use, internal use within companies, and commercial use to save on infrastructure costs. You can modify and redistribute the code.
*   **Prohibited:** You may not provide Bittice to third parties as a hosted or managed service (SaaS), and you may not remove licensing/copyright notices.

For the full terms, see the [LICENSE](LICENSE) file.

---
*Bittice - Fast, Local, Efficient.*
