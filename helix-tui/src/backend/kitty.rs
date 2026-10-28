//! Small, direct-transfer Kitty graphics encoder. No files or shared memory are needed.

use std::io::{self, Write};

use super::{CursorAnimation, CursorImage};

const RAW_CHUNK_SIZE: usize = 3072;
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Compression {
    None,
    #[default]
    Zlib,
}

pub(super) fn delete_image(writer: &mut impl Write, image_id: u32) -> io::Result<()> {
    write!(writer, "\x1b_Ga=d,d=I,i={image_id},q=2;\x1b\\")
}

#[derive(Default)]
pub(super) struct Encoder {
    compression: Compression,
    compressor: Option<zlib_rs::Deflate>,
    compressed: Vec<u8>,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("compression", &self.compression)
            .field("capacity", &self.compressed.capacity())
            .finish_non_exhaustive()
    }
}

impl Encoder {
    pub(super) fn new(compression: Compression) -> Self {
        Self {
            compression,
            ..Self::default()
        }
    }

    pub(super) fn draw_image(
        &mut self,
        writer: &mut impl Write,
        image_id: u32,
        image: &CursorImage<'_>,
    ) -> io::Result<()> {
        image.validate()?;

        // Positioning the placement must not move the real cursor.
        writer.write_all(b"\x1b7")?;
        let result = (|| {
            write!(
                writer,
                "\x1b[{};{}H",
                image.position.row + 1,
                image.position.col + 1
            )?;
            self.write_rgba(writer, image.rgba, false, |writer, compression, more| {
                write!(writer,
                    "\x1b_Ga=T,t=d,f=32{compression},i={image_id},p=1,s={},v={},X={},Y={},z=-1,C=1,q=2,m={more};",
                    image.width, image.height, image.offset_x, image.offset_y)
            })
        })();
        let restore = writer.write_all(b"\x1b8");
        result.and(restore)
    }

    pub(super) fn upload_animation(
        &mut self,
        writer: &mut impl Write,
        image_id: u32,
        animation: &CursorAnimation,
    ) -> io::Result<()> {
        animation.validate()?;
        // Only the root needs padding to the full canvas. Other frames use a
        // fresh transparent canvas with a small replacement rectangle.
        let first = &animation.frames[0];
        let mut root = vec![0; animation.width as usize * animation.height as usize * 4];
        for (y, row) in first
            .rgba
            .chunks_exact(first.width as usize * 4)
            .enumerate()
        {
            let offset = ((first.y as usize + y) * animation.width as usize + first.x as usize) * 4;
            root[offset..offset + row.len()].copy_from_slice(row);
        }
        self.write_rgba(writer, &root, false, |writer, compression, more| {
            write!(
                writer,
                "\x1b_Ga=t,t=d,f=32{compression},i={image_id},s={},v={},N=1,q=2,m={more};",
                animation.width, animation.height
            )
        })?;
        // Root frames are gapless until explicitly assigned a duration.
        write!(
            writer,
            "\x1b_Ga=a,i={image_id},r=1,z={},q=2;\x1b\\",
            first.gap_ms
        )?;
        for frame in animation.frames.iter().skip(1) {
            self.write_rgba(writer, &frame.rgba, true, |writer, compression, more| {
                write!(writer, "\x1b_Ga=f,t=d,f=32{compression},i={image_id},s={},v={},x={},y={},X=1,Y=0,z={},N=1,q=2,m={more};",
                    frame.width, frame.height, frame.x, frame.y, frame.gap_ms)
            })?;
        }
        Ok(())
    }

