//! RESP (the Redis protocol): the value type, its parser and encoder, and the
//! small client the API uses. The server half is `cache.rs`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Value>>),
}

impl Value {
    pub fn ok() -> Self {
        Value::Simple("OK".into())
    }

    pub fn bulk(data: &[u8]) -> Self {
        Value::Bulk(Some(data.to_vec()))
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Value::Simple(s) => out.extend(format!("+{s}\r\n").as_bytes()),
            Value::Error(s) => out.extend(format!("-{s}\r\n").as_bytes()),
            Value::Int(n) => out.extend(format!(":{n}\r\n").as_bytes()),
            Value::Bulk(None) => out.extend(b"$-1\r\n"),
            Value::Bulk(Some(b)) => {
                out.extend(format!("${}\r\n", b.len()).as_bytes());
                out.extend(b);
                out.extend(b"\r\n");
            }
            Value::Array(None) => out.extend(b"*-1\r\n"),
            Value::Array(Some(items)) => {
                out.extend(format!("*{}\r\n", items.len()).as_bytes());
                for i in items {
                    i.encode(out);
                }
            }
        }
    }
}

/// A command as RESP: an array of bulk strings.
pub fn encode_command(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    Value::Array(Some(args.iter().map(|a| Value::bulk(a)).collect())).encode(&mut out);
    out
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn line<R: BufRead>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(64 * 1024).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// The largest bulk string accepted (bytes).
const MAX_BULK: i64 = 16 * 1024 * 1024;

/// Reads one value; `Ok(None)` at a clean end of stream. A line that does not
/// start with a RESP type byte is an inline command (`PING` typed by hand),
/// returned as an array of its words.
pub fn read_value<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
    let l = match line(r)? {
        Some(l) => l,
        None => return Ok(None),
    };
    if l.is_empty() {
        return Ok(Some(Value::Array(Some(Vec::new()))));
    }
    let (kind, body) = l.split_at(1);
    Ok(Some(match kind {
        "+" => Value::Simple(body.to_string()),
        "-" => Value::Error(body.to_string()),
        ":" => Value::Int(body.parse().map_err(|_| bad("bad integer"))?),
        "$" => {
            let len: i64 = body.parse().map_err(|_| bad("bad bulk length"))?;
            if len < 0 {
                Value::Bulk(None)
            } else if len > MAX_BULK {
                return Err(bad("bulk too large"));
            } else {
                let mut data = vec![0u8; len as usize + 2];
                r.read_exact(&mut data)?;
                data.truncate(len as usize);
                Value::Bulk(Some(data))
            }
        }
        "*" => {
            let n: i64 = body.parse().map_err(|_| bad("bad array length"))?;
            if n < 0 {
                Value::Array(None)
            } else if n > 1024 * 1024 {
                return Err(bad("array too large"));
            } else {
                let mut items = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    items.push(read_value(r)?.ok_or_else(|| bad("eof in array"))?);
                }
                Value::Array(Some(items))
            }
        }
        _ => Value::Array(Some(l.split_whitespace().map(|w| Value::bulk(w.as_bytes())).collect())),
    }))
}

/// A RESP client with one connection, reconnecting (once per call) on failure.
pub struct Client {
    host: String,
    port: u16,
    password: Option<String>,
    timeout: Duration,
    conn: Mutex<Option<(TcpStream, BufReader<TcpStream>)>>,
}

impl Client {
    pub fn new(host: &str, port: u16, password: Option<String>, timeout: Duration) -> Self {
        Client { host: host.to_string(), port, password, timeout, conn: Mutex::new(None) }
    }

    fn connect(&self) -> io::Result<(TcpStream, BufReader<TcpStream>)> {
        let addr = crate::http::resolve_one(&self.host, self.port)?;
        let s = TcpStream::connect_timeout(&addr, self.timeout)?;
        s.set_read_timeout(Some(self.timeout))?;
        s.set_write_timeout(Some(self.timeout))?;
        let mut conn = (s.try_clone()?, BufReader::new(s));
        if let Some(pw) = &self.password {
            if let Value::Error(e) = Self::exchange(&mut conn, &[b"AUTH", pw.as_bytes()])? {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, e));
            }
        }
        Ok(conn)
    }

    fn exchange(conn: &mut (TcpStream, BufReader<TcpStream>), args: &[&[u8]]) -> io::Result<Value> {
        conn.0.write_all(&encode_command(args))?;
        conn.0.flush()?;
        read_value(&mut conn.1)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "cache closed the connection"))
    }

    /// Runs a command. A RESP error reply is returned as `Err`.
    pub fn command(&self, args: &[&[u8]]) -> io::Result<Value> {
        let mut guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut last = None;
        for _ in 0..2 {
            if guard.is_none() {
                match self.connect() {
                    Ok(c) => *guard = Some(c),
                    Err(e) => {
                        last = Some(e);
                        continue;
                    }
                }
            }
            let conn = guard.as_mut().expect("connected above");
            match Self::exchange(conn, args) {
                Ok(Value::Error(e)) => return Err(io::Error::other(e)),
                Ok(v) => return Ok(v),
                Err(e) => {
                    *guard = None;
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("cache unreachable")))
    }

    pub fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match self.command(&[b"GET", key.as_bytes()])? {
            Value::Bulk(b) => Ok(b),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn setex(&self, key: &str, ttl: u64, value: &[u8]) -> io::Result<()> {
        self.command(&[b"SETEX", key.as_bytes(), ttl.to_string().as_bytes(), value]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_values() {
        let v = Value::Array(Some(vec![Value::ok(), Value::Int(-3), Value::Bulk(None), Value::bulk(b"a\r\nb")]));
        let mut out = Vec::new();
        v.encode(&mut out);
        assert_eq!(read_value(&mut Cursor::new(out)).unwrap().unwrap(), v);
    }

    #[test]
    fn inline_commands_are_words() {
        let v = read_value(&mut Cursor::new(b"PING hello\r\n".to_vec())).unwrap().unwrap();
        assert_eq!(v, Value::Array(Some(vec![Value::bulk(b"PING"), Value::bulk(b"hello")])));
    }
}
