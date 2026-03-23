# Bittice: High-Performance Local Data Engine

[Read in English](README.md) | [Leer en Español](README.es.md)

**Bittice** is a high-performance local data engine designed to synchronize directly with MySQL databases, serving data instantly through an interactive CLI and local APIs (REST & gRPC).

## ⚡ Why Bittice? (Performance vs. Traditional DBs)

Bittice is not a replacement for your primary transactional database; it is a **high-performance read-layer** designed to handle massive search and analysis workloads that would otherwise slow down your production environment.

### 1. Dynamic Bitmaps vs. Static Indexes
In traditional SQL databases, you need specific composite indexes (e.g., `INDEX(a, b)`) for every combination of filters. 
**Bittice uses Roaring Bitmaps** for every unique value. This allows the engine to perform ultra-fast logical `AND`/`OR` operations between filters dynamically, providing total flexibility without the overhead of maintaining hundreds of traditional indexes.

### 2. Columnar Efficiency
Traditional databases (Row-Oriented) must read entire rows from disk even if you only need one or two fields.
**Bittice is Column-Oriented.** It only touches the specific data requested. This drastically reduces I/O pressure and allows it to process millions of records in milliseconds.

### 3. Production Isolation (via CDC)
Running heavy analytical queries (GroupBys, deep filters) on your production database can cause locks and slow down users.
**Bittice uses Change Data Capture (CDC)** to act as an isolated, real-time replica. You can run intensive search workloads on Bittice with **zero impact** on your main database's performance.

### 4. Native Time-Series Enrichment
Instead of calculating year, month, or day during query time (which is slow), **Bittice automatically expands date fields** into sub-columns during ingestion. This transforms expensive date calculations into simple, instant index lookups.

---

## 🦀 Built with Rust

Bittice is written entirely in **Rust**, which is crucial for its performance and reliability:

- **Zero-Cost Abstractions:** High-level code that compiles down to efficient machine instructions without the overhead of a Garbage Collector (GC).
- **Memory Safety:** Rust's ownership model ensures memory safety and prevents common bugs like null pointers or data races, critical for a multi-threaded data engine.
- **High Concurrency:** Leveraging `Tokio` and `Rayon`, Bittice parallelizes searches and data materialization across all CPU cores with minimal overhead.
- **Direct System Access:** Rust allows fine-grained control over memory-mapped files (`mmap`), enabling the engine to handle datasets much larger than available RAM by letting the OS manage page caching.

---

## 🛠 Prerequisites

Before starting, ensure you have the following installed:
- **Docker & Docker Desktop:** Mandatory. Bittice uses Docker to containerize the engine and the synchronization worker.
- **Rust (Cargo):** To run the project locally.

---

## 🚀 Getting Started

To start Bittice, simply run the project. The interactive wizard will guide you through the setup:

```bash
cargo run
```

This single command gives you two clear paths:
1. **Connect and synchronize:** Configure a new MySQL connection to start a real-time CDC sync.
2. **Use existing data:** Jump directly to the query engine using data already synchronized.

---

## 🔄 Step-by-Step: Connecting to MySQL

When you choose **"Connect and synchronize"**, follow these steps:

1.  **MySQL Host:** Enter the address of your database (e.g., `localhost` or `192.168.1.100`).
2.  **Port:** The port where MySQL is listening (usually `3306`).
3.  **User & Password:** Your database credentials.
4.  **Database to synchronize:** The name of the specific database you want to index.
5.  **Entity Name:** A nickname for this connection in Bittice (used in your API paths).
6.  **Initial Sync:** Bittice will start a "Bootstrap" to clone your existing data into local indices.
7.  **Docker Image Build:** The wizard will ask to build a custom Docker image for your entity. **This is highly recommended.**
8.  **Docker Compose Stack:** Finally, it will offer to generate and start a `docker-compose.yml`. This creates two containers:
    -   `engine`: The query server (REST/gRPC).
    -   `sync`: The worker that keeps data updated in real-time using the MySQL Binlog.

---

## 🔄 MySQL Synchronization (CDC)

Bittice acts as a real-time replica of your MySQL database. Once the initial sync is complete, it listens to the MySQL binary log (Binlog) to reflect `INSERT`, `UPDATE`, and `DELETE` operations instantly in your local indices. 

**Offline Support:** If the sync stops, it resumes from the last known state upon restart.

---

## 🛠 Query Management

Queries in Bittice are called **Operations**. You can manage them using the REST API at the `/_config` endpoint.

### Create a Query (POST)
Send a `POST` request to `http://localhost:3000/_config` with the query definition:

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
    "filters_op": "And",
    "order_by": [{ "field": "payment_date", "direction": "Desc" }],
    "limit": 10,
    "selected_fields": ["*"]
  }
}
```

### List Queries (GET)
`GET http://localhost:3000/_config`

### Delete a Query (DELETE)
`DELETE http://localhost:3000/_config?name=recent_sales`

---

## 🌐 API Reference

### REST API (Port 3000)

- **Execute Query:** `GET /query_name`
- **Parameterized Query:** `GET /query_name?param1=value1`
  - *Note: Use `$` prefix in your query definition (e.g., `"value": "$min_amount"`) to make it a parameter.*
- **System Info:** `GET /_debug`, `GET /_entities`

### gRPC API (Port 50051)

Bittice provides a high-performance gRPC interface defined in `proto/bittice.proto`.

- **`Search` / `SearchUnary`:** Direct ad-hoc searching.
- **`ExecuteSavedQuery`:** Run a pre-configured operation by name.
- **`SubscribeUpdates` (Real-time):** Stream updates for a specific table. Get notified instantly when data changes.

---

## 📁 Data Structure & Ports

- **Default REST Port:** `3000`
- **Default gRPC Port:** `50051`
- **Storage Path:** All indexed data is stored in the `data/` directory.
- **Operations:** Saved queries are persisted in `data/.bittice_ops.json`.

---
*Bittice - Fast, Local, Efficient.*
