use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;
use std::{fs, thread};

// Prints the "serving at http://…" banner to stderr before blocking on accept.
#[allow(clippy::print_stderr)]
pub fn serve(dir: &Path, port: u16) -> std::io::Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)?;
    eprintln!("serving at http://{addr}");

    let url = format!("http://{addr}");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).status();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).status();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", &url])
        .status();

    let dir = dir.to_path_buf();
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // Cap how long a connection can sit before sending its request line,
        // so a client that connects and stays silent can't pin a thread.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let dir = dir.clone();
        let _ = thread::spawn(move || {
            handle_request(&mut stream, &dir);
        });
    }
    Ok(())
}

#[allow(clippy::indexing_slicing)] // n is bounded by buf.len() from read()
fn handle_request(stream: &mut (impl Read + Write), dir: &Path) {
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]);

    let request_line = request.lines().next().unwrap_or("");
    // HEAD must return the same status line + headers (incl. Content-Length)
    // as GET but with an empty body; parse the method so `respond` can drop
    // the body. Anything that isn't HEAD is treated as GET.
    let is_head = request_line
        .split_whitespace()
        .next()
        .is_some_and(|m| m.eq_ignore_ascii_case("HEAD"));

    // Strip the query/fragment so paths like `/index.html?v=1` and
    // `/foo#anchor` resolve to a file on disk.
    let path = request_line
        .split_whitespace()
        .nth(1)
        .map(|p| p.split(['?', '#']).next().unwrap_or(p))
        .unwrap_or("/");

    let file_path = if path == "/" || path == "/index.html" {
        dir.join("index.html")
    } else {
        // Decode `%xx` then verify the canonical path stays inside `dir`.
        // `.contains("..")` alone misses URL-encoded variants (`%2e%2e`),
        // backslash on Windows, and absolute `/etc/...` paths.
        let clean = path.trim_start_matches('/');
        let decoded = percent_decode(clean);
        let candidate = dir.join(&decoded);
        if !path_within(dir, &candidate) {
            respond(stream, 403, "text/plain", b"forbidden", is_head);
            return;
        }
        candidate
    };

    if file_path.is_file() {
        let ct = match file_path.extension().and_then(|e| e.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("json") => "application/json",
            Some("js") => "text/javascript",
            Some("css") => "text/css",
            _ => "application/octet-stream",
        };
        match fs::read(&file_path) {
            Ok(body) => respond(stream, 200, ct, &body, is_head),
            Err(_) => respond(stream, 500, "text/plain", b"read error", is_head),
        }
    } else if !path.contains('.') {
        // SPA fallback: any extension-less path that isn't a real file
        // (e.g. /p/3/s/12, /dash/p/0) is a client-side route. Serve
        // index.html so the JS router can take over and parse the path.
        let index = dir.join("index.html");
        match fs::read(&index) {
            Ok(body) => respond(stream, 200, "text/html; charset=utf-8", &body, is_head),
            Err(_) => respond(stream, 404, "text/plain", b"not found", is_head),
        }
    } else {
        respond(stream, 404, "text/plain", b"not found", is_head);
    }
}

// Minimal %xx decoder — enough for the generated SPA's static files.
// We only feed the result into `dir.join` and then `path_within` rejects
// anything that escapes `dir`, so an incomplete `%xx` sequence is safe
// (it just won't match a file).
#[allow(clippy::indexing_slicing)] // i and i+2 are bounds-checked against bytes.len()
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// True iff `candidate` resolves to a path inside `dir`. Canonicalize
// both sides (resolves `..` and symlinks); if `candidate` doesn't yet
// exist, walk it lexically using the canonical `dir` as the prefix.
fn path_within(dir: &Path, candidate: &Path) -> bool {
    let Ok(canon_dir) = dir.canonicalize() else {
        return false;
    };
    if let Ok(canon) = candidate.canonicalize() {
        return canon.starts_with(&canon_dir);
    }
    // Path doesn't exist (404 case): compose with `canon_dir` and
    // lexically resolve `..` segments. Any segment that pops above
    // `canon_dir` is rejected.
    use std::path::Component;
    let rel = candidate.strip_prefix(dir).unwrap_or(candidate);
    let mut depth: i32 = 0;
    for c in rel.components() {
        match c {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn respond(stream: &mut impl Write, status: u16, content_type: &str, body: &[u8], is_head: bool) {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    // HEAD keeps the identical Content-Length header (computed from the
    // body that GET would have sent) but writes no body.
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    if !is_head {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}
