use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

mod index;
mod tx;

use index::IvfIndex;
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::io::{Read, Write};
use std::sync::OnceLock;
use std::thread;

// ── Pre-built HTTP responses ─────────────────────────────────────────────────
static FRAUD_RESP: &[(&[u8], &[u8])] = &[
    (b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.0}",  b""),
    (b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}",  b""),
    (b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}",  b""),
    (b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}", b""),
    (b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}", b""),
    (b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":1.0}", b""),
];

const READY_RESP: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";

static INDEX: OnceLock<IvfIndex> = OnceLock::new();

#[derive(Clone, Copy)]
struct Config {
    nprobe:     usize,
    fast_nprobe: usize,
    adapt_min:  u8,
    adapt_max:  u8,
    thr0:       u64,
    thr1:       u64,
    thr5:       u64,
    thr_any:    u64,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn do_search(q: &[i16; index::DIMS]) -> u8 {
    let idx = INDEX.get().unwrap();
    let cfg = CONFIG.get().unwrap();
    let qp = q.as_ptr();

    if cfg.fast_nprobe > 0 && cfg.fast_nprobe < cfg.nprobe {
        let tr = if cfg.fast_nprobe == 1 {
            idx.query_top1(qp)
        } else {
            idx.query_topn(qp, cfg.fast_nprobe)
        };
        let r = tr.fraud;
        if r < cfg.adapt_min || r > cfg.adapt_max {
            let thr = match r { 0 => cfg.thr0, 1 => cfg.thr1, 5 => cfg.thr5, _ => 0 };
            let rerun = (cfg.thr_any > 0 && tr.final_worst >= cfg.thr_any)
                     || (thr > 0 && tr.final_worst >= thr);
            if rerun {
                return idx.query_topn(qp, cfg.nprobe).fraud.min(5);
            }
            return r.min(5);
        }
    }
    idx.query_topn(qp, cfg.nprobe).fraud.min(5)
}

fn http_path(buf: &[u8]) -> &[u8] {
    let a = match buf.iter().position(|&b| b == b' ') { Some(i) => i + 1, None => return b"" };
    let b = match buf[a..].iter().position(|&b| b == b' ') { Some(i) => a + i, None => return b"" };
    &buf[a..b]
}

fn http_content_length(hdr: &[u8]) -> usize {
    for tag in [b"Content-Length: ".as_slice(), b"content-length: ".as_slice()] {
        if let Some(pos) = hdr.windows(tag.len()).position(|w| w == tag) {
            let mut p = pos + tag.len();
            while p < hdr.len() && hdr[p] == b' ' { p += 1; }
            let mut n = 0usize;
            while p < hdr.len() && hdr[p] >= b'0' && hdr[p] <= b'9' {
                n = n * 10 + (hdr[p] - b'0') as usize; p += 1;
            }
            return n;
        }
    }
    // Try without space after colon
    for tag in [b"Content-Length:".as_slice(), b"content-length:".as_slice()] {
        if let Some(pos) = hdr.windows(tag.len()).position(|w| w == tag) {
            let mut p = pos + tag.len();
            while p < hdr.len() && hdr[p] == b' ' { p += 1; }
            let mut n = 0usize;
            while p < hdr.len() && hdr[p] >= b'0' && hdr[p] <= b'9' {
                n = n * 10 + (hdr[p] - b'0') as usize; p += 1;
            }
            return n;
        }
    }
    0
}

fn serve_conn<R: Read + Write>(mut stream: R) {
    // Set TCP_NODELAY + TCP_QUICKACK via raw fd if possible — done by caller for TCP
    let mut buf = [0u8; 8192];
    let mut have = 0usize;

    'outer: loop {
        // Read more data
        let n = match stream.read(&mut buf[have..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        have += n;

        let mut consumed = 0usize;
        loop {
            let data = &buf[consumed..have];
            let hdr_end = match data.windows(4).position(|w| w == b"\r\n\r\n") {
                Some(i) => i,
                None => break,
            };
            let hdr = &data[..hdr_end + 4];
            let clen = http_content_length(hdr);
            let need = hdr_end + 4 + clen;
            if data.len() < need { break; }

            let path = http_path(hdr);
            let body = &data[hdr_end + 4..hdr_end + 4 + clen];

            let resp: &[u8] = if path == b"/fraud-score" {
                let mut q = [0i16; index::DIMS];
                let b = if tx::extract(body, &mut q) { do_search(&q) } else { 0 };
                let b = b.min(5) as usize;
                FRAUD_RESP[b].0
            } else if path == b"/ready" {
                READY_RESP
            } else {
                FRAUD_RESP[0].0
            };

            if stream.write_all(resp).is_err() { break 'outer; }
            consumed += need;
        }

        if consumed > 0 {
            have -= consumed;
            if have > 0 { buf.copy_within(consumed..consumed + have, 0); }
        }
        if have == buf.len() { break; } // oversized request
    }
}

fn set_tcp_opts(stream: &TcpStream) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        let fd = stream.as_raw_fd();
        let one: libc::c_int = 1;
        libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY,
            &one as *const _ as *const libc::c_void, std::mem::size_of_val(&one) as libc::socklen_t);
        libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_QUICKACK,
            &one as *const _ as *const libc::c_void, std::mem::size_of_val(&one) as libc::socklen_t);
    }
}

