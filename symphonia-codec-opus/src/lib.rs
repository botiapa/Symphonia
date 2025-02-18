// Symphonia
// Copyright (c) 2019-2021 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]

use std::sync::{Arc, Mutex};

use symphonia_core::audio::{
    AsAudioBufferRef, AudioBuffer, AudioBufferRef, Channels, Layout, Signal, SignalSpec,
};
use symphonia_core::codecs::{
    CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult, CODEC_TYPE_OPUS,
};
use symphonia_core::errors::{decode_error, unsupported_error, Error, Result};
use symphonia_core::formats::Packet;
use symphonia_core::io::{BufReader, ReadBytes};
use symphonia_core::sample::SampleFormat;
use symphonia_core::support_codec;

#[allow(dead_code)]
pub struct OpusDecoder {
    decoder: Mutex<opus::Decoder>,
    params: CodecParameters,
    buffer: AudioBuffer<f32>,
    inner_buffer: Vec<f32>,
}

/// The operating mode for the Opus Decoder.
/// See RFC 6716 Section 3.1, https://tools.ietf.org/pdf/rfc7845.pdf.
enum Mode {
    /// SILK-only mode.
    Silk,
    /// CELT-only mode.
    Celt,
    /// SILK and CELT mode.
    Hybrid,
}

impl Decoder for OpusDecoder {
    fn try_new(params: &CodecParameters, _: &DecoderOptions) -> Result<Self> {
        let extra_data = match params.extra_data.as_ref() {
            Some(buf) => buf,
            _ => return unsupported_error("opus: missing extra data"),
        };

        let channels = match params.channels {
            Some(ch) if ch.count() == 1 => opus::Channels::Mono,
            Some(ch) if ch.count() == 2 => opus::Channels::Stereo,
            _ => return unsupported_error("opus: unsupported channel_layout"),
        };

        let sample_rate = match params.sample_rate {
            Some(48000) => 48000,
            _ => return unsupported_error("opus: unsupported sample rate"),
        };

        let decoder = opus::Decoder::new(sample_rate, channels)
            .map_err(|_| Error::Unsupported("opus: failed to create decoder"))?;
        let channels = params.channels.ok_or(Error::Unsupported("opus: missing channel layout"))?;
        Ok(OpusDecoder {
            decoder: Mutex::new(decoder),
            params: params.clone(),
            buffer: AudioBuffer::new(2880, SignalSpec::new(48000, channels)),
            inner_buffer: vec![0f32; 2880 * 2],
        })
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[support_codec!(CODEC_TYPE_OPUS, "opus", "Opus")]
    }

    fn reset(&mut self) {
        self.decoder.get_mut().expect("Failed locking decoder").reset_state().unwrap();
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    #[allow(unused_variables)]
    fn decode(&mut self, packet: &Packet) -> Result<AudioBufferRef<'_>> {
        let d = self
            .decoder
            .get_mut()
            .map_err(|_| Error::DecodeError("opus: failed to lock decoder"))?;
        let read = d
            .decode_float(&packet.data, &mut self.inner_buffer, false)
            .map_err(|_| Error::DecodeError("opus: failed to decode packet"))?
            as usize;

        self.buffer.clear();
        self.buffer.render_reserved(Some(read));
        let (l, r) = self.buffer.chan_pair_mut(0, 1);

        for i in 0..read {
            l[i] = self.inner_buffer[i * 2];
            r[i] = self.inner_buffer[i * 2 + 1];
        }
        self.buffer.truncate(read);
        Ok(self.buffer.as_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        self.buffer.as_audio_buffer_ref()
    }
}
