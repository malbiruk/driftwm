//! Multi-threaded TIFF tile decoder pool. Off-loads `TiffSource::read_tile`
//! from the render thread so the compositor stays at 60fps while chunked
//! backgrounds resolve. Texture upload (`import_memory`) stays on the render
//! thread — it needs the GL context — but the long pole (libtiff +
//! decompression) runs across `N_WORKERS` cores.
//!
//! Workers park on `recv()` when idle: zero CPU, ~stack-size RAM each. Pool
//! is owned by [`BgChunkCache`](super::tile_chunks::BgChunkCache), so workers
//! exist only while a pyramidal TIFF wallpaper is active and shut down
//! cleanly when the cache drops (config reload, output removal, exit).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use calloop::LoopSignal;

use super::tile_chunks_tiff::{DecodedTile, TiffSource};

/// Per-pool worker count. Six fills our 8-tile-per-frame upload budget
/// when decodes run ~10 ms each (60 wpr × 60 fps ≈ 600 decodes/sec, brushes
/// the upload cap of ~480/sec → no further parallelism gain past this).
/// Workers are parked in the kernel when idle and don't reserve cores;
/// idle cost is ~2 MB stack each, file handles only while a TIFF wallpaper
/// is active. See commit history for the bench rationale.
pub const N_WORKERS: usize = 6;

#[derive(Debug, Clone, Copy)]
pub struct TileRequest {
    pub lod: u32,
    pub cx: u32,
    pub cy: u32,
}

pub struct TileResponse {
    pub req: TileRequest,
    pub result: Result<DecodedTile, String>,
}

struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

struct QueueState {
    pending: VecDeque<TileRequest>,
    shutdown: bool,
}

pub struct WorkerPool {
    queue: Arc<Queue>,
    responses: mpsc::Receiver<TileResponse>,
    workers: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// Spawn `N_WORKERS` decoder threads, each owning an independent
    /// [`TiffSource`] for the same file (libtiff isn't thread-safe on a
    /// single decoder). Returns the pool ready to enqueue requests.
    pub fn spawn(path: PathBuf, loop_signal: LoopSignal) -> Result<Self, String> {
        let queue = Arc::new(Queue {
            state: Mutex::new(QueueState {
                pending: VecDeque::new(),
                shutdown: false,
            }),
            cv: Condvar::new(),
        });
        let (resp_tx, resp_rx) = mpsc::channel();
        let mut workers = Vec::with_capacity(N_WORKERS);

        for worker_id in 0..N_WORKERS {
            let queue = Arc::clone(&queue);
            let resp_tx = resp_tx.clone();
            let path = path.clone();
            let loop_signal = loop_signal.clone();
            let source = TiffSource::open(&path)
                .map_err(|e| format!("tile worker {worker_id}: open: {e}"))?;
            let handle = std::thread::Builder::new()
                .name(format!("driftwm-tile-{worker_id}"))
                .spawn(move || worker_loop(source, queue, resp_tx, loop_signal))
                .map_err(|e| format!("spawn tile worker {worker_id}: {e}"))?;
            workers.push(handle);
        }

        Ok(Self {
            queue,
            responses: resp_rx,
            workers,
        })
    }

    pub fn enqueue(&self, req: TileRequest) {
        let mut s = self.queue.state.lock().unwrap();
        s.pending.push_back(req);
        self.queue.cv.notify_one();
    }

    pub fn try_recv(&self) -> Option<TileResponse> {
        self.responses.try_recv().ok()
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        {
            let mut s = self.queue.state.lock().unwrap();
            s.shutdown = true;
            s.pending.clear();
        }
        self.queue.cv.notify_all();
        for handle in self.workers.drain(..) {
            // Workers exit after their current decode (5-20ms worst case).
            // join() is bounded; we don't need a timeout.
            let _ = handle.join();
        }
    }
}

fn worker_loop(
    mut source: TiffSource,
    queue: Arc<Queue>,
    resp_tx: mpsc::Sender<TileResponse>,
    loop_signal: LoopSignal,
) {
    loop {
        let req = {
            let mut s = queue.state.lock().unwrap();
            loop {
                if s.shutdown {
                    return;
                }
                if let Some(r) = s.pending.pop_front() {
                    break r;
                }
                s = queue.cv.wait(s).unwrap();
            }
        };
        let result = source.read_tile(req.lod, req.cx, req.cy);
        if resp_tx.send(TileResponse { req, result }).is_err() {
            // Pool dropped; we're the last writer or close to it.
            return;
        }
        // Break calloop out of poll() so the render thread sees the response
        // this cycle instead of waiting for unrelated damage.
        loop_signal.wakeup();
    }
}
