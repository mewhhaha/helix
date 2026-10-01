//! Small, direct-transfer Kitty graphics encoder. No files or shared memory are needed.

use std::io::{self, Write};

use super::CursorImage;

const RAW_CHUNK_SIZE: usize = 3072;
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(super) fn delete_image(writer: &mut impl Write, image_id: u32) -> io::Result<()> {
    write!(writer, "\x1b_Ga=d,d=I,i={image_id},q=2;\x1b\\")
}

#[derive(Default)]
pub(super) struct Encoder {
    compressor: Option<zlib_rs::Deflate>,
    compressed: Vec<u8>,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("capacity", &self.compressed.capacity())
            .finish_non_exhaustive()
    }
}

impl Encoder {
    pub(super) fn draw_image(
        &mut self,
        writer: &mut impl Write,
        image_id: u32,
        image: &CursorImage<'_>,
    ) -> io::Result<()> {
        image.validate()?;

        // Positioning the placement must not move the real cursor (including on timer-only frames).
        let capacity = zlib_rs::compress_bound(image.rgba.len());
        if self.compressed.len() < capacity {
            self.compressed.resize(capacity, 0);
        }
        let compressor = self
            .compressor
            .get_or_insert_with(|| zlib_rs::Deflate::new(1, true, 15));
        compressor.reset();
        let status = compressor
            .compress(
                image.rgba,
                &mut self.compressed,
                zlib_rs::DeflateFlush::Finish,
            )
            .map_err(|error| io::Error::other(format!("cursor compression failed: {error:?}")))?;
        if status != zlib_rs::Status::StreamEnd
            || compressor.total_in() as usize != image.rgba.len()
        {
            return Err(io::Error::other("incomplete cursor image compression"));
        }
        let compressed = &self.compressed[..compressor.total_out() as usize];
        writer.write_all(b"\x1b7")?;
        let result = (|| {
            write!(
                writer,
                "\x1b[{};{}H",
                image.position.row + 1,
                image.position.col + 1
            )?;
            let mut encoded = [0; 4096];
            let mut chunks = compressed.chunks(RAW_CHUNK_SIZE).peekable();
            let mut first = true;
            while let Some(chunk) = chunks.next() {
                let more = u8::from(chunks.peek().is_some());
                if first {
                    write!(
                    writer,
                    "\x1b_Ga=T,t=d,f=32,o=z,i={image_id},p=1,s={},v={},X={},Y={},z=-1,C=1,q=2,m={more};",
                    image.width, image.height, image.offset_x, image.offset_y,
                )?;
                    first = false;
                } else {
                    write!(writer, "\x1b_Gm={more};")?;
                }
                let len = encode_base64(chunk, &mut encoded);
                writer.write_all(&encoded[..len])?;
                writer.write_all(b"\x1b\\")?;
            }
            Ok(())
        })();
        let restore = writer.write_all(b"\x1b8");
        result.and(restore)
    }
}

fn encode_base64(input: &[u8], output: &mut [u8; 4096]) -> usize {
    let mut len = 0;
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        output[len] = BASE64[usize::from(a >> 2)];
        output[len + 1] = BASE64[usize::from(((a & 3) << 4) | (b >> 4))];
        output[len + 2] = if chunk.len() > 1 {
            BASE64[usize::from(((b & 15) << 2) | (c >> 6))]
        } else {
            b'='
        };
        output[len + 3] = if chunk.len() > 2 {
            BASE64[usize::from(c & 63)]
        } else {
            b'='
        };
        len += 4;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::Position;

    #[test]
    fn rgba_upload_chunks_preserves_alpha_position_and_cursor() {
        let mut state = 42_u32;
        let rgba: Vec<_> = (0..8192)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let image = CursorImage {
            position: Position::new(2, 5),
            offset_x: 3,
            offset_y: 4,
            width: 64,
            height: 32,
            rgba: &rgba,
        };
        let mut output = Vec::new();
        Encoder::default()
            .draw_image(&mut output, 123, &image)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with(
            "\x1b7\x1b[3;6H\x1b_Ga=T,t=d,f=32,o=z,i=123,p=1,s=64,v=32,X=3,Y=4,z=-1,C=1,q=2,m=1;"
        ));
        assert!(output.ends_with("\x1b\\\x1b8"));
        let uploads: Vec<_> = output.split("\x1b_G").skip(1).collect();
        assert!(uploads.len() > 1);
        let first_payload = uploads[0]
            .split_once(';')
            .unwrap()
            .1
            .split("\x1b\\")
            .next()
            .unwrap();
        assert_eq!(first_payload.len(), 4096);
        assert!(uploads.last().unwrap().starts_with("m=0;"));
        let mut compressed = Vec::new();
        for (index, upload) in uploads.iter().enumerate() {
            let payload = upload
                .split_once(';')
                .unwrap()
                .1
                .split("\x1b\\")
                .next()
                .unwrap();
            assert!(payload.len() <= 4096);
            assert_eq!(payload.len() % 4, 0);
            if index + 1 != uploads.len() {
                assert!(upload.split_once(';').unwrap().0.ends_with("m=1"));
                assert_eq!(payload.len(), 4096);
            }
            for chunk in payload.as_bytes().chunks_exact(4) {
                let mut value = 0_u32;
                for &byte in chunk {
                    let digit = BASE64
                        .iter()
                        .position(|&candidate| candidate == byte)
                        .unwrap_or(0);
                    value = (value << 6) | digit as u32;
                }
                compressed.push((value >> 16) as u8);
                if chunk[2] != b'=' {
                    compressed.push((value >> 8) as u8);
                }
                if chunk[3] != b'=' {
                    compressed.push(value as u8);
                }
            }
        }
        assert_eq!(
            miniz_oxide::inflate::decompress_to_vec_zlib(&compressed).unwrap(),
            rgba
        );
    }

    #[test]
    fn deleting_cursor_frees_only_its_image() {
        let mut output = Vec::new();
        delete_image(&mut output, 123).unwrap();
        assert_eq!(output, b"\x1b_Ga=d,d=I,i=123,q=2;\x1b\\");
    }

    #[test]
    fn malformed_image_writes_nothing() {
        let image = CursorImage {
            position: Position::new(0, 0),
            offset_x: 0,
            offset_y: 0,
            width: u32::MAX,
            height: u32::MAX,
            rgba: &[0; 4],
        };
        let mut output = Vec::new();
        assert_eq!(
            Encoder::default()
                .draw_image(&mut output, 123, &image)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(output.is_empty());
    }

    #[test]
    fn reused_compressor_produces_independent_zlib_streams() {
        let mut encoder = Encoder::default();
        for width in [512, 3, 1024, 1, 512] {
            let rgba: Vec<_> = (0..width * 4).map(|index| (index % 251) as u8).collect();
            let image = CursorImage {
                position: Position::new(0, 0),
                offset_x: 0,
                offset_y: 0,
                width,
                height: 1,
                rgba: &rgba,
            };
            encoder.draw_image(&mut Vec::new(), 123, &image).unwrap();
            let len = encoder.compressor.as_ref().unwrap().total_out() as usize;
            assert_eq!(
                miniz_oxide::inflate::decompress_to_vec_zlib(&encoder.compressed[..len]).unwrap(),
                rgba
            );
        }
    }
}