    pub(super) fn show_animation(
        writer: &mut impl Write,
        image_id: u32,
        animation: &CursorAnimation,
    ) -> io::Result<()> {
        writer.write_all(b"\x1b7")?;
        let result = (|| {
            write!(
                writer,
                "\x1b[{};{}H\x1b_Ga=p,i={image_id},p=1,X={},Y={},z=-1,C=1,q=2;\x1b\\",
                animation.position.row + 1,
                animation.position.col + 1,
                animation.offset_x,
                animation.offset_y
            )?;
            // Loading mode parks on the final opaque cursor; it never loops.
            write!(writer, "\x1b_Ga=a,i={image_id},c=1,s=2,q=2;\x1b\\")
        })();
        let restore = writer.write_all(b"\x1b8");
        result.and(restore)
    }

    fn write_rgba<W: Write>(
        &mut self,
        writer: &mut W,
        rgba: &[u8],
        animation_frame: bool,
        header: impl FnOnce(&mut W, &str, u8) -> io::Result<()>,
    ) -> io::Result<()> {
        let (data, compression) = match self.compression {
            Compression::None => (rgba, ""),
            Compression::Zlib => {
                let capacity = zlib_rs::compress_bound(rgba.len());
                if self.compressed.len() < capacity {
                    self.compressed.resize(capacity, 0);
                }
                let compressor = self
                    .compressor
                    .get_or_insert_with(|| zlib_rs::Deflate::new(1, true, 15));
                compressor.reset();
                let status = compressor
                    .compress(rgba, &mut self.compressed, zlib_rs::DeflateFlush::Finish)
                    .map_err(|error| {
                        io::Error::other(format!("cursor compression failed: {error:?}"))
                    })?;
                if status != zlib_rs::Status::StreamEnd
                    || compressor.total_in() as usize != rgba.len()
                {
                    return Err(io::Error::other("incomplete cursor image compression"));
                }
                (&self.compressed[..compressor.total_out() as usize], ",o=z")
            }
        };
        let mut encoded = [0; 4096];
        let mut chunks = data.chunks(RAW_CHUNK_SIZE).peekable();
        let mut header = Some(header);
        while let Some(chunk) = chunks.next() {
            let more = u8::from(chunks.peek().is_some());
            if let Some(header) = header.take() {
                header(writer, compression, more)?;
            } else if animation_frame {
                write!(writer, "\x1b_Ga=f,m={more};")?;
            } else {
                write!(writer, "\x1b_Gm={more};")?;
            }
            let len = encode_base64(chunk, &mut encoded);
            writer.write_all(&encoded[..len])?;
            writer.write_all(b"\x1b\\")?;
        }
        Ok(())
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
    use crate::backend::CursorAnimationFrame;
    use helix_core::Position;

    #[test]
    fn animation_uploads_independent_frames_offscreen_and_parks_on_the_last() {
        let mut state = 42u32;
        let pixels: Vec<u8> = (0..8192)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let animation = CursorAnimation {
            position: Position::new(2, 3),
            offset_x: 1,
            offset_y: 2,
            width: 64,
            height: 32,
            frames: vec![
                CursorAnimationFrame {
                    x: 1,
                    y: 1,
                    width: 1,
                    height: 1,
                    gap_ms: 17,
                    rgba: vec![1, 2, 3, 255],
                },
                CursorAnimationFrame {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 32,
                    gap_ms: 1,
                    rgba: pixels.clone(),
                },
            ],
        };
        for compression in [Compression::None, Compression::Zlib] {
            let mut encoder = Encoder::new(compression);
            let mut output = Vec::new();
            encoder
                .upload_animation(&mut output, 123, &animation)
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(!output.contains("a=p") && !output.contains("a=T"));
            let (root, frames) = output
                .split_once("\x1b_Ga=a,i=123,r=1,z=17,q=2;\x1b\\")
                .unwrap();
            let decode = |encoded: &str| {
                let bytes = transmitted_bytes(encoded);
                if compression == Compression::Zlib {
                    miniz_oxide::inflate::decompress_to_vec_zlib(&bytes).unwrap()
                } else {
                    bytes
                }
            };
            let root = decode(root);
            let pixel = (64 + 1) * 4;
            assert_eq!(&root[pixel..pixel + 4], &[1, 2, 3, 255]);
            assert!(root[..pixel].iter().all(|&byte| byte == 0));
            assert!(root[pixel + 4..].iter().all(|&byte| byte == 0));
            assert_eq!(decode(frames), pixels);
            assert!(frames.contains("x=0,y=0,X=1,Y=0,z=1"));
            assert!(frames.contains("\x1b_Ga=f,m=0;"));
            assert!(!frames.contains(",c="));
            let mut placement = Vec::new();
            Encoder::show_animation(&mut placement, 123, &animation).unwrap();
            let placement = String::from_utf8(placement).unwrap();
            assert!(placement.starts_with("\x1b7\x1b[3;4H"));
            assert!(placement.ends_with("\x1b_Ga=a,i=123,c=1,s=2,q=2;\x1b\\\x1b8"));
        }
        let mut invalid = animation;
        invalid.frames[1].x = 1;
        let mut output = Vec::new();
        assert!(Encoder::default()
            .upload_animation(&mut output, 1, &invalid)
            .is_err());
        assert!(output.is_empty());
    }

    fn transmitted_bytes(output: &str) -> Vec<u8> {
        let uploads: Vec<_> = output.split("\x1b_G").skip(1).collect();
        assert!(
            uploads.last().unwrap().starts_with("m=0;")
                || uploads.last().unwrap().starts_with("a=f,m=0;")
                || uploads.len() == 1
        );
        let mut bytes = Vec::new();
        for (index, upload) in uploads.iter().enumerate() {
            let (control, payload) = upload.split_once(';').unwrap();
            let payload = payload.split("\x1b\\").next().unwrap();
            assert!(payload.len() <= 4096);
            assert_eq!(payload.len() % 4, 0);
            let more = u8::from(index + 1 != uploads.len());
            assert!(control.ends_with(&format!("m={more}")));
            if more != 0 {
                assert_eq!(payload.len(), 4096);
            }
            for chunk in payload.as_bytes().chunks_exact(4) {
                let mut value = 0_u32;
                for &byte in chunk {
                    let digit = if byte == b'=' {
                        0
                    } else {
                        BASE64
                            .iter()
                            .position(|&candidate| candidate == byte)
                            .unwrap()
                    };
                    value = (value << 6) | digit as u32;
                }
                bytes.push((value >> 16) as u8);
                if chunk[2] != b'=' {
                    bytes.push((value >> 8) as u8);
                }
                if chunk[3] != b'=' {
                    bytes.push(value as u8);
                }
            }
        }
        bytes
    }

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
        let compressed = transmitted_bytes(&output);
        assert_eq!(
            miniz_oxide::inflate::decompress_to_vec_zlib(&compressed).unwrap(),
            rgba
        );
    }

