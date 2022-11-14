use std::io::{Read, Seek, SeekFrom};
use std::iter::Iterator;

use crate::Source;

use audiopus::{coder::Decoder, Channels};
use bitreader::BitReader;
use magnum::error::OpusSourceError;
use magnum::metadata::OpusMeta;
use ogg::PacketReader;

pub struct OpusDecoder<R>
where
    R: Read + Seek,
{
    pub metadata: OpusMeta,
    packet: ogg::PacketReader<R>,
    decoder: Decoder,
    buffer: Vec<i16>,
    buffer_pos: usize,
}

impl<R> OpusDecoder<R>
where
    R: Read + Seek,
{
    pub fn new(mut data: R) -> Result<Self, R> {
        let stream_pos = data.seek(SeekFrom::Current(0)).unwrap();
        let mut packet = ogg::PacketReader::new(data);
        let metadata = read_metadata(&mut packet);
        if metadata.is_err() {
            let mut data = packet.into_inner();
            data.seek(SeekFrom::Start(stream_pos)).unwrap();
            return Err(data);
        }

        Ok(Self::from_meta_and_reader(metadata.unwrap(), packet))
    }

    pub fn from_packet_reader(mut packet: ogg::PacketReader<R>) -> Result<Self, OpusSourceError> {
        let metadata = read_metadata(&mut packet)?;

        Ok(Self::from_meta_and_reader(metadata, packet))
    }

    fn from_meta_and_reader(metadata: OpusMeta, packet: ogg::PacketReader<R>) -> Self {
        let decoder = Decoder::new(
            audiopus::SampleRate::Hz48000,
            if metadata.channel_count == 1 {
                Channels::Mono
            } else {
                Channels::Stereo
            },
        )
        .unwrap();

        Self {
            metadata,
            packet,
            decoder,
            buffer: vec![],
            buffer_pos: 0,
        }
    }

    /* FRAME SIZE Reference
    +-----------------------+-----------+-----------+-------------------+
    | Configuration         | Mode      | Bandwidth | Frame Sizes       |
    | Number(s)             |           |           |                   |
    +-----------------------+-----------+-----------+-------------------+
    | 0...3                 | SILK-only | NB        | 10, 20, 40, 60 ms |
    |                       |           |           |                   |
    | 4...7                 | SILK-only | MB        | 10, 20, 40, 60 ms |
    |                       |           |           |                   |
    | 8...11                | SILK-only | WB        | 10, 20, 40, 60 ms |
    |                       |           |           |                   |
    | 12...13               | Hybrid    | SWB       | 10, 20 ms         |
    |                       |           |           |                   |
    | 14...15               | Hybrid    | FB        | 10, 20 ms         |
    |                       |           |           |                   |
    | 16...19               | CELT-only | NB        | 2.5, 5, 10, 20 ms |
    |                       |           |           |                   |
    | 20...23               | CELT-only | WB        | 2.5, 5, 10, 20 ms |
    |                       |           |           |                   |
    | 24...27               | CELT-only | SWB       | 2.5, 5, 10, 20 ms |
    |                       |           |           |                   |
    | 28...31               | CELT-only | FB        | 2.5, 5, 10, 20 ms |
    +-----------------------+-----------+-----------+-------------------+
     */

    fn load_next_chunk(&mut self) -> Result<(), OpusSourceError> {
        let packet = self
            .packet
            .read_packet_expected()
            .map_err(|_| OpusSourceError::InvalidAudioStream)?;
        let mut toc = BitReader::new(&packet.data[0..1]);
        let c = toc.read_u8(5).unwrap();
        let s = toc.read_u8(1).unwrap();
        // Frames per packet, minimum of 1
        let f = toc.read_u8(2).unwrap().max(1);

        // In milliseconds
        let frame_size = {
            match c {
                0 | 4 | 8 | 12 | 14 | 18 | 22 | 26 | 30 => 10.0,
                1 | 5 | 9 | 13 | 15 | 19 | 23 | 27 | 31 => 20.0,
                2 | 6 | 10 => 40.0,
                3 | 7 | 11 => 60.0,
                16 | 20 | 24 | 28 => 2.5,
                17 | 21 | 25 | 29 => 5.0,
                _ => panic!("Unsupported frame size"),
            }
        };

        self.buffer = vec![
            0;
            (self.metadata.sample_rate / (1000.0 / frame_size) as u32
                * f as u32
                * if s == 0 { 1 } else { 2 }) as usize
        ];

        self.decoder
            .decode(Some(&packet.data), &mut self.buffer, false)
            .map_err(|_| OpusSourceError::InvalidAudioStream)?;

        Ok(())
    }

    pub fn into_inner(self) -> PacketReader<R> {
        self.packet
    }
}

impl<R> Source for OpusDecoder<R>
where
    R: Read + Seek,
{
    fn current_frame_len(&self) -> Option<usize> {
        Some(240)
    }

    fn channels(&self) -> u16 {
        self.metadata.channel_count as u16
    }

    fn sample_rate(&self) -> u32 {
        48_000 as u32
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

impl<R> Iterator for OpusDecoder<R>
where
    R: Read + Seek,
{
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        // If we're out of data (or haven't started) then load a chunk of data into our buffer
        if self.buffer.is_empty() {
            if self.load_next_chunk().is_err() {
                self.buffer.clear();
            } else {
                // Reset the read counter
                self.buffer_pos = 0;
            }
        }
        // Assuming there's data now we can read it using our counter
        self.buffer_pos += 1;
        if self.buffer_pos > self.buffer.len() {
            //println!("End of data chunk");
            self.buffer.clear();
            return self.next();
        }

        if self.buffer.is_empty() {
            None
        } else {
            Some(self.buffer[self.buffer_pos - 1])
        }
    }
}

fn read_metadata<R>(packet: &mut PacketReader<R>) -> Result<OpusMeta, OpusSourceError>
where
    R: Read + Seek,
{
    let id_header = packet
        .read_packet_expected()
        .map_err(|_| OpusSourceError::InvalidHeaderData)?
        .data;
    let comment_header = packet
        .read_packet_expected()
        .map_err(|_| OpusSourceError::InvalidHeaderData)?
        .data;

    OpusMeta::with_headers(id_header, comment_header)
}
