//! Headless driver for the workcat map's store: exercises the exact
//! production read/append/checkpoint code paths without the GUI, so the
//! event append + fold + commit cycle can be proven against a throwaway
//! clone (and then run once for real).
//!
//! Usage (db dir from WORKCAT_DB_DIR, default ~/workcat-db):
//!
//! ```text
//! cargo run -p workcat_map --example workcat_cycle -- load
//! cargo run -p workcat_map --example workcat_cycle -- move <item-id> <x> <y>
//! cargo run -p workcat_map --example workcat_cycle -- status <item-id> <status>
//! cargo run -p workcat_map --example workcat_cycle -- checkpoint
//! ```
//!
//! `move` only appends (fine grain); `status` and `checkpoint` run the
//! full write protocol: lock acquire, fold, commit, push, release.

use workcat_map::model::Status;
use workcat_map::store;

fn progress_channel() -> (store::Progress, std::thread::JoinHandle<()>) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
    let printer = std::thread::spawn(move || {
        use futures::StreamExt as _;
        futures::executor::block_on(async {
            while let Some(message) = rx.next().await {
                println!("progress: {message}");
            }
        });
    });
    (tx, printer)
}

fn main() -> anyhow::Result<()> {
    let db = store::resolve_db_dir();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: workcat_cycle load | move <id> <x> <y> | status <id> <status> | checkpoint";
    match args.first().map(String::as_str) {
        Some("load") => {
            let items = store::load_items(&db)?;
            let positions = store::load_positions(&db)?;
            let visible: Vec<_> = items
                .iter()
                .filter(|item| item.status.in_default_scope())
                .collect();
            let total_edges: usize = items.iter().map(|item| item.depends_on.len()).sum();
            let visible_id8s: std::collections::HashSet<&str> =
                visible.iter().map(|item| item.id8()).collect();
            let visible_edges: usize = visible
                .iter()
                .flat_map(|item| item.depends_on.iter())
                .filter(|dep| visible_id8s.contains(dep.as_str()))
                .count();
            println!(
                "db: {}\nitems: {} total, {} in default scope\nedges: {} total, {} between visible nodes\npersisted positions: {}",
                db.display(),
                items.len(),
                visible.len(),
                total_edges,
                visible_edges,
                positions.len(),
            );
        }
        Some("move") => {
            let (id, x, y) = (
                args.get(1).expect(usage),
                args.get(2).expect(usage).parse::<f32>()?,
                args.get(3).expect(usage).parse::<f32>()?,
            );
            store::append_event(&db, &store::node_moved_event(id, x, y))?;
            println!("appended node_moved for {id} at ({x}, {y}) (uncommitted)");
        }
        Some("status") => {
            let (id, status) = (args.get(1).expect(usage), args.get(2).expect(usage));
            let status = Status::parse(status).expect("valid status");
            store::append_event(&db, &store::status_set_event(id, status.as_str()))?;
            let (progress, printer) = progress_channel();
            let message = format!("status: {} -> {}", &id[..id.len().min(8)], status.as_str());
            let sha = smol::block_on(store::checkpoint(&db, &message, &progress))?;
            drop(progress);
            printer.join().ok();
            println!("status_set appended and checkpointed: {sha:?}");
        }
        Some("checkpoint") => {
            let (progress, printer) = progress_channel();
            let sha = smol::block_on(store::checkpoint(&db, "map: session checkpoint", &progress))?;
            drop(progress);
            printer.join().ok();
            println!("checkpoint: {sha:?}");
        }
        _ => anyhow::bail!(usage),
    }
    Ok(())
}
