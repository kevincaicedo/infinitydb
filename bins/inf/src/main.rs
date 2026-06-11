//! `inf` — minimal InfinityDB CLI (M0 dev tool).
//!
//! One-shot: `inf -p 6379 SET k v` · REPL: `inf -p 6379`.
//! Speaks RESP2/RESP3 replies (the server decides per HELLO; we print both).
//! Intentionally tiny and dependency-thin: this is a smoke-test tool, not a
//! product surface (`redis-cli` remains the reference client — L8).
#![forbid(unsafe_code)]

mod reply;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;

use reply::{Reply, parse_reply};

fn encode_command(args: &[String], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
}

struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    filled: usize,
}

impl Conn {
    fn connect(host: &str, port: u16) -> std::io::Result<Conn> {
        let stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true)?;
        Ok(Conn { stream, buf: vec![0; 64 * 1024], filled: 0 })
    }

    fn round_trip(&mut self, args: &[String]) -> std::io::Result<Reply> {
        let mut req = Vec::new();
        encode_command(args, &mut req);
        self.stream.write_all(&req)?;
        loop {
            if self.filled > 0
                && let Some((reply, used)) = parse_reply(&self.buf[..self.filled])
            {
                self.buf.copy_within(used..self.filled, 0);
                self.filled -= used;
                return Ok(reply);
            }
            if self.filled == self.buf.len() {
                self.buf.resize(self.buf.len() * 2, 0);
            }
            let n = self.stream.read(&mut self.buf[self.filled..])?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                ));
            }
            self.filled += n;
        }
    }
}

fn split_line(line: &str) -> Vec<String> {
    // Quote-aware splitter for the REPL ("…" and '…').
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in line.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        args.push(cur);
    }
    args
}

fn usage() -> ExitCode {
    eprintln!("usage: inf [-h HOST] [-p PORT] [COMMAND [ARG ...]]");
    eprintln!("       no COMMAND starts a REPL");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 6379;
    let mut rest: Vec<String> = Vec::new();

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" => match argv.next() {
                Some(v) => host = v,
                None => return usage(),
            },
            "-p" => match argv.next().and_then(|v| v.parse().ok()) {
                Some(v) => port = v,
                None => return usage(),
            },
            "--help" => return usage(),
            _ => {
                rest.push(arg);
                rest.extend(argv.by_ref());
            }
        }
    }

    let mut conn = match Conn::connect(&host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("inf: cannot connect to {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if !rest.is_empty() {
        return match conn.round_trip(&rest) {
            Ok(reply) => {
                println!("{}", reply.render(0));
                if matches!(reply, Reply::Error(_)) { ExitCode::FAILURE } else { ExitCode::SUCCESS }
            }
            Err(e) => {
                eprintln!("inf: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // REPL
    let stdin = std::io::stdin();
    let mut lines = BufReader::new(stdin.lock()).lines();
    loop {
        print!("{host}:{port}> ");
        let _ = std::io::stdout().flush();
        let Some(Ok(line)) = lines.next() else { break };
        let args = split_line(&line);
        if args.is_empty() {
            continue;
        }
        if args[0].eq_ignore_ascii_case("quit") || args[0].eq_ignore_ascii_case("exit") {
            break;
        }
        match conn.round_trip(&args) {
            Ok(reply) => println!("{}", reply.render(0)),
            Err(e) => {
                eprintln!("inf: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_encoding_is_resp() {
        let mut out = Vec::new();
        encode_command(&["SET".into(), "k".into(), "v1".into()], &mut out);
        assert_eq!(out, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\nv1\r\n");
    }

    #[test]
    fn repl_splitter_handles_quotes() {
        assert_eq!(split_line(r#"SET k "hello world""#), vec!["SET", "k", "hello world"]);
        assert_eq!(split_line("GET   k"), vec!["GET", "k"]);
        assert!(split_line("   ").is_empty());
    }
}
