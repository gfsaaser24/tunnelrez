//! Ending one client session: stop its worker threads without waiting on a stuck socket.

use crate::frame_queue::FrameQueue;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// Stop the session's workers (decoder, stats) and wait for them.
///
/// Order matters. A worker can be blocked in a socket write (the peer stopped reading) or waiting for the
/// writer lock behind such a write, and closing the queue does not wake it. So the socket is shut down
/// first, which fails those writes, and only then are the workers joined.
pub(crate) fn end_session(queue: &FrameQueue, stop: &AtomicBool, stream: &TcpStream, workers: Vec<JoinHandle<()>>) {
    queue.close();
    stop.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(Shutdown::Both);
    for h in workers {
        let _ = h.join();
    }
}

// Unix only: Linux (and Android) fail a blocked send() when the socket is shut down; Windows does not wake it,
// so on a Windows host this test would hang on the peer, not on the code under test.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    /// The peer stops reading: the stats-like worker blocks in `write_all` while holding the writer lock, and
    /// the decoder-like worker blocks behind it (an IDR request). Ending the session must still finish.
    #[test]
    fn ends_with_a_worker_blocked_on_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap(); // never reads
        let (stream, _) = listener.accept().unwrap();
        let w = Arc::new(Mutex::new(stream.try_clone().unwrap()));
        let queue = Arc::new(FrameQueue::new(3, 3));
        let stop = Arc::new(AtomicBool::new(false));

        let stats = {
            let (w, stop) = (Arc::clone(&w), Arc::clone(&stop));
            std::thread::spawn(move || {
                let chunk = vec![0u8; 1 << 20];
                while !stop.load(Ordering::SeqCst) {
                    if w.lock().unwrap().write_all(&chunk).is_err() {
                        break;
                    }
                }
            })
        };
        // Let the send buffers fill so the stats worker is blocked inside write_all.
        std::thread::sleep(Duration::from_millis(500));
        let dec = {
            let (w, q) = (Arc::clone(&w), Arc::clone(&queue));
            std::thread::spawn(move || {
                let _ = w.lock().unwrap().write_all(b"idr");
                while q.pop(Duration::from_millis(10)).is_some() || !q.is_closed() {}
            })
        };
        std::thread::sleep(Duration::from_millis(100));

        let (done_tx, done_rx) = mpsc::channel();
        let ender = std::thread::spawn(move || {
            end_session(&queue, &stop, &stream, vec![dec, stats]);
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_secs(10)).is_ok(), "session end hung on a blocked writer");
        ender.join().unwrap();
        drop(peer);
    }
}
