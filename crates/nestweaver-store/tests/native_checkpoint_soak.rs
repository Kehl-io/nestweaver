use lbug::{Connection, Database, SystemConfig, Value};
use std::{
    io::Write,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
const ROWS: i64 = 8192;
fn expected(id: i64) -> String {
    format!("row|{id}|αβγ|{}|end", "x".repeat(96))
}
/// Manual upstream-engine qualification. Run ten complete repetitions with
/// the same binary/config; ordinary CI intentionally does not run the soak.
/// This does not change the shipping single-thread engine default.
#[test]
#[ignore = "manual 5-minute raw-engine checkpoint/scan soak; run ten repetitions"]
fn native_checkpoint_scan_soak() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let seconds: u64 = std::env::var("NESTWEAVER_NATIVE_SOAK_SECONDS")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(300);
    let threads: u64 = std::env::var("NESTWEAVER_NATIVE_SOAK_THREADS")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(8);
    assert!(seconds > 0 && threads > 0);
    let csv = root.join("rows.csv");
    let mut file = std::fs::File::create(&csv).unwrap();
    writeln!(file, "id,value").unwrap();
    for id in 0..ROWS {
        writeln!(file, "{id},{}", expected(id)).unwrap();
    }
    drop(file);
    let db = Database::new(
        root.join("stress.lbug"),
        SystemConfig::default()
            .max_db_size(4 * 1024 * 1024 * 1024)
            .buffer_pool_size(128 * 1024 * 1024)
            .max_num_threads(threads)
            .auto_checkpoint(false),
    )
    .unwrap();
    let init = Connection::new(&db).unwrap();
    init.query("CREATE NODE TABLE Sample(id INT64, value STRING, PRIMARY KEY(id))")
        .unwrap();
    init.query(&format!(
        "COPY Sample FROM '{}' (HEADER=true, PARALLEL=false)",
        csv.display()
    ))
    .unwrap();
    init.query("CHECKPOINT").unwrap();
    drop(init);
    let stop = AtomicBool::new(false);
    let scans = AtomicU64::new(0);
    let read_rows = AtomicU64::new(0);
    let writes = AtomicU64::new(0);
    let malformed = AtomicU64::new(0);
    let cardinality = AtomicU64::new(0);
    let errors = Mutex::new(Vec::<String>::new());
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                let conn = Connection::new(&db).unwrap();
                while !stop.load(Ordering::Relaxed)
                    && started.elapsed() < Duration::from_secs(seconds)
                {
                    let result = conn.query("MATCH (n:Sample) RETURN n.id,n.value");
                    match result {
                        Ok(result) => {
                            let mut count = 0u64;
                            let mut seen = std::collections::HashSet::new();
                            for row in result {
                                count += 1;
                                match (&row[0], &row[1]) {
                                    (Value::Int64(id), Value::String(value))
                                        if *id >= 0 && *id < ROWS && *value == expected(*id) =>
                                    {
                                        if !seen.insert(*id) {
                                            cardinality.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                    _ => {
                                        malformed.fetch_add(1, Ordering::Relaxed);
                                        let mut errors = errors.lock().unwrap();
                                        if errors.len() < 5 {
                                            errors.push(format!("malformed row: {row:?}"));
                                        }
                                        drop(errors);
                                        stop.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                            if count != ROWS as u64 {
                                cardinality.fetch_add(1, Ordering::Relaxed);
                            }
                            scans.fetch_add(1, Ordering::Relaxed);
                            read_rows.fetch_add(count, Ordering::Relaxed);
                        }
                        Err(e) => {
                            errors.lock().unwrap().push(format!("read query: {e}"));
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
        scope.spawn(||{let conn=Connection::new(&db).unwrap();while !stop.load(Ordering::Relaxed)&&started.elapsed()<Duration::from_secs(seconds){
   let result=(||->Result<(),lbug::Error>{conn.query("BEGIN TRANSACTION")?;conn.query("MATCH(n:Sample) WHERE n.id < 64 DELETE n")?;conn.query(&format!("UNWIND range(0,63) AS i CREATE(:Sample {{id:i,value:'row|'+CAST(i AS STRING)+'|αβγ|{}|end'}})","x".repeat(96)))?;conn.query("COMMIT")?;conn.query("CHECKPOINT")?;Ok(())})();
   match result{Ok(())=>{writes.fetch_add(1,Ordering::Relaxed);},Err(e)=>{errors.lock().unwrap().push(format!("writer: {e}"));stop.store(true,Ordering::Relaxed);}}
  }});
    });
    let errs = errors.into_inner().unwrap();
    for e in &errs {
        eprintln!("{e}");
    }
    println!(
        "RESULT {{\"seconds\":{},\"engine_threads\":{},\"scans\":{},\"rows_read\":{},\"write_checkpoint_cycles\":{},\"malformed_rows\":{},\"cardinality_events\":{},\"errors\":{}}}",
        started.elapsed().as_secs_f64(),
        threads,
        scans.load(Ordering::Relaxed),
        read_rows.load(Ordering::Relaxed),
        writes.load(Ordering::Relaxed),
        malformed.load(Ordering::Relaxed),
        cardinality.load(Ordering::Relaxed),
        errs.len()
    );
    assert_eq!(malformed.load(Ordering::Relaxed), 0, "malformed rows");
    assert_eq!(
        cardinality.load(Ordering::Relaxed),
        0,
        "duplicate/missing row snapshots"
    );
    assert!(errs.is_empty(), "native query errors: {errs:?}");
    assert!(scans.load(Ordering::Relaxed) > 0 && writes.load(Ordering::Relaxed) > 0);
}
