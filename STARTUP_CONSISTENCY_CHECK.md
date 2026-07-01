# BITTICE_STARTUP_CONSISTENCY_CHECK — v0.1.173

## Qué hace

Al arrancar con `BITTICE_STARTUP_CONSISTENCY_CHECK=1`, antes de iniciar CDC y abrir HTTP, compara `COUNT(*)` entre cada tabla del mirror y su fuente MySQL. Solo repara las tablas con drift significativo (>10 filas). Si todo está en sync, arranca directo sin tocar nada.

**No agresivo**: no borra todo el mirror, solo las tablas con diferencias reales.

## Uso

```bash
docker run ... \
  -e BITTICE_STARTUP_CONSISTENCY_CHECK=1 \
  -e BITTICE_STARTUP_CONSISTENCY_THRESHOLD=10 \   # opcional, default 10
  ...
```

## Flujo

```
Arranque
  │
  ├─ BITTICE_STARTUP_CONSISTENCY_CHECK=1 ?
  │   │
  │   ├─ Sí → Para cada perfil CDC:
  │   │        1. Lee cdc_config.json + cdc_state.json
  │   │        2. Conecta a MySQL y hace COUNT(*) por tabla
  │   │        3. Cuenta filas vivas en mirror (offsets - deleted.bitmap)
  │   │        4. Si |diff| > threshold → invalida bootstrap de esa tabla:
  │   │           - La quita de bootstrapped_tables en cdc_state.json
  │   │           - Borra el directorio mirror/<entity>/<table>
  │   │        5. Si |diff| ≤ threshold → la deja intacta
  │   │        6. Si no se puede conectar a MySQL → log warn, sigue sin reparar
  │   │
  │   └─ No → salta
  │
  ├─ CDC autostart (staged, secuencial)
  │   │
  │   └─ Tablas intactas → CDC arranca directo desde binlog position
  │       Tablas invalidadas → CDC hace SELECT * completo (re-bootstrap)
  │
  └─ HTTP/gRPC abren
```

## Casos de uso

| Escenario | Comportamiento |
|---|---|
| Reinicio normal, todo en sync | Validación rápida, 0 reparaciones, CDC directo |
| Hubo caída, 3 tablas con drift | Solo esas 3 se re-bootstrapean, las otras 19 intactas |
| No hay conexión a MySQL | Log warn, arranca con mirror estático |
| Primer arranque (bootstrapped_tables=0) | Salta (no hay nada que validar) |

## Código

- `src/server/mod.rs:798-980` — `startup_consistency_check_enabled()` + `run_startup_consistency_repair()`
- Se llama en `start_all_servers()` línea ~288, antes del CDC autostart
- Usa `mirror_consistency::check_mirror_consistency()` y `mirror_consistency::resolve_mirror_dir()`

## Lecciones del incidente 2026-07-01

1. **No resetear solo bootstrapped_tables**: al resetear manualmente también hay que resetear `binlog_file` y `binlog_pos` en cdc_state.json. Si no, CDC hace bootstrap fresco pero luego intenta reprocesar todo el binlog desde la posición vieja → backlog enorme.

2. **Reset completo para resync**:
   ```json
   {
     "binlog_file": "",
     "binlog_pos": 0,
     "gtid_executed": "",
     "bootstrapped_tables": [],
     "pk_map": {},
     "last_mirror_batch_unix_ms": 0,
     "last_mirror_batch": ""
   }
   ```

3. **No correr `check-mirror` desde otro contenedor mientras bittice está activo**: los archivos mmap están bloqueados y el segundo proceso ve datos inconsistentes (MIRROR=0). Si se necesita, detener bittice primero.

4. **`--network host` rompe Caddy**: Caddy espera resolver `bittice:3000` via Docker DNS. Si se usa host network, cambiar Caddyfile a `172.17.0.1:3000`.
