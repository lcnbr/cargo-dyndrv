//! Bounded, integrity-checked metadata delivery over stock Nix build logs.
use super::{CompilerOutput, Prepared};
use color_eyre::eyre::{self, bail};
use data_encoding::BASE64;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};

pub(super) const PREFIX: &str = "DYNDRV_STOCK_METADATA ";
const CHUNK: usize = 16 * 1024;
const LIMIT: usize = 512 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Frame {
    Start {
        path: PathBuf,
        size: usize,
        digest: String,
        notification: Vec<u8>,
    },
    Chunk {
        index: usize,
        data: String,
    },
    End,
}

pub(super) struct Artifact {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub notification: Vec<u8>,
}

impl Artifact {
    fn path(line: &[u8], output: &CompilerOutput) -> eyre::Result<Option<PathBuf>> {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return Ok(None);
        };
        if value["$message_type"] != "artifact" || value["emit"] != "metadata" {
            return Ok(None);
        }
        let Some(path) = value["artifact"].as_str() else {
            bail!("metadata notification lacks artifact path")
        };
        let path = PathBuf::from(path);
        Prepared::scratch(&path)?;
        if !matches!(output, CompilerOutput::Directory(directory) if path.parent() == Some(directory.as_path()))
            || path
                .extension()
                .is_none_or(|extension| extension != "rmeta")
        {
            bail!("metadata notification escapes declared output");
        }
        Ok(Some(path))
    }

    pub fn emit(line: &[u8], output: &CompilerOutput, log: &mut impl Write) -> eyre::Result<()> {
        let Some(path) = Self::path(line, output)? else {
            return Ok(());
        };
        if !path.symlink_metadata()?.is_file() {
            bail!("metadata must be a regular file")
        }
        let size = usize::try_from(path.metadata()?.len())?;
        if size > LIMIT {
            bail!("metadata exceeds transport limit")
        }
        let bytes = fs::read(&path)?;
        if bytes.len() != size {
            bail!("metadata changed while reading")
        }
        Self::frame(
            &Frame::Start {
                path,
                size,
                digest: Prepared::digest(&bytes),
                notification: line.to_vec(),
            },
            log,
        )?;
        for (index, bytes) in bytes.chunks(CHUNK).enumerate() {
            Self::frame(
                &Frame::Chunk {
                    index,
                    data: BASE64.encode(bytes),
                },
                log,
            )?;
        }
        Self::frame(&Frame::End, log)?;
        log.flush()?;
        Ok(())
    }

    fn frame(frame: &Frame, log: &mut impl Write) -> eyre::Result<()> {
        write!(log, "{PREFIX}")?;
        serde_json::to_writer(&mut *log, frame)?;
        log.write_all(b"\n")?;
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Receiver {
    pending: Option<(Artifact, usize, String)>,
    chunks: usize,
    complete: bool,
}

impl Receiver {
    pub fn accept(
        &mut self,
        line: &str,
        output: &CompilerOutput,
    ) -> eyre::Result<Option<Artifact>> {
        if self.complete {
            bail!("metadata frame after completion")
        }
        match serde_json::from_str::<Frame>(line)? {
            Frame::Start {
                path,
                size,
                digest,
                notification,
            } => {
                if self.pending.is_some()
                    || size > LIMIT
                    || Artifact::path(&notification, output)? != Some(path.clone())
                {
                    bail!("invalid metadata transfer header");
                }
                self.pending = Some((
                    Artifact {
                        path,
                        bytes: Vec::new(),
                        notification,
                    },
                    size,
                    digest,
                ));
            }
            Frame::Chunk { index, data } => {
                let Some((artifact, size, _)) = self.pending.as_mut() else {
                    bail!("metadata chunk without header")
                };
                if index != self.chunks || data.len() > CHUNK.div_ceil(3) * 4 {
                    bail!("invalid metadata chunk")
                }
                let bytes = BASE64.decode(data.as_bytes())?;
                if bytes.is_empty()
                    || bytes.len() > CHUNK
                    || artifact.bytes.len() + bytes.len() > *size
                {
                    bail!("metadata chunk exceeds declared size");
                }
                artifact.bytes.extend(bytes);
                self.chunks += 1;
            }
            Frame::End => {
                let Some((artifact, size, digest)) = self.pending.take() else {
                    bail!("metadata completion without header")
                };
                if artifact.bytes.len() != size || Prepared::digest(&artifact.bytes) != digest {
                    bail!("incomplete or corrupt metadata payload");
                }
                self.complete = true;
                return Ok(Some(artifact));
            }
        }
        Ok(None)
    }

    pub fn finish(&self) -> eyre::Result<()> {
        if self.pending.is_some() {
            bail!("incomplete metadata transport")
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_preserves_binary_bytes_and_rejects_corrupt_or_incomplete_frames() {
        let root =
            std::env::temp_dir().join(format!("dyndrv-metadata-transport-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let path = root.join("libfixture.rmeta");
        let bytes: Vec<_> = (0..=255).cycle().take(4 * 1024 * 1024 + 7).collect();
        fs::write(&path, &bytes).unwrap();
        let output = CompilerOutput::Directory(root.clone());
        let notification = serde_json::to_vec(
            &serde_json::json!({"$message_type":"artifact", "emit":"metadata", "artifact":path}),
        )
        .unwrap();
        let mut encoded = Vec::new();
        Artifact::emit(&notification, &output, &mut encoded).unwrap();
        let text = String::from_utf8(encoded).unwrap();
        let frames: Vec<_> = text
            .lines()
            .map(|line| line.strip_prefix(PREFIX).unwrap())
            .collect();
        let mut receiver = Receiver::default();
        for line in &frames[..frames.len() - 1] {
            assert!(receiver.accept(line, &output).unwrap().is_none());
        }
        assert!(receiver.finish().is_err());
        let result = receiver
            .accept(frames.last().unwrap(), &output)
            .unwrap()
            .unwrap();
        assert_eq!(result.bytes, bytes);
        assert_eq!(result.notification, notification);
        assert_eq!(result.path, path);
        assert!(receiver.finish().is_ok());
        assert!(receiver.accept(frames.last().unwrap(), &output).is_err());
        for bad in [
            r#"{"kind":"Chunk","index":1,"data":"AA=="}"#,
            r#"{"kind":"Chunk","index":0,"data":"A=="}"#,
            r#"{"kind":"End"}"#,
            frames[0],
        ] {
            let mut receiver = Receiver::default();
            receiver.accept(frames[0], &output).unwrap();
            assert!(receiver.accept(bad, &output).is_err(), "{bad}");
        }
        let mut header: serde_json::Value = serde_json::from_str(frames[0]).unwrap();
        header["digest"] = "wrong".into();
        let mut receiver = Receiver::default();
        receiver.accept(&header.to_string(), &output).unwrap();
        for line in &frames[1..frames.len() - 1] {
            receiver.accept(line, &output).unwrap();
        }
        assert!(receiver.accept(frames.last().unwrap(), &output).is_err());
        header["path"] = "/tmp/elsewhere.rmeta".into();
        assert!(
            Receiver::default()
                .accept(&header.to_string(), &output)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
