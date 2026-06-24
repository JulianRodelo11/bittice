//! Composite PK: multiple rows sharing the first PK column must all remain queryable.

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};
use std::collections::HashMap;

#[test]
fn composite_pk_collision_uses_both_filters() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity");
    let mut table = Table::open(&dir, "clients").expect("open");
    table.manifest.primary_key = "TipoDocumento".into();
    table.manifest.primary_key_columns = vec!["TipoDocumento".into(), "Cedula".into()];

    for (cedula, tipo, name) in [
        ("111001", "1", "Alice"),
        ("222002", "1", "Bob"),
    ] {
        let mut row = HashMap::new();
        row.insert("TipoDocumento".into(), tipo.into());
        row.insert("Cedula".into(), cedula.into());
        row.insert("Nombres".into(), name.into());
        table.insert(row).expect("insert");
    }
    table.flush_active_segment().expect("flush");
    table.reconcile_orphan_rows().expect("reconcile");
    assert_eq!(table.live_row_count(), 2);
    table.close().expect("close");

    let table = Table::open(&dir, "clients").expect("reopen");

    let fields = vec!["Cedula".into(), "TipoDocumento".into(), "Nombres".into()];
    let filters = vec![
        Filter {
            field: "Cedula".into(),
            op: ComparisonOp::Eq,
            value: "111001".into(),
            value_to: None,
            field_type: None,
            value_options: vec![],
        },
        Filter {
            field: "TipoDocumento".into(),
            op: ComparisonOp::Eq,
            value: "1".into(),
            value_to: None,
            field_type: None,
            value_options: vec![],
        },
    ];
    let result = table
        .search(&fields, &filters, &LogicalOp::And, &[], &[], 10, 0, None)
        .expect("search");
    assert_eq!(result.total_found, 1);
    assert_eq!(result.rows[0][2], "Alice");
}
