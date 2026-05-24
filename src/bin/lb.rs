/// Load balancer: accepts TCP on :PORT, round-robins client fds to api ctrl sockets via SCM_RIGHTS.
use std::net::TcpListener;
use std::os::unix::net::UnixStream;

fn connect_ctrl(path: &str) -> Option<UnixStream> {
    for _ in 0..600 { // 60s total (100ms intervals)
        if let Ok(s) = UnixStream::connect(path) { return Some(s); }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

fn pass_fd(ctrl: &UnixStream, client_fd: i32) -> bool {
    use std::os::unix::io::AsRawFd;
    let ctrl_fd = ctrl.as_raw_fd();
    unsafe {
        let mut byte = b'!';
        let mut iov = libc::iovec { iov_base: &mut byte as *mut u8 as *mut _, iov_len: 1 };
        let mut cmsgbuf = [0u8; 64]; // CMSG_SPACE(sizeof(int)) = 24 on Linux/amd64
        let mut mh: libc::msghdr = std::mem::zeroed();
        mh.msg_iov        = &mut iov;
        mh.msg_iovlen     = 1;
        mh.msg_control    = cmsgbuf.as_mut_ptr() as *mut _;
        mh.msg_controllen = cmsgbuf.len() as _;
        let cm = libc::CMSG_FIRSTHDR(&mh);
        (*cm).cmsg_level = libc::SOL_SOCKET;
        (*cm).cmsg_type  = libc::SCM_RIGHTS;
        (*cm).cmsg_len   = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping(&client_fd as *const i32 as *const u8, libc::CMSG_DATA(cm), 4);
        libc::sendmsg(ctrl_fd, &mh, libc::MSG_NOSIGNAL) > 0
    }
}

fn main() {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    let port = std::env::var("PORT").ok()
        .and_then(|v| v.parse::<u16>().ok()).unwrap_or(9999);

    let ups_env = std::env::var("UPSTREAMS").expect("UPSTREAMS required");
    let ctrl_paths: Vec<String> = ups_env.split(',')
        .map(|s| format!("{}.ctrl", s.trim()))
        .collect();

    eprintln!("lb: port={port} upstreams={} connecting...", ctrl_paths.len());

    // Preflight: connect to all upstreams BEFORE binding (avoids health-check timeout)
    let mut ctrl_fds: Vec<UnixStream> = ctrl_paths.iter().map(|p| {
        let s = connect_ctrl(p).unwrap_or_else(|| {
            eprintln!("lb: failed to connect {p}"); std::process::exit(1);
        });
        eprintln!("lb: connected to {p}");
        s
    }).collect();

    let listener = {
        use std::os::unix::io::AsRawFd;
        let sock = unsafe {
            libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0)
        };
        assert!(sock >= 0);
        let one: libc::c_int = 1;
        unsafe {
            libc::setsockopt(sock, libc::SOL_SOCKET, libc::SO_REUSEADDR,
                &one as *const _ as *const _, std::mem::size_of_val(&one) as _);
            let defer: libc::c_int = 1;
            libc::setsockopt(sock, libc::IPPROTO_TCP, libc::TCP_DEFER_ACCEPT,
                &defer as *const _ as *const _, std::mem::size_of_val(&defer) as _);
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as _;
            addr.sin_addr.s_addr = libc::INADDR_ANY;
            addr.sin_port = port.to_be();
            assert_eq!(libc::bind(sock, &addr as *const _ as *const _, std::mem::size_of_val(&addr) as _), 0);
            assert_eq!(libc::listen(sock, 65535), 0);
            use std::os::unix::io::FromRawFd;
            TcpListener::from_raw_fd(sock)
        }
    };
    eprintln!("lb: listening on port {port}");

    let n = ctrl_paths.len();
    let mut rr = 0usize;

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        use std::os::unix::io::AsRawFd;
        let client_fd = stream.as_raw_fd();

        let mut sent = false;
        for attempt in 0..n {
            let idx = (rr + attempt) % n;
            if pass_fd(&ctrl_fds[idx], client_fd) {
                rr = (idx + 1) % n;
                sent = true;
                break;
            }
            // reconnect
            if let Some(s) = connect_ctrl(&ctrl_paths[idx]) {
                ctrl_fds[idx] = s;
                if pass_fd(&ctrl_fds[idx], client_fd) {
                    rr = (idx + 1) % n;
                    sent = true;
                    break;
                }
            }
        }
        if !sent { eprintln!("lb: dropped connection"); }
        // client_fd is owned by the TcpStream, dropped here — kernel dups via SCM_RIGHTS
        drop(stream);
    }
}
