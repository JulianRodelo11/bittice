# BITTICE_STARTUP_CONSISTENCY_CHECK — v0.1.174

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

## Flujo (v0.1.174)

```
Arranque
  │
  ├─ BITTICE_STARTUP_CONSISTENCY_CHECK=1 ?
  │   │
  │   ├─ Sí → Para cada perfil CDC:
  │   │        1. Conecta a MySQL, COUNT(*) por tabla
  │   │        2. Cuenta filas mirror (offsets - deleted)
  │   │        3. Si |diff| > threshold → invalida bootstrap
  │   │        4. Si |diff| ≤ threshold → intacta
  │   │        5. Activa BITTICE_STAGED_WAIT_FOR_RESUME_CATCHUP=1
  │   │
  │   └─ No → salta
  │
  ├─ CDC autostart (staged, secuencial)
  │   │
  │   ├─ Tablas intactas → CDC arranca desde binlog position
  │   ├─ Tablas invalidadas → CDC re-bootstrap (SELECT *)
  │   │
  │   └─ Resume gap? 
  │       ├─ Sí + BITTICE_STAGED_WAIT_FOR_RESUME_CATCHUP=1
  │       │   → HTTP BLOQUEADO hasta catch-up completo ⏳
  │       └─ No → HTTP abre inmediatamente
  │
  └─ HTTP/gRPC abren solo cuando todo está en sync ✓
```

**Diferencia clave v0.1.174 vs v0.1.173**: en .174, `BITTICE_STARTUP_CONSISTENCY_CHECK=1` activa automáticamente `BITTICE_STAGED_WAIT_FOR_RESUME_CATCHUP=1`. HTTP no abre hasta que CDC esté 100% al día con el binlog.

## Casos de uso (v0.1.174)

| Escenario | Comportamiento |
|---|---|
| Reinicio normal, todo en sync | Validación, 0 reparaciones, CDC directo, HTTP abre ✓ |
| Hubo caída, 3 tablas con drift | Solo esas 3 se re-bootstrapean. HTTP **bloqueado** hasta que CDC alcance el binlog actual ✓ |
| Binlog gap detectado | CDC aplica eventos pendientes. HTTP **bloqueado** hasta catch-up completo ✓ |
| No hay conexión a MySQL | Log warn, arranca con mirror estático |
| Primer arranque (bootstrapped_tables=0) | Bootstrap completo. HTTP **bloqueado** hasta terminar ✓ |

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
