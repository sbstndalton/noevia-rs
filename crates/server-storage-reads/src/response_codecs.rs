//! Private incremental entity pipeline. Counters are logical, not physical heap bounds.
use brotli_decompressor::{BrotliDecompressStream, BrotliResult, BrotliState, StandardAlloc};
use flate2::{bufread::MultiGzDecoder, Decompress, FlushDecompress, Status};
use hyper::http::HeaderMap;
use std::{
    io::{self, BufRead, Read},
    time::Instant,
};
pub(crate) const STEP: usize = 4096;
pub(crate) const ENTITY: usize = 64 * 1024 * 1024;
pub(crate) fn entity_total(current: usize, added: usize) -> Result<usize, Fault> {
    current
        .checked_add(added)
        .filter(|n| *n <= ENTITY)
        .ok_or(Fault::Body)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    Body,
    Timeout,
}
#[derive(Clone, Copy)]
enum Kind {
    Gzip,
    Deflate,
    Brotli,
}
pub(crate) struct Plan {
    kinds: [Option<Kind>; 5],
    count: usize,
}
impl Plan {
    pub(crate) fn headers(headers: &HeaderMap) -> Result<Self, Fault> {
        let mut kinds = [None; 5];
        let mut count = 0usize;
        let mut unknown = false;
        let values = headers.get_all("content-encoding");
        let mut iter = values.iter().peekable();
        while let Some(value) = iter.next() {
            if count == 0 && value.as_bytes().is_empty() && iter.peek().is_none() {
                break;
            }
            for token in value.as_bytes().split(|byte| *byte == b',') {
                if count == 5 {
                    return Err(Fault::Body);
                }
                let token = token.trim_ascii();
                // Latin1 JS trim additionally includes NBSP; CR/LF are framing-invalid.
                let mut start = 0;
                let mut end = token.len();
                while token
                    .get(start)
                    .is_some_and(|b| b.is_ascii_whitespace() || *b == 0xa0)
                {
                    start += 1;
                }
                while end > start
                    && token
                        .get(end - 1)
                        .is_some_and(|b| b.is_ascii_whitespace() || *b == 0xa0)
                {
                    end -= 1;
                }
                let token = token.get(start..end).ok_or(Fault::Body)?;
                let kind = if token.eq_ignore_ascii_case(b"gzip")
                    || token.eq_ignore_ascii_case(b"x-gzip")
                {
                    Some(Kind::Gzip)
                } else if token.eq_ignore_ascii_case(b"deflate") {
                    Some(Kind::Deflate)
                } else if token.eq_ignore_ascii_case(b"br") {
                    Some(Kind::Brotli)
                } else {
                    unknown = true;
                    None
                };
                *kinds.get_mut(count).ok_or(Fault::Body)? = kind;
                count += 1;
            }
        }
        if unknown {
            return Ok(Self {
                kinds: [None; 5],
                count: 0,
            });
        }
        kinds.get_mut(..count).ok_or(Fault::Body)?.reverse();
        Ok(Self { kinds, count })
    }
}
struct Feed {
    bytes: [u8; STEP],
    position: usize,
    length: usize,
    eof: bool,
}
impl Default for Feed {
    fn default() -> Self {
        Self {
            bytes: [0; STEP],
            position: 0,
            length: 0,
            eof: false,
        }
    }
}
impl Feed {
    fn empty(&self) -> bool {
        self.position == self.length
    }
    fn put(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        if !self.empty() || self.eof || bytes.len() > STEP {
            return Err(Fault::Body);
        }
        self.bytes
            .get_mut(..bytes.len())
            .ok_or(Fault::Body)?
            .copy_from_slice(bytes);
        self.position = 0;
        self.length = bytes.len();
        Ok(())
    }
    fn input(&self) -> Result<&[u8], Fault> {
        self.bytes
            .get(self.position..self.length)
            .ok_or(Fault::Body)
    }
    fn consume_checked(&mut self, amount: usize) -> Result<(), Fault> {
        self.position = self.position.checked_add(amount).ok_or(Fault::Body)?;
        if self.position > self.length {
            return Err(Fault::Body);
        }
        Ok(())
    }
}
impl Read for Feed {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let input = self.fill_buf()?;
        let n = input.len().min(out.len());
        out.get_mut(..n)
            .ok_or_else(|| io::Error::other("range"))?
            .copy_from_slice(input.get(..n).ok_or_else(|| io::Error::other("range"))?);
        self.consume(n);
        Ok(n)
    }
}
impl BufRead for Feed {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.empty() && !self.eof {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.input().map_err(|_| io::Error::other("range"))
    }
    fn consume(&mut self, amount: usize) {
        self.position = self.position.saturating_add(amount).min(self.length);
    }
}
type Br = BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>;
enum Codec {
    Gzip(MultiGzDecoder<Feed>),
    Deflate {
        decoder: Option<Decompress>,
        feed: Feed,
    },
    Brotli {
        decoder: Box<Br>,
        feed: Feed,
        total: usize,
    },
}
struct Stage {
    codec: Codec,
    ended: bool,
    forwarded_eof: bool,
    produced: usize,
}
impl Stage {
    fn feed(&mut self) -> &mut Feed {
        match &mut self.codec {
            Codec::Gzip(g) => g.get_mut(),
            Codec::Deflate { feed, .. } | Codec::Brotli { feed, .. } => feed,
        }
    }
    fn new(kind: Kind) -> Self {
        let codec = match kind {
            Kind::Gzip => Codec::Gzip(MultiGzDecoder::new(Feed::default())),
            Kind::Deflate => Codec::Deflate {
                decoder: None,
                feed: Feed::default(),
            },
            Kind::Brotli => Codec::Brotli {
                decoder: Box::new(Br::new_strict(
                    StandardAlloc::default(),
                    StandardAlloc::default(),
                    StandardAlloc::default(),
                )),
                feed: Feed::default(),
                total: 0,
            },
        };
        Self {
            codec,
            ended: false,
            forwarded_eof: false,
            produced: 0,
        }
    }
    fn step(&mut self, output: &mut [u8]) -> Result<(usize, bool), Fault> {
        if self.ended {
            let feed = self.feed();
            if !feed.empty() {
                return Err(Fault::Body);
            }
            return Ok((0, feed.eof));
        }
        let before = {
            let f = self.feed();
            (f.position, f.length)
        };
        let n = match &mut self.codec {
            Codec::Gzip(g) => match g.read(output) {
                Ok(0) => {
                    self.ended = true;
                    0
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                Err(_) => return Err(Fault::Body),
            },
            Codec::Deflate { decoder, feed } => {
                if decoder.is_none() {
                    if let Some(first) = feed.input()?.first() {
                        let zlib_header = (*first & 15) == 8;
                        *decoder = Some(Decompress::new(zlib_header));
                    } else if feed.eof {
                        return Err(Fault::Body);
                    } else {
                        return Ok((0, false));
                    }
                }
                let d = decoder.as_mut().ok_or(Fault::Body)?;
                let (bi, bo) = (d.total_in(), d.total_out());
                let status = d
                    .decompress(feed.input()?, output, FlushDecompress::None)
                    .map_err(|_| Fault::Body)?;
                let used = usize::try_from(d.total_in().checked_sub(bi).ok_or(Fault::Body)?)
                    .map_err(|_| Fault::Body)?;
                let n = usize::try_from(d.total_out().checked_sub(bo).ok_or(Fault::Body)?)
                    .map_err(|_| Fault::Body)?;
                feed.consume_checked(used)?;
                if status == Status::StreamEnd {
                    self.ended = true;
                }
                if !self.ended && feed.eof && used == 0 && n == 0 {
                    return Err(Fault::Body);
                }
                n
            }
            Codec::Brotli {
                decoder,
                feed,
                total,
            } => {
                let mut available_in = feed.input()?.len();
                let mut input_offset = 0;
                let mut available_out = output.len();
                let mut output_offset = 0;
                let result = BrotliDecompressStream(
                    &mut available_in,
                    &mut input_offset,
                    feed.input()?,
                    &mut available_out,
                    &mut output_offset,
                    output,
                    total,
                    decoder,
                );
                feed.consume_checked(input_offset)?;
                match result {
                    BrotliResult::ResultFailure => return Err(Fault::Body),
                    BrotliResult::ResultSuccess => self.ended = true,
                    BrotliResult::NeedsMoreInput if feed.eof => return Err(Fault::Body),
                    _ => {}
                }
                output_offset
            }
        };
        let (after, eof) = {
            let f = self.feed();
            ((f.position, f.length), f.eof)
        };
        if self.ended && !self.feed().empty() {
            return Err(Fault::Body);
        }
        Ok((n, n != 0 || before != after || (self.ended && eof)))
    }
}
pub(crate) enum Progress {
    Input,
    Complete,
    Capped,
}
pub(crate) struct Pipeline {
    stages: [Option<Stage>; 5],
    count: usize,
    encoded: usize,
    deadline: Instant,
}
impl Pipeline {
    pub(crate) fn new(plan: Plan, deadline: Instant) -> Result<Self, Fault> {
        let mut pipeline = Self {
            stages: std::array::from_fn(|_| None),
            count: plan.count,
            encoded: 0,
            deadline,
        };
        for i in 0..plan.count {
            pipeline.check()?;
            *pipeline.stages.get_mut(i).ok_or(Fault::Body)? = Some(Stage::new(
                plan.kinds.get(i).copied().flatten().ok_or(Fault::Body)?,
            ));
            pipeline.check()?;
        }
        pipeline.check()?;
        Ok(pipeline)
    }
    pub(crate) fn compressed(&self) -> bool {
        self.count != 0
    }
    fn check(&self) -> Result<(), Fault> {
        if Instant::now() >= self.deadline {
            Err(Fault::Timeout)
        } else {
            Ok(())
        }
    }
    pub(crate) fn feed(
        &mut self,
        bytes: &[u8],
        payload: &mut Vec<u8>,
        cap: usize,
    ) -> Result<Progress, Fault> {
        self.check()?;
        self.encoded = entity_total(self.encoded, bytes.len())?;
        if self.count == 0 {
            let take = bytes.len().min(cap + 1 - payload.len());
            self.check()?;
            payload.extend_from_slice(bytes.get(..take).ok_or(Fault::Body)?);
            return Ok(if payload.len() == cap + 1 {
                Progress::Capped
            } else {
                Progress::Input
            });
        }
        self.stages
            .get_mut(0)
            .and_then(Option::as_mut)
            .ok_or(Fault::Body)?
            .feed()
            .put(bytes)?;
        self.check()?;
        self.pump(payload, cap)
    }
    pub(crate) fn finish(&mut self, payload: &mut Vec<u8>, cap: usize) -> Result<Progress, Fault> {
        self.check()?;
        if self.count == 0 {
            return Ok(Progress::Complete);
        }
        self.stages
            .get_mut(0)
            .and_then(Option::as_mut)
            .ok_or(Fault::Body)?
            .feed()
            .eof = true;
        self.pump(payload, cap)
    }
    fn pump(&mut self, payload: &mut Vec<u8>, cap: usize) -> Result<Progress, Fault> {
        let mut scratch = [0; STEP];
        loop {
            self.check()?;
            let mut progress = false;
            for i in (0..self.count).rev() {
                self.check()?;
                let final_stage = i + 1 == self.count;
                if !final_stage
                    && !self
                        .stages
                        .get_mut(i + 1)
                        .and_then(Option::as_mut)
                        .ok_or(Fault::Body)?
                        .feed()
                        .empty()
                {
                    continue;
                }
                let space = if final_stage {
                    STEP.min(cap + 1 - payload.len())
                } else {
                    STEP
                };
                if space == 0 {
                    return Ok(Progress::Capped);
                }
                let stage = self
                    .stages
                    .get_mut(i)
                    .and_then(Option::as_mut)
                    .ok_or(Fault::Body)?;
                let was_ended = stage.ended;
                let (n, changed) = stage.step(scratch.get_mut(..space).ok_or(Fault::Body)?)?;
                if n > space {
                    return Err(Fault::Body);
                }
                progress |= changed || was_ended != stage.ended;
                if !final_stage {
                    stage.produced = entity_total(stage.produced, n)?;
                }
                let eof = stage.ended && stage.feed().empty() && stage.feed().eof;
                let forward_eof = eof && !stage.forwarded_eof;
                if forward_eof {
                    stage.forwarded_eof = true;
                    progress = true;
                }
                self.check()?;
                if final_stage {
                    payload.extend_from_slice(scratch.get(..n).ok_or(Fault::Body)?);
                    if payload.len() == cap + 1 {
                        return Ok(Progress::Capped);
                    }
                    if eof {
                        return Ok(Progress::Complete);
                    }
                } else {
                    let next = self
                        .stages
                        .get_mut(i + 1)
                        .and_then(Option::as_mut)
                        .ok_or(Fault::Body)?
                        .feed();
                    if n != 0 {
                        next.put(scratch.get(..n).ok_or(Fault::Body)?)?;
                    }
                    if forward_eof {
                        next.eof = true;
                    }
                }
                self.check()?;
            }
            if !progress {
                if self
                    .stages
                    .get_mut(0)
                    .and_then(Option::as_mut)
                    .ok_or(Fault::Body)?
                    .feed()
                    .empty()
                {
                    return Ok(Progress::Input);
                }
                return Err(Fault::Body);
            }
        }
    }
}
