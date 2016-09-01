//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//
//  This library is free software; you can redistribute it and/or
//  modify it under the terms of the GNU Library General Public
//  License as published by the Free Software Foundation; either
//  version 2 of the License, or (at your option) any later version.
//
//  This library is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
//  Library General Public License for more details.
//
//  You should have received a copy of the GNU Library General Public
//  License along with this library; if not, write to the
//  Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
//  Boston, MA 02110-1301, USA.

use std::u64;
use std::io::Read;
use url::Url;
use hyper::header::{ContentLength, ContentRange, ContentRangeSpec, Range, ByteRangeSpec,
                    AcceptRanges, RangeUnit};
use hyper::client::Client;
use hyper::client::response::Response;

use error::*;
use rssource::*;

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started {
        uri: Url,
        response: Response,
        seekable: bool,
        position: u64,
        size: u64,
        start: u64,
        stop: u64,
    },
}

#[derive(Debug)]
pub struct HttpSrc {
    streaming_state: StreamingState,
    client: Client,
}

impl HttpSrc {
    pub fn new() -> HttpSrc {
        HttpSrc {
            streaming_state: StreamingState::Stopped,
            client: Client::new(),
        }
    }

    pub fn new_boxed() -> Box<Source> {
        Box::new(HttpSrc::new())
    }

    fn do_request(&self, uri: Url, start: u64, stop: u64) -> Result<StreamingState, ErrorMessage> {
        let mut req = self.client.get(uri.clone());

        if start != 0 || stop != u64::MAX {
            req = if stop == u64::MAX {
                req.header(Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]))
            } else {
                req.header(Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop - 1)]))
            };
        }

        let response = try!(req.send().or_else(|err| {
            Err(error_msg!(SourceError::ReadFailed,
                           ["Failed to fetch {}: {}", uri, err.to_string()]))
        }));

        if !response.status.is_success() {
            return Err(error_msg!(SourceError::ReadFailed,
                                  ["Failed to fetch {}: {}", uri, response.status]));
        }

        let size = if let Some(&ContentLength(content_length)) = response.headers.get() {
            content_length + start
        } else {
            u64::MAX
        };

        let accept_byte_ranges = if let Some(&AcceptRanges(ref ranges)) = response.headers
            .get() {
            ranges.iter().any(|u| *u == RangeUnit::Bytes)
        } else {
            false
        };

        let seekable = size != u64::MAX && accept_byte_ranges;

        let position = if let Some(&ContentRange(ContentRangeSpec::Bytes { range: Some((range_start,
                                                                                 _)),
                                                                           .. })) = response.headers
            .get() {
            range_start
        } else {
            start
        };

        if position != start {
            return Err(error_msg!(SourceError::SeekFailed,
                                  ["Failed to seek to {}: Got {}", start, position]));
        }

        Ok(StreamingState::Started {
            uri: uri,
            response: response,
            seekable: seekable,
            position: 0,
            size: size,
            start: start,
            stop: stop,
        })
    }
}

fn validate_uri(uri: &Url) -> Result<(), UriError> {
    if uri.scheme() != "http" && uri.scheme() != "https" {
        return Err(UriError::new(UriErrorKind::UnsupportedProtocol,
                                 Some(format!("Unsupported URI '{}'", uri.as_str()))));
    }

    Ok(())
}

impl Source for HttpSrc {
    fn uri_validator(&self) -> Box<UriValidator> {
        Box::new(validate_uri)
    }

    fn is_seekable(&self) -> bool {
        match self.streaming_state {
            StreamingState::Started { seekable, .. } => seekable,
            _ => false,
        }
    }

    fn get_size(&self) -> u64 {
        match self.streaming_state {
            StreamingState::Started { size, .. } => size,
            _ => u64::MAX,
        }
    }

    fn start(&mut self, uri: &Url) -> Result<(), ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;
        self.streaming_state = try!(self.do_request(uri.clone(), 0, u64::MAX));

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn seek(&mut self, start: u64, stop: u64) -> Result<(), ErrorMessage> {
        let (position, old_stop, uri) = match self.streaming_state {
            StreamingState::Started { position, stop, ref uri, .. } => {
                (position, stop, uri.clone())
            }
            StreamingState::Stopped => {
                return Err(error_msg!(SourceError::Failure, ["Not started yet"]));
            }
        };

        if position == start && old_stop == stop {
            return Ok(());
        }

        self.streaming_state = StreamingState::Stopped;
        self.streaming_state = try!(self.do_request(uri, start, stop));

        Ok(())
    }

    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, FlowError> {
        let (response, position) = match self.streaming_state {
            StreamingState::Started { ref mut response, ref mut position, .. } => {
                (response, position)
            }
            StreamingState::Stopped => {
                return Err(FlowError::Error(error_msg!(SourceError::Failure, ["Not started yet"])));
            }
        };

        if *position != offset {
            return Err(FlowError::Error(error_msg!(SourceError::SeekFailed,
                                                   ["Got unexpected offset {}, expected {}",
                                                    offset,
                                                    position])));
        }

        let size = try!(response.read(data).or_else(|err| {
            Err(FlowError::Error(error_msg!(SourceError::ReadFailed,
                                            ["Failed to read at {}: {}", offset, err.to_string()])))
        }));

        if size == 0 {
            return Err(FlowError::Eos);
        }

        *position += size as u64;

        Ok(size)
    }
}