//! SqlExactStore::find_definitions 的 SQLite 集成测试：
//! fanout 窗口函数正确性（门控判定依赖它）、scope 过滤、endpoint 优先。

use oce_core::search::ExactSearchStore;
use oce_infra::sqlite::chains::SqlExactStore;
use oce_infra::sqlite::SqlDb;

fn seed_blob(conn: &rusqlite::Connection, name: &str, path: &str) {
    conn.execute(
        "INSERT INTO blobs (blob_name, path, content_size, status) VALUES (?1, ?2, 10, 'ready')",
        rusqlite::params![name, path],
    )
    .unwrap();
}

fn seed_symbol(
    conn: &rusqlite::Connection,
    identifier: &str,
    blob_name: &str,
    content_hash: &str,
    kind: &str,
) {
    conn.execute(
        "INSERT INTO chunks (content_hash, content, content_size) VALUES (?1, 'x', 1)
         ON CONFLICT DO NOTHING",
        rusqlite::params![content_hash],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO blob_chunks (blob_name, content_hash, start_line, end_line, chunk_index)
         VALUES (?1, ?2, 1, 1, 0)",
        rusqlite::params![blob_name, content_hash],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO symbol_occurrences (identifier, blob_name, content_hash, kind, start_line, end_line)
         VALUES (?1, ?2, ?3, ?4, 1, 1)",
        rusqlite::params![identifier, blob_name, content_hash, kind],
    )
    .unwrap();
}

fn store(db: &SqlDb) -> SqlExactStore {
    SqlExactStore {
        db: db.clone(),
        max_scope_blobs: 2_000,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fanout_counts_distinct_blobs_and_gates() {
    let db = SqlDb::open_memory().unwrap();
    db.with_conn(|conn| {
        // GenericSym 在 16 个 blob 里定义（fanout=16 > 15 门控）
        for i in 0..16 {
            let blob = format!("blob-{i:02}");
            seed_blob(conn, &blob, &format!("src/f{i:02}.rs"));
            seed_symbol(
                conn,
                "GenericSym",
                &blob,
                &format!("hash-g{i:02}"),
                "definition",
            );
        }
        // SpecificSym 只在 1 个 blob 里定义
        seed_blob(conn, "blob-api", "src/api.rs");
        seed_symbol(conn, "SpecificSym", "blob-api", "hash-api", "definition");
        Ok(())
    })
    .unwrap();

    let defs = store(&db)
        .find_definitions(&["GenericSym".into(), "SpecificSym".into()], None)
        .await
        .unwrap();

    let generic: Vec<_> = defs
        .iter()
        .filter(|d| d.identifier == "GenericSym")
        .collect();
    assert!(!generic.is_empty());
    assert_eq!(
        generic[0].file_fanout, 16,
        "fanout must count distinct blobs"
    );
    let specific = defs.iter().find(|d| d.identifier == "SpecificSym").unwrap();
    assert_eq!(specific.file_fanout, 1);
    assert_eq!(specific.path, "src/api.rs");
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_filter_excludes_other_workspaces() {
    let db = SqlDb::open_memory().unwrap();
    db.with_conn(|conn| {
        seed_blob(conn, "ws1-blob", "ws1/mod.rs");
        seed_symbol(conn, "SharedSym", "ws1-blob", "hash-ws1", "definition");
        seed_blob(conn, "ws2-blob", "ws2/mod.rs");
        seed_symbol(conn, "SharedSym", "ws2-blob", "hash-ws2", "definition");
        Ok(())
    })
    .unwrap();

    // scope 限定 ws1：只返回 ws1 的定义
    let defs = store(&db)
        .find_definitions(&["SharedSym".into()], Some(&["ws1-blob".to_string()]))
        .await
        .unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].path, "ws1/mod.rs");
    // scope 内 fanout 也只计 scope 内的文件
    assert_eq!(defs[0].file_fanout, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn endpoint_kind_ranks_first() {
    let db = SqlDb::open_memory().unwrap();
    db.with_conn(|conn| {
        seed_blob(conn, "blob-a", "src/routes.rs");
        seed_symbol(conn, "handler_x", "blob-a", "hash-ep", "endpoint");
        seed_blob(conn, "blob-b", "src/other.rs");
        seed_symbol(conn, "handler_x", "blob-b", "hash-def", "definition");
        Ok(())
    })
    .unwrap();

    let defs = store(&db)
        .find_definitions(&["handler_x".into()], None)
        .await
        .unwrap();
    // endpoint 行在前（core 侧 or_insert 首见胜出 → endpoint 被选中）
    assert_eq!(defs[0].kind, "endpoint");
    assert_eq!(defs[0].path, "src/routes.rs");
}

#[tokio::test(flavor = "multi_thread")]
async fn oversize_scope_returns_empty() {
    let db = SqlDb::open_memory().unwrap();
    let scope: Vec<String> = (0..2001).map(|i| format!("b-{i}")).collect();
    let defs = store(&db)
        .find_definitions(&["x".into()], Some(&scope))
        .await
        .unwrap();
    assert!(defs.is_empty());
}
