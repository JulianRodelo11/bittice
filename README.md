# Bittice: High-Performance Local Data Engine

**Bittice** is a high-performance local data engine designed to process massive NDJSON files or synchronize directly with MySQL databases, serving data instantly through an interactive CLI and local APIs (REST & gRPC).

## Key Features

- **Blazing Fast Indexing:** Transform JSON Lines into optimized binary structures (`.dat`) and position indexes (`.offsets`).
- **MySQL CDC (Change Data Capture):** Real-time synchronization with MySQL databases.
- **Roaring Bitmaps:** Millisecond-level filtering using advanced bitmap indices.
- **Time Components:** Automatic date detection and generation of sub-fields (day, month, hour) for time-series analysis.
- **Dual API Surface:** Serve your data via **REST (Axum)** or **gRPC (Tonic)**.
- **Interactive Startup:** A guided CLI flow to connect databases, sync data, and even containerize your setup.

---

## 🚀 Getting Started

Simply run `bittice` without arguments to enter the interactive startup flow:

```bash
./bittice
```

This will guide you through:
1. **Selecting Operation Mode:** Connect a new database or use existing data.
2. **MySQL Synchronization:** Configure your connection (Host, Port, User, Password, Database).
3. **Docker Integration:** Optionally build a Docker image and generate a `docker-compose.yml` for your stack.

### Manual CLI Commands

- **Load NDJSON:** `./bittice load --input data.ndjson --entity my_app --table users`
- **Start Server:** `./bittice server --type all --port 50051` (Starts both REST on `:3000` and gRPC on `:50051`).
- **Manual CDC:** `./bittice cdc --url "mysql://user:pass@host:port/db" --entity my_app --database db`

---

## 🔄 MySQL Synchronization (CDC)

Bittice can act as a real-time replica of your MySQL database. 

1. **Connection:** Provide your MySQL credentials during startup.
2. **Bootstrap:** Bittice first performs a full snapshot of your tables.
3. **Real-time Sync:** Once bootstrapped, it listens to the MySQL binary log (Binlog) to reflect `INSERT`, `UPDATE`, and `DELETE` operations instantly in your local indices.
4. **Offline Support:** If the sync stops, it resumes from the last known state upon restart.

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
    "aggregations": [],
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
- **`SubscribeUpdates` (Real-time):** Stream updates for a specific table. Get notified instantly when data changes in the underlying storage.

---

## 📁 Data Structure & Ports

- **Default REST Port:** `3000`
- **Default gRPC Port:** `50051`
- **Storage Path:** All indexed data is stored in the `data/` directory.
- **Operations:** Saved queries are persisted in `data/.bittice_ops.json`.

---
*Bittice - Fast, Local, Efficient.*