    #[test]
    fn uncompressed_frames_preserve_rgba_and_chunking_without_allocating_a_compressor() {
        let mut encoder = Encoder::new(Compression::None);
        // Include a frame larger than the Ghostty inflater's 32 KiB window,
        // the maximum animation frame size, and smaller frames after both.
        for (width, height) in [(10, 20), (218, 99), (512, 512), (1, 1), (10, 20)] {
            let rgba: Vec<_> = (0..width * height * 4)
                .map(|index| (index % 251) as u8)
                .collect();
            let image = CursorImage {
                position: Position::new(2, 5),
                offset_x: 3,
                offset_y: 4,
                width,
                height,
                rgba: &rgba,
            };
            let mut output = Vec::new();
            encoder.draw_image(&mut output, 123, &image).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.starts_with(&format!(
                "\x1b7\x1b[3;6H\x1b_Ga=T,t=d,f=32,i=123,p=1,s={width},v={height},X=3,Y=4,z=-1,C=1,q=2,m="
            )));
            assert!(output.ends_with("\x1b\\\x1b8"));
            assert!(!output.contains("o=z"));
            assert_eq!(transmitted_bytes(&output), rgba);
            assert!(encoder.compressor.is_none());
            assert!(encoder.compressed.is_empty());
        }
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
