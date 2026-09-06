//! Optional raw CLOB tape for future latency/queue-aware maker research.
//! A bounded queue isolates disk I/O from book processing. Gap markers make
//! overflow visible; intervals containing gaps are unsuitable for fill replay.
use std::sync::Arc;
use tokio::sync::mpsc;
use crate::db::Database;

pub type TapeSender = mpsc::Sender<(i64, String)>;

pub fn start(db: Arc<Database>) -> TapeSender {
    let (tx, mut rx) = mpsc::channel::<(i64, String)>(8192);
    std::thread::spawn(move || {
        let mut lost = 0usize;
        while let Some(first) = rx.blocking_recv() {
            let mut batch = Vec::with_capacity(256);
            if lost > 0 {
                batch.push((first.0, format!(r#"{{"event_type":"capture_gap","reason":"database_write_failure","dropped":{lost}}}"#)));
            }
            batch.push(first);
            while batch.len() < 256 {
                match rx.try_recv() { Ok(row) => batch.push(row), Err(_) => break }
            }
            match db.insert_market_events(&batch) {
                Ok(()) => lost = 0,
                Err(e) => {
                    lost += batch.len();
                    tracing::error!("Market tape write failed; {} missing events: {}", lost, e);
                }
            }
        }
    });
    tx
}

pub fn record(sender: &Option<TapeSender>, dropped: &mut usize, payload: &str) {
    let Some(tx) = sender else { return };
    let now = chrono::Utc::now().timestamp_millis();
    if *dropped > 0 {
        let marker = format!(r#"{{"event_type":"capture_gap","reason":"queue_overflow","dropped":{}}}"#, dropped);
        if tx.try_send((now, marker)).is_err() { *dropped += 1; return; }
        *dropped = 0;
    }
    if tx.try_send((now, payload.to_string())).is_err() {
        *dropped += 1;
        tracing::warn!("Market tape queue full or closed; capture gap will be recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflow_is_marked_before_capture_resumes() {
        let (tx, mut rx) = mpsc::channel(2);
        let sender = Some(tx);
        let mut dropped = 0;
        record(&sender, &mut dropped, "first");
        record(&sender, &mut dropped, "second");
        record(&sender, &mut dropped, "lost");
        assert_eq!(dropped, 1);
        rx.try_recv().unwrap(); rx.try_recv().unwrap();
        record(&sender, &mut dropped, "resumed");
        let marker: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap().1).unwrap();
        assert_eq!(marker["event_type"], "capture_gap");
        assert_eq!(marker["dropped"], 1);
        assert_eq!(rx.try_recv().unwrap().1, "resumed");
    }
}
