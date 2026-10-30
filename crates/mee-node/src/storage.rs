//! Durable node storage: the consensus safety state (replaced atomically and
//! fsynced before any vote leaves the node) and an append-only log of
//! committed blocks from which the chain state is rebuilt on restart.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use borsh::{BorshDeserialize, BorshSerialize};
use mee_consensus::{Block, SafetyState};

pub struct Storage {
    dir: PathBuf,
    log: File,
}

pub struct Recovered {
    /// (epoch, state)
    pub safety: Option<(u64, SafetyState)>,
    /// (epoch, block) in commit order.
    pub blocks: Vec<(u64, Block)>,
}

impl Storage {
    /// Opens (or creates) `dir` and reads back what a previous run left.
    /// A torn last log record (crash mid-append) is dropped and truncated.
    pub fn open(dir: &Path) -> io::Result<(Storage, Recovered)> {
        fs::create_dir_all(dir)?;
        let safety = match fs::read(dir.join("safety.bin")) {
            Ok(bytes) => Some(<(u64, SafetyState)>::try_from_slice(&bytes).map_err(io::Error::other)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let path = dir.join("blocks.log");
        let mut blocks = Vec::new();
        let mut good = 0u64;
        if let Ok(mut f) = File::open(&path) {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            let mut at = 0usize;
            while at + 4 <= buf.len() {
                let len = u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
                let Some(body) = buf.get(at + 4..at + 4 + len) else { break };
                let Ok(b) = <(u64, Block)>::try_from_slice(body) else { break };
                blocks.push(b);
                at += 4 + len;
                good = at as u64;
            }
        }
        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        log.set_len(good)?;
        Ok((Storage { dir: dir.to_path_buf(), log }, Recovered { safety, blocks }))
    }

    /// Replaces the safety state: write a temp file, fsync, rename, fsync the
    /// directory. Either the old or the new state survives a crash.
    pub fn save_safety(&mut self, epoch: u64, s: &SafetyState) -> io::Result<()> {
        let tmp = self.dir.join("safety.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&borsh::to_vec(&(epoch, s)).map_err(io::Error::other)?)?;
        f.sync_all()?;
        fs::rename(&tmp, self.dir.join("safety.bin"))?;
        File::open(&self.dir)?.sync_all()
    }

    pub fn append_block(&mut self, epoch: u64, b: &Block) -> io::Result<()> {
        let mut body = Vec::new();
        (epoch, b).serialize(&mut body)?;
        let mut rec = (body.len() as u32).to_le_bytes().to_vec();
        rec.extend_from_slice(&body);
        self.log.write_all(&rec)?;
        self.log.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mee_consensus::{Qc, SafetyState};

    fn block(round: u64) -> Block {
        Block {
            round,
            author: 0,
            qc: Qc { block: [0; 32], round: round - 1, votes: vec![] },
            payload: vec![round as u8; 10],
            signature: [0; 64],
        }
    }

    #[test]
    fn torn_tail_is_dropped_and_safety_survives() {
        let dir = std::env::temp_dir().join(format!("mee-storage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let (mut st, rec) = Storage::open(&dir).unwrap();
            assert!(rec.safety.is_none() && rec.blocks.is_empty());
            st.append_block(0, &block(1)).unwrap();
            st.append_block(0, &block(2)).unwrap();
            let s =
                SafetyState { round: 5, last_voted_round: 4, high_qc: Qc { block: [1; 32], round: 3, votes: vec![] } };
            st.save_safety(0, &s).unwrap();
        }
        // Simulate a crash in the middle of appending a third record.
        let mut f = OpenOptions::new().append(true).open(dir.join("blocks.log")).unwrap();
        f.write_all(&[200, 0, 0, 0, 1, 2, 3]).unwrap();
        drop(f);
        let (mut st, rec) = Storage::open(&dir).unwrap();
        assert_eq!(rec.blocks, vec![(0, block(1)), (0, block(2))]);
        assert_eq!(rec.safety.unwrap().1.last_voted_round, 4);
        st.append_block(0, &block(3)).unwrap();
        drop(st);
        let (_, rec) = Storage::open(&dir).unwrap();
        assert_eq!(rec.blocks.len(), 3, "appends continue cleanly after the truncated tail");
        let _ = fs::remove_dir_all(&dir);
    }
}
