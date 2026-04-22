use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::{fs, thread};

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

    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let file_path = if path == "/" || path == "/index.html" {
        dir.join("index.html")
    } else {
        let clean = path.trim_start_matches('/');
        if clean.contains("..") {
            respond(stream, 403, "text/plain", b"forbidden");
            return;
        }
        dir.join(clean)
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
            Ok(body) => respond(stream, 200, ct, &body),
            Err(_) => respond(stream, 500, "text/plain", b"read error"),
        }
    } else if !path.contains('.') {
        // SPA fallback: any extension-less path that isn't a real file
        // (e.g. /p/3/s/12, /dash/p/0) is a client-side route. Serve
        // index.html so the JS router can take over and parse the path.
        let index = dir.join("index.html");
        match fs::read(&index) {
            Ok(body) => respond(stream, 200, "text/html; charset=utf-8", &body),
            Err(_) => respond(stream, 404, "text/plain", b"not found"),
        }
    } else {
        respond(stream, 404, "text/plain", b"not found");
    }
}

fn respond(stream: &mut impl Write, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}
