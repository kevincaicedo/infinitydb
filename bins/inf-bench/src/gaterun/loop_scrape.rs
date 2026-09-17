//! Persistent per-cell connections keep discovery outside measured windows.

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::time::Duration;

use crate::loop_histogram::{REPLY_BYTES_MAX, Snapshot};
use crate::resp::{connect, parse_info, request_bounded};

pub(super) struct LoopScraper {
    streams: Vec<TcpStream>,
}

impl LoopScraper {
    pub fn connect(port: u16, cells: u16) -> Result<Self, String> {
        if cells == 0 {
            return Err("no loop histogram cells requested".into());
        }
        let mut streams = BTreeMap::new();
        for _ in 0..512 {
            let mut stream = connect("127.0.0.1", port)?;
            stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
            stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
            let info =
                parse_info(&request_bounded(&mut stream, &[b"INFO", b"server"], REPLY_BYTES_MAX)?);
            let cell = info
                .get("cell")
                .and_then(|v| v.parse::<u16>().ok())
                .filter(|&cell| cell < cells)
                .ok_or("invalid loop histogram cell identity")?;
            streams.entry(cell).or_insert(stream);
            if streams.len() == usize::from(cells) {
                return Ok(Self { streams: streams.into_values().collect() });
            }
        }
        Err(format!("scraped {}/{} loop histogram cells", streams.len(), cells))
    }

    pub fn fresh(&mut self) -> Result<Vec<Snapshot>, String> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .streams
                .iter_mut()
                .map(|stream| scope.spawn(move || fresh_cell(stream)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().map_err(|_| "loop histogram scraper panicked")?)
                .collect()
        })
    }
}

fn read(stream: &mut TcpStream) -> Result<Option<Snapshot>, String> {
    Snapshot::decode_info(&request_bounded(
        stream,
        &[b"INFO", b"server", b"loophist"],
        REPLY_BYTES_MAX,
    )?)
}

fn fresh_cell(stream: &mut TcpStream) -> Result<Snapshot, String> {
    let previous = read(stream)?;
    for _ in 0..64 {
        if let Some(snapshot) = read(stream)? {
            if let Some(ref previous) = previous
                && (snapshot.cell != previous.cell
                    || snapshot.run_id != previous.run_id
                    || snapshot.cells != previous.cells
                    || snapshot.samples < previous.samples)
            {
                return Err("loop histogram changed identity or reset during scrape".into());
            }
            if snapshot.samples > previous.as_ref().map_or(0, |p| p.samples) {
                return Ok(snapshot);
            }
        }
    }
    Err("stale loop histogram snapshot after 64 requests".into())
}
