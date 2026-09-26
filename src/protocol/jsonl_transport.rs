use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

pub(super) async fn write_json_line<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
) -> Result<(), String> {
    let encoded = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if encoded.len().saturating_add(1) > MAX_MESSAGE_BYTES {
        return Err(format!(
            "protocol message exceeds the {MAX_MESSAGE_BYTES} byte limit"
        ));
    }
    writer
        .write_all(&encoded)
        .await
        .map_err(|error| error.to_string())?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|error| error.to_string())?;
    writer.flush().await.map_err(|error| error.to_string())
}

pub(super) enum BoundedLine {
    Line(Vec<u8>),
    TooLarge,
}

pub(super) async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<BoundedLine>, String> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf().await.map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            if line.is_empty() && !oversized {
                return Ok(None);
            }
            return if oversized {
                Ok(Some(BoundedLine::TooLarge))
            } else {
                Ok(Some(BoundedLine::Line(line)))
            };
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |index| index + 1);
        if line.len() + take > MAX_MESSAGE_BYTES {
            oversized = true;
        } else if !oversized {
            line.extend_from_slice(&chunk[..take]);
        }
        reader.consume(take);
        if newline.is_some() {
            if oversized {
                return Ok(Some(BoundedLine::TooLarge));
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(BoundedLine::Line(line)));
        }
    }
}
