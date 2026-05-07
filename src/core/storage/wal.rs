use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use serde::{Deserialize, Serialize};
use anyhow::{Result, Context};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum WalOperation {
    Insert { id: String, data: Vec<u8> },
    Delete { id: String },
    Update { id: String, data: Vec<u8> },
}

pub struct Wal {
    writer: BufWriter<File>,
    pub path: std::path::PathBuf,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)
            .with_context(|| format!("Failed to open WAL at {:?}", path))?;

        Ok(Self {
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
        })
    }

    pub fn append(&mut self, op: &WalOperation) -> Result<()> {
        let encoded: Vec<u8> = bincode::serialize(op)?;
        let len = encoded.len() as u64;
        self.writer.write_all(&len.to_le_bytes())?;
        self.writer.write_all(&encoded)?;
        Ok(())
    }

    /// Persist buffered WAL bytes before truncating (required when [`Wal::append`] omits per-op flush).
    pub fn flush_writes(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }

    pub fn replay(&self) -> Result<Vec<WalOperation>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);
        let mut ops = Vec::new();
        let mut len_buf = [0u8; 8];

        loop {
            match reader.read_exact(&mut len_buf) {
                Ok(_) => {
                    let len = u64::from_le_bytes(len_buf) as usize;
                    let mut data_buf = vec![0u8; len];
                    reader.read_exact(&mut data_buf)?;
                    let op: WalOperation = bincode::deserialize(&data_buf)?;
                    ops.push(op);
                }
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
        }

        Ok(ops)
    }

    pub fn truncate(&mut self) -> Result<()> {
        self.writer.flush()?;
        {
            let file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
            file.sync_all()?;
        }
        self.writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.path)?,
        );
        Ok(())
    }

    pub fn sync_directory(&self) -> Result<()> {
        let dir_path = self.path.parent().context("WAL path has no parent directory")?;
        let dir_file = File::open(dir_path).context("WAL directory fsync")?;
        dir_file.sync_all().context("WAL directory fsync")?;
        Ok(())
    }
}