fn spawn_thread<F: FnOnce() + Send + 'static>(f: F) {
    thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(f)
        .ok();
}

/// Receive one fd from the LB's control socket via SCM_RIGHTS.
fn recv_fd(ctrl: &UnixStream) -> Option<i32> {
    use std::os::unix::io::AsRawFd;
    let ctrl_fd = ctrl.as_raw_fd();
    unsafe {
        let mut byte = 0u8;
        let mut iov = libc::iovec { iov_base: &mut byte as *mut u8 as *mut _, iov_len: 1 };
        let mut cmsgbuf = [0u8; 64]; // CMSG_SPACE(sizeof(int)) = 24 on Linux/amd64
        let mut mh: libc::msghdr = std::mem::zeroed();
        mh.msg_iov        = &mut iov;
        mh.msg_iovlen     = 1;
        mh.msg_control    = cmsgbuf.as_mut_ptr() as *mut _;
        mh.msg_controllen = cmsgbuf.len() as _;
        loop {
            let n = libc::recvmsg(ctrl_fd, &mut mh, 0);
            if n < 0 && *libc::__errno_location() == libc::EINTR { continue; }
            if n <= 0 { return None; }
            break;
        }
        let mut cmsg = libc::CMSG_FIRSTHDR(&mh);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut fd = 0i32;
                std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg), &mut fd as *mut i32 as *mut u8, 4);
                return Some(fd);
            }
            cmsg = libc::CMSG_NXTHDR(&mh, cmsg);
        }
        None
    }
}

fn run_ctrl_loop(ctrl: UnixStream) {
    loop {
        match recv_fd(&ctrl) {
            None => break,
            Some(raw_fd) => {
                spawn_thread(move || {
                    let stream = unsafe {
                        use std::os::unix::io::FromRawFd;
                        TcpStream::from_raw_fd(raw_fd)
                    };
                    set_tcp_opts(&stream);
                    serve_conn(stream);
                });
            }
        }
    }
}

fn main() {
    // Suppress SIGPIPE
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    let idx_path   = std::env::var("INDEX_PATH").unwrap_or_else(|_| "/index/index.bin".into());
    let listen_env = std::env::var("LISTEN").ok();
    let tcp_port   = std::env::var("TCP_PORT").ok()
        .and_then(|v| v.parse::<u16>().ok());

    let cfg = Config {
        nprobe:      env_usize("NPROBE",     64),
        fast_nprobe: env_usize("FAST_NPROBE", 4),
        adapt_min:   env_u8("ADAPTIVE_MIN",  2),
        adapt_max:   env_u8("ADAPTIVE_MAX",  4),
        thr0:        env_u64("EXTREME0_WORST_THRESHOLD", 3501932),
        thr1:        env_u64("EXTREME1_WORST_THRESHOLD", 3569273),
        thr5:        env_u64("EXTREME5_WORST_THRESHOLD", 4594089),
        thr_any:     env_u64("EXTREME_WORST_THRESHOLD",  0),
    };
    CONFIG.set(cfg).ok();

    // mlockall to keep pages resident
    unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE); }

    // Load index (must happen before we start serving)
    INDEX.set(IvfIndex::open(&idx_path)).ok();

    if let Some(port) = tcp_port {
        // Direct TCP mode (dev local, SO_REUSEPORT)
        let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind");
        for stream in listener.incoming().flatten() {
            set_tcp_opts(&stream);
            spawn_thread(move || serve_conn(stream));
        }
    } else {
        // LB mode: accept control connections from lb, receive fds via SCM_RIGHTS
        let ctrl_path = format!("{}.ctrl",
            listen_env.as_deref().unwrap_or("/sockets/api.sock"));
        let _ = std::fs::remove_file(&ctrl_path);
        let srv = UnixListener::bind(&ctrl_path).expect("bind ctrl");
        // chmod 777 so lb can connect
        let _ = std::process::Command::new("chmod").args(["0777", &ctrl_path]).status();

        for stream in srv.incoming().flatten() {
            spawn_thread(move || run_ctrl_loop(stream));
        }
    }
}

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn env_u8(k: &str, d: u8) -> u8 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
