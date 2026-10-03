//! Bounded Content-Length framing shared by language servers and debug adapters.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Includes all headers, their separators, and any startup noise before them.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// A malformed peer must not allocate arbitrary amounts of editor memory.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Read one frame into reusable buffers, returning false only at a clean EOF.
///
/// Unknown headers and startup log lines are tolerated within the header budget.
/// Invalid or truncated frames terminate the stream; their bodies are not decoded.
pub async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin + ?Sized),
    header: &mut String,
    content: &mut Vec<u8>,
) -> io::Result<bool> {
    read_frame_with_limits(reader, header, content, MAX_HEADER_BYTES, MAX_MESSAGE_BYTES).await
}

async fn read_frame_with_limits(
    reader: &mut (impl AsyncBufRead + Unpin + ?Sized),
    header: &mut String,
    content: &mut Vec<u8>,
    max_headers: usize,
    max_message: usize,
) -> io::Result<bool> {
    content.clear();
    let mut header_bytes = 0;
    let mut content_length = None;
    loop {
        header.clear();
        let remaining = max_headers - header_bytes;
        // Take bounds read_line itself, including unterminated lines.
        let read = (&mut *reader)
            .take(remaining.saturating_add(1) as u64)
            .read_line(header)
            .await?;
        if read == 0 {
            return if header_bytes == 0 {
                Ok(false)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete protocol headers",
                ))
            };
        }
        if read > remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol headers exceed byte limit",
            ));
        }
        header_bytes += read;
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.trim().split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                let length = value.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid content length")
                })?;
                if length > max_message {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "protocol body exceeds byte limit",
                    ));
                }
                if content_length.replace(length).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "duplicate content length",
                    ));
                }
            }
        }
    }

    let length = content_length
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing content length"))?;
    content
        .try_reserve(length)
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    content.resize(length, 0);
    if let Err(error) = reader.read_exact(content).await {
        content.clear();
        return Err(error);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn fragmented_frames_preserve_boundaries_and_tolerate_startup_noise() {
        let input = b"starting server\ncontent-length: 2\r\nX-Trace: ignored\r\n\r\n{}Content-Length: 4\r\n\r\nnull";
        let mut reader = BufReader::with_capacity(3, input.as_slice());
        let mut header = String::new();
        let mut content = Vec::new();
        assert!(read_frame(&mut reader, &mut header, &mut content)
            .await
            .unwrap());
        assert_eq!(content, b"{}");
        assert!(read_frame(&mut reader, &mut header, &mut content)
            .await
            .unwrap());
        assert_eq!(content, b"null");
        assert!(!read_frame(&mut reader, &mut header, &mut content)
            .await
            .unwrap());
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn invalid_lengths_are_rejected_before_body_allocation() {
        for length in [
            (MAX_MESSAGE_BYTES + 1).to_string(),
            usize::MAX.to_string(),
            "999999999999999999999999999999999999".into(),
            "-1".into(),
        ] {
            let input = format!("Content-Length: {length}\r\n\r\n");
            let mut content = Vec::new();
            let error = read_frame(&mut input.as_bytes(), &mut String::new(), &mut content)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{length}");
            assert_eq!(content.capacity(), 0);
        }
    }

    #[tokio::test]
    async fn header_limit_covers_unterminated_lines_and_the_entire_header_block() {
        for input in [
            "x".repeat(MAX_HEADER_BYTES + 1),
            "X-Extra: value\r\n".repeat(MAX_HEADER_BYTES),
        ] {
            let mut header = String::new();
            let error = read_frame(&mut input.as_bytes(), &mut header, &mut Vec::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(header.len() <= MAX_HEADER_BYTES + 1);
        }
    }

    #[tokio::test]
    async fn missing_and_duplicate_lengths_are_invalid() {
        for input in [
            "Content-Type: application/json\r\n\r\n",
            "Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}",
        ] {
            let error = read_frame(&mut input.as_bytes(), &mut String::new(), &mut Vec::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[tokio::test]
    async fn partial_eof_never_leaves_a_decodable_body() {
        for input in ["Content-Length: 2\r\n", "Content-Length: 4\r\n\r\n{}"] {
            let mut content = b"previous body".to_vec();
            let error = read_frame(&mut input.as_bytes(), &mut String::new(), &mut content)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
            assert!(content.is_empty());
        }
    }

    #[tokio::test]
    async fn exact_limits_accept_a_frame_without_consuming_the_next_one() {
        let header = "Content-Length: 5\r\n\r\n";
        let input = format!("{header}hello{header}world");
        let mut reader = input.as_bytes();
        let mut content = Vec::new();
        let mut buffer = String::new();
        for expected in [b"hello", b"world"] {
            assert!(read_frame_with_limits(
                &mut reader,
                &mut buffer,
                &mut content,
                header.len(),
                5
            )
            .await
            .unwrap());
            assert_eq!(&content, expected);
        }
        assert!(reader.is_empty());
    }
}
