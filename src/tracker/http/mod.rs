mod reader;
mod writer;

use std::time::{Instant, Duration};
use std::mem;
use std::io;
use std::collections::HashMap;
use std::net::SocketAddr;

use url::percent_encoding::percent_encode_byte;
use url::Url;

use {PEER_ID, bencode, amy};
use self::writer::Writer;
use self::reader::{Reader, ReadRes};
use socket::TSocket;
use tracker::{self, Announce, Response, TrackerResponse, Result, ResultExt, Error, ErrorKind, dns};

const TIMEOUT_MS: u64 = 5_000;

pub struct Handler {
    reg: amy::Registrar,
    connections: HashMap<usize, Tracker>,
}

enum Event {
    DNSResolved(dns::QueryResponse),
    Readable,
    Writable,
}

struct Tracker {
    torrent: usize,
    last_updated: Instant,
    redirect: bool,
    state: TrackerState,
}

enum TrackerState {
    Error,
    ResolvingDNS {
        sock: TSocket,
        req: Vec<u8>,
        port: u16,
    },
    Writing { sock: TSocket, writer: Writer },
    Reading { sock: TSocket, reader: Reader },
    Redirect(String),
    Complete(TrackerResponse),
}

enum HTTPRes {
    None,
    Redirect(String),
    Complete(TrackerResponse),
}

impl TrackerState {
    fn new(sock: TSocket, req: Vec<u8>, port: u16) -> TrackerState {
        TrackerState::ResolvingDNS { sock, req, port }
    }

    fn handle(&mut self, event: Event) -> Result<HTTPRes> {
        let s = mem::replace(self, TrackerState::Error);
        match s.next(event)? {
            TrackerState::Complete(r) => Ok(HTTPRes::Complete(r)),
            TrackerState::Redirect(l) => Ok(HTTPRes::Redirect(l)),
            n => {
                mem::replace(self, n);
                Ok(HTTPRes::None)
            }
        }
    }

    fn next(self, event: Event) -> Result<TrackerState> {
        match (self, event) {
            (TrackerState::ResolvingDNS { sock, req, port }, Event::DNSResolved(r)) => {
                let addr = SocketAddr::new(r.res?, port);
                sock.connect(addr).chain_err(|| ErrorKind::IO)?;
                Ok(TrackerState::Writing {
                    sock,
                    writer: Writer::new(req),
                }.next(Event::Writable)?)
            }
            (TrackerState::Writing {
                 mut sock,
                 mut writer,
             },
             Event::Writable) => {
                match writer.writable(&mut sock.conn)? {
                    Some(()) => {
                        let r = Reader::new();
                        Ok(TrackerState::Reading { sock, reader: r }.next(Event::Readable)?)
                    }
                    None => Ok(TrackerState::Writing { sock, writer }),
                }
            }
            (TrackerState::Reading {
                 mut sock,
                 mut reader,
             },
             Event::Readable) => {
                match reader.readable(&mut sock.conn)? {
                    ReadRes::Done(data) => {
                        let content = bencode::decode_buf(&data).chain_err(|| {
                            ErrorKind::InvalidResponse("Invalid BEncoded response!")
                        })?;
                        let resp = TrackerResponse::from_bencode(content)?;
                        Ok(TrackerState::Complete(resp))
                    }
                    ReadRes::Redirect(l) => Ok(TrackerState::Redirect(l)),
                    ReadRes::None => Ok(TrackerState::Reading { sock, reader }),
                }
            }
            (s @ TrackerState::Writing { .. }, _) |
            (s @ TrackerState::Reading { .. }, _) |
            (s @ TrackerState::ResolvingDNS { .. }, _) => Ok(s),
            _ => bail!("Unknown state transition encountered!"),
        }
    }
}

impl Handler {
    pub fn new(reg: &amy::Registrar) -> io::Result<Handler> {
        Ok(Handler {
            reg: reg.try_clone()?,
            connections: HashMap::new(),
        })
    }

    pub fn active_requests(&self) -> usize {
        self.connections.len()
    }

    pub fn complete(&self) -> bool {
        self.connections.is_empty()
    }

    pub fn contains(&self, id: usize) -> bool {
        self.connections.contains_key(&id)
    }

    pub fn dns_resolved(&mut self, resp: dns::QueryResponse) -> Option<Response> {
        let id = resp.id;
        debug!("Received a DNS resp for {:?}", id);
        let resp = if let Some(trk) = self.connections.get_mut(&id) {
            trk.last_updated = Instant::now();
            match trk.state.handle(Event::DNSResolved(resp)) {
                Ok(_) => None,
                Err(e) => Some((trk.torrent, Err(e))),
            }
        } else {
            None
        };
        if resp.is_some() {
            self.connections.remove(&id);
        }
        resp
    }

    pub fn writable(&mut self, id: usize) -> Option<Response> {
        let resp = if let Some(trk) = self.connections.get_mut(&id) {
            trk.last_updated = Instant::now();
            match trk.state.handle(Event::Writable) {
                Ok(_) => None,
                Err(e) => Some((trk.torrent, Err(e))),
            }
        } else {
            None
        };
        if resp.is_some() {
            self.connections.remove(&id);
        }
        resp
    }

    pub fn readable(&mut self, id: usize, dns: &mut dns::Resolver) -> Option<Response> {
        let mut loc = None;
        let mut resp = if let Some(trk) = self.connections.get_mut(&id) {
            trk.last_updated = Instant::now();
            match trk.state.handle(Event::Readable) {
                Ok(HTTPRes::Complete(r)) => {
                    debug!("Announce response received for {:?} succesfully", id);
                    Some((trk.torrent, Ok(r)))
                }
                Ok(HTTPRes::Redirect(l)) => {
                    loc = Some(l);
                    None
                }
                Ok(HTTPRes::None) => None,
                Err(e) => Some((trk.torrent, Err(e))),
            }
        } else {
            None
        };

        if resp.is_some() {
            self.connections.remove(&id);
        }

        if let Some(l) = loc {
            let trk = self.connections.remove(&id).unwrap();
            // Disallow 2 levels of redirection
            if trk.redirect {
                resp = Some((
                    trk.torrent,
                    Err(ErrorKind::InvalidResponse("Too many redirects").into()),
                ));
            }
            if let Err(e) = self.try_redirect(&l, trk.torrent, dns) {
                debug!(
                    "Announce response received for {:?}, redirecting!",
                    trk.torrent
                );
                resp = Some((trk.torrent, Err(e)));
            }
        }
        resp
    }

    fn try_redirect(&mut self, url: &str, torrent: usize, dns: &mut dns::Resolver) -> Result<()> {
        let url = Url::parse(url).chain_err(|| {
            ErrorKind::InvalidResponse("Malformed redirect!")
        })?;
        let mut http_req = Vec::with_capacity(50);
        http_req.extend_from_slice(b"GET ");
        http_req.extend_from_slice(url.path().as_bytes());
        if let Some(q) = url.query() {
            http_req.extend_from_slice(b"?");
            http_req.extend_from_slice(url.query().unwrap().as_bytes());
        }

        http_req.extend_from_slice(b" HTTP/1.1\r\n");
        http_req.extend_from_slice(b"Host: ");
        let host = url.host_str().ok_or_else(|| {
            Error::from(ErrorKind::InvalidResponse("Malformed redirect!"))
        })?;
        let port = url.port().unwrap_or(80);
        http_req.extend_from_slice(host.as_bytes());
        http_req.extend_from_slice(b"\r\n\r\n");

        // Setup actual connection and start DNS query
        let (id, sock) = TSocket::new_v4(&self.reg).chain_err(|| ErrorKind::IO)?;
        self.connections.insert(
            id,
            Tracker {
                last_updated: Instant::now(),
                redirect: true,
                torrent,
                state: TrackerState::new(sock, http_req, port),
            },
        );

        debug!("Dispatching redirect DNS req, id {:?}", id);
        dns.new_query(id, host);
        Ok(())
    }

    pub fn tick(&mut self) -> Vec<Response> {
        let mut resps = Vec::new();
        self.connections.retain(
            |id, trk| if trk.last_updated.elapsed() >
                Duration::from_millis(TIMEOUT_MS)
            {
                resps.push((trk.torrent, Err(ErrorKind::Timeout.into())));
                debug!("Announce {:?} timed out", id);
                false
            } else {
                true
            },
        );
        resps
    }


    pub fn new_announce(
        &mut self,
        req: Announce,
        url: &Url,
        dns: &mut dns::Resolver,
    ) -> Result<()> {
        debug!("Received a new announce req for {:?}", url);
        let mut http_req = Vec::with_capacity(50);
        // Encode GET req
        http_req.extend_from_slice(b"GET ");

        // Encode the URL
        http_req.extend_from_slice(url.path().as_bytes());
        http_req.extend_from_slice(b"?");
        append_query_pair(&mut http_req, "info_hash", &encode_param(&req.hash));
        append_query_pair(&mut http_req, "peer_id", &encode_param(&PEER_ID[..]));
        append_query_pair(&mut http_req, "uploaded", &req.uploaded.to_string());
        append_query_pair(&mut http_req, "downloaded", &req.downloaded.to_string());
        append_query_pair(&mut http_req, "left", &req.left.to_string());
        append_query_pair(&mut http_req, "compact", "1");
        append_query_pair(&mut http_req, "port", &req.port.to_string());
        if let Some(nw) = req.num_want {
            append_query_pair(&mut http_req, "numwant", &nw.to_string());
        }
        match req.event {
            Some(tracker::Event::Started) => {
                append_query_pair(&mut http_req, "event", "started");
            }
            Some(tracker::Event::Stopped) => {
                append_query_pair(&mut http_req, "event", "stopped");
            }
            Some(tracker::Event::Completed) => {
                append_query_pair(&mut http_req, "event", "completed");
            }
            None => {}
        }

        // Encode HTTP protocol
        http_req.extend_from_slice(b" HTTP/1.1\r\n");
        // Encode host header
        http_req.extend_from_slice(b"Host: ");
        let host = url.host_str().ok_or_else(|| {
            Error::from(ErrorKind::InvalidRequest(
                "Tracker announce url has no host!".to_owned(),
            ))
        })?;
        let port = url.port().unwrap_or(80);
        http_req.extend_from_slice(host.as_bytes());
        http_req.extend_from_slice(b"\r\n");
        // Encode empty line to terminate request
        http_req.extend_from_slice(b"\r\n");

        // Setup actual connection and start DNS query
        let (id, sock) = TSocket::new_v4(&self.reg).chain_err(|| ErrorKind::IO)?;
        self.connections.insert(
            id,
            Tracker {
                last_updated: Instant::now(),
                torrent: req.id,
                state: TrackerState::new(sock, http_req, port),
                redirect: false,
            },
        );

        debug!("Dispatching DNS req, id {:?}", id);
        dns.new_query(id, host);

        Ok(())
    }
}

fn append_query_pair(s: &mut Vec<u8>, k: &str, v: &str) {
    s.extend_from_slice(k.as_bytes());
    s.extend_from_slice(b"=");
    s.extend_from_slice(v.as_bytes());
    s.extend_from_slice(b"&");
}

fn encode_param(data: &[u8]) -> String {
    let mut resp = String::new();
    for byte in data {
        let c = char::from(*byte);
        if (*byte > 0x20 && *byte < 0x7E) && (c.is_numeric() || c.is_alphabetic() || c == '-') {
            resp.push(c);
        } else {
            resp += percent_encode_byte(*byte);
        }
    }
    resp
}
