// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Snapshot network service implementation.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{ManifestData, StateRebuilder, BlockRebuilder, RestorationStatus, SnapshotService};
use super::io::{SnapshotReader, LooseReader, SnapshotWriter, LooseWriter};

use blockchain::BlockChain;
use client::{BlockChainClient, Client};
use engines::Engine;
use error::Error;
use ids::BlockID;
use service::ClientIoMessage;

use io::IoChannel;

use util::{Bytes, H256, Mutex, RwLock, RwLockReadGuard, UtilError};
use util::journaldb::Algorithm;
use util::kvdb::{Database, DatabaseConfig};
use util::snappy;

/// Helper for removing directories in case of error.
struct Guard(bool, PathBuf);

impl Guard {
	fn new(path: PathBuf) -> Self { Guard(true, path) }

	fn disarm(mut self) { self.0 = false }
}

impl Drop for Guard {
	fn drop(&mut self) {
		if self.0 {
			let _ = fs::remove_dir_all(&self.1);
		}
	}
}

/// External database restoration handler
pub trait DatabaseRestore: Send + Sync {
	/// Restart with a new backend. Takes ownership of passed database and moves it to a new location.
	fn restore_db(&self, new_db: &str) -> Result<(), Error>;
}

/// State restoration manager.
struct Restoration {
	manifest: ManifestData,
	state_chunks_left: HashSet<H256>,
	block_chunks_left: HashSet<H256>,
	state: StateRebuilder,
	blocks: BlockRebuilder,
	writer: Option<LooseWriter>,
	snappy_buffer: Bytes,
	final_state_root: H256,
	guard: Guard,
	canonical_hashes: HashMap<u64, H256>,
	db: Arc<Database>,
}

struct RestorationParams<'a> {
	manifest: ManifestData, // manifest to base restoration on.
	pruning: Algorithm, // pruning algorithm for the database.
	db_path: PathBuf, // database path
	db_config: &'a DatabaseConfig, // configuration for the database.
	writer: Option<LooseWriter>, // writer for recovered snapshot.
	genesis: &'a [u8], // genesis block of the chain.
	guard: Guard, // guard for the restoration directory.
}

impl Restoration {
	// make a new restoration using the given parameters.
	fn new(params: RestorationParams) -> Result<Self, Error> {
		let manifest = params.manifest;

		let state_chunks = manifest.state_hashes.iter().cloned().collect();
		let block_chunks = manifest.block_hashes.iter().cloned().collect();

		let raw_db = Arc::new(try!(Database::open(params.db_config, &*params.db_path.to_string_lossy())
			.map_err(UtilError::SimpleString)));

		let chain = BlockChain::new(Default::default(), params.genesis, raw_db.clone());
		let blocks = try!(BlockRebuilder::new(chain, raw_db.clone(), &manifest));

		let root = manifest.state_root.clone();
		Ok(Restoration {
			manifest: manifest,
			state_chunks_left: state_chunks,
			block_chunks_left: block_chunks,
			state: StateRebuilder::new(raw_db.clone(), params.pruning),
			blocks: blocks,
			writer: params.writer,
			snappy_buffer: Vec::new(),
			final_state_root: root,
			guard: params.guard,
			canonical_hashes: HashMap::new(),
			db: raw_db,
		})
	}

	// feeds a state chunk, aborts early if `flag` becomes false.
	fn feed_state(&mut self, hash: H256, chunk: &[u8], flag: &AtomicBool) -> Result<(), Error> {
		if self.state_chunks_left.remove(&hash) {
			let len = try!(snappy::decompress_into(chunk, &mut self.snappy_buffer));

			try!(self.state.feed(&self.snappy_buffer[..len], flag));

			if let Some(ref mut writer) = self.writer.as_mut() {
				try!(writer.write_state_chunk(hash, chunk));
			}
		}

		Ok(())
	}

	// feeds a block chunk
	fn feed_blocks(&mut self, hash: H256, chunk: &[u8], engine: &Engine, flag: &AtomicBool) -> Result<(), Error> {
		if self.block_chunks_left.remove(&hash) {
			let len = try!(snappy::decompress_into(chunk, &mut self.snappy_buffer));

			try!(self.blocks.feed(&self.snappy_buffer[..len], engine, flag));
			if let Some(ref mut writer) = self.writer.as_mut() {
				 try!(writer.write_block_chunk(hash, chunk));
			}
		}

		Ok(())
	}

	// note canonical hashes.
	fn note_canonical(&mut self, hashes: &[(u64, H256)]) {
		self.canonical_hashes.extend(hashes.iter().cloned());
	}

	// finish up restoration.
	fn finalize(self) -> Result<(), Error> {
		use util::trie::TrieError;

		if !self.is_done() { return Ok(()) }

		// verify final state root.
		let root = self.state.state_root();
		if root != self.final_state_root {
			warn!("Final restored state has wrong state root: expected {:?}, got {:?}", root, self.final_state_root);
			return Err(TrieError::InvalidStateRoot(root).into());
		}

		// check for missing code.
		try!(self.state.check_missing());

		// connect out-of-order chunks and verify chain integrity.
		try!(self.blocks.finalize(self.canonical_hashes));

		if let Some(writer) = self.writer {
			try!(writer.finish(self.manifest));
		}

		self.guard.disarm();
		Ok(())
	}

	// is everything done?
	fn is_done(&self) -> bool {
		self.block_chunks_left.is_empty() && self.state_chunks_left.is_empty()
	}
}

/// Type alias for client io channel.
pub type Channel = IoChannel<ClientIoMessage>;

/// Snapshot service parameters.
pub struct ServiceParams {
	/// The consensus engine this is built on.
	pub engine: Arc<Engine>,
	/// The chain's genesis block.
	pub genesis_block: Bytes,
	/// Database configuration options.
	pub db_config: DatabaseConfig,
	/// State pruning algorithm.
	pub pruning: Algorithm,
	/// Async IO channel for sending messages.
	pub channel: Channel,
	/// The directory to put snapshots in.
	/// Usually "<chain hash>/snapshot"
	pub snapshot_root: PathBuf,
	/// A handle for database restoration.
	pub db_restore: Arc<DatabaseRestore>,
}

/// `SnapshotService` implementation.
/// This controls taking snapshots and restoring from them.
pub struct Service {
	restoration: Mutex<Option<Restoration>>,
	snapshot_root: PathBuf,
	db_config: DatabaseConfig,
	io_channel: Mutex<Channel>,
	pruning: Algorithm,
	status: Mutex<RestorationStatus>,
	reader: RwLock<Option<LooseReader>>,
	engine: Arc<Engine>,
	genesis_block: Bytes,
	state_chunks: AtomicUsize,
	block_chunks: AtomicUsize,
	db_restore: Arc<DatabaseRestore>,
	progress: super::Progress,
	taking_snapshot: AtomicBool,
	restoring_snapshot: AtomicBool,
}

impl Service {
	/// Create a new snapshot service from the given parameters.
	pub fn new(params: ServiceParams) -> Result<Self, Error> {
		let mut service = Service {
			restoration: Mutex::new(None),
			snapshot_root: params.snapshot_root,
			db_config: params.db_config,
			io_channel: Mutex::new(params.channel),
			pruning: params.pruning,
			status: Mutex::new(RestorationStatus::Inactive),
			reader: RwLock::new(None),
			engine: params.engine,
			genesis_block: params.genesis_block,
			state_chunks: AtomicUsize::new(0),
			block_chunks: AtomicUsize::new(0),
			db_restore: params.db_restore,
			progress: Default::default(),
			taking_snapshot: AtomicBool::new(false),
			restoring_snapshot: AtomicBool::new(false),
		};

		// create the root snapshot dir if it doesn't exist.
		if let Err(e) = fs::create_dir_all(&service.snapshot_root) {
			if e.kind() != ErrorKind::AlreadyExists {
				return Err(e.into())
			}
		}

		// delete the temporary restoration dir if it does exist.
		if let Err(e) = fs::remove_dir_all(service.restoration_dir()) {
			if e.kind() != ErrorKind::NotFound {
				return Err(e.into())
			}
		}

		// delete the temporary snapshot dir if it does exist.
		if let Err(e) = fs::remove_dir_all(service.temp_snapshot_dir()) {
			if e.kind() != ErrorKind::NotFound {
				return Err(e.into())
			}
		}

		let reader = LooseReader::new(service.snapshot_dir()).ok();
		*service.reader.get_mut() = reader;

		Ok(service)
	}

	// get the current snapshot dir.
	fn snapshot_dir(&self) -> PathBuf {
		let mut dir = self.snapshot_root.clone();
		dir.push("current");
		dir
	}

	// get the temporary snapshot dir.
	fn temp_snapshot_dir(&self) -> PathBuf {
		let mut dir = self.snapshot_root.clone();
		dir.push("in_progress");
		dir
	}

	// get the restoration directory.
	fn restoration_dir(&self) -> PathBuf {
		let mut dir = self.snapshot_root.clone();
		dir.push("restoration");
		dir
	}

	// restoration db path.
	fn restoration_db(&self) -> PathBuf {
		let mut dir = self.restoration_dir();
		dir.push("db");
		dir
	}

	// temporary snapshot recovery path.
	fn temp_recovery_dir(&self) -> PathBuf {
		let mut dir = self.restoration_dir();
		dir.push("temp");
		dir
	}

	// replace one the client's database with our own.
	fn replace_client_db(&self) -> Result<(), Error> {
		let our_db = self.restoration_db();

		try!(self.db_restore.restore_db(&*our_db.to_string_lossy()));
		Ok(())
	}

	/// Get a reference to the snapshot reader.
	pub fn reader(&self) -> RwLockReadGuard<Option<LooseReader>> {
		self.reader.read()
	}

	/// Tick the snapshot service. This will log any active snapshot
	/// being taken.
	pub fn tick(&self) {
		if self.progress.done() || !self.taking_snapshot.load(Ordering::SeqCst) { return }

		let p = &self.progress;
		info!("Snapshot: {} accounts {} blocks {} bytes", p.accounts(), p.blocks(), p.size());
	}

	/// Take a snapshot at the block with the given number.
	/// calling this while a restoration is in progress or vice versa
	/// will lead to a race condition where the first one to finish will
	/// have their produced snapshot overwritten.
	pub fn take_snapshot(&self, client: &Client, num: u64) -> Result<(), Error> {
		if self.taking_snapshot.compare_and_swap(false, true, Ordering::SeqCst) {
			info!("Skipping snapshot at #{} as another one is currently in-progress.", num);
			return Ok(());
		}

		info!("Taking snapshot at #{}", num);
		self.progress.reset();

		let temp_dir = self.temp_snapshot_dir();
		let snapshot_dir = self.snapshot_dir();

		let _ = fs::remove_dir_all(&temp_dir);

		let writer = try!(LooseWriter::new(temp_dir.clone()));

		let guard = Guard::new(temp_dir.clone());
		let res = client.take_snapshot(writer, BlockID::Number(num), &self.progress);

		self.taking_snapshot.store(false, Ordering::SeqCst);
		if let Err(e) = res {
			if client.chain_info().best_block_number >= num + client.pruning_history() {
				// "Cancelled" is mincing words a bit -- what really happened
				// is that the state we were snapshotting got pruned out
				// before we could finish.
				info!("Periodic snapshot failed: block state pruned.\
					Run with a longer `--pruning-history` or with `--no-periodic-snapshot`");
				return Ok(())
			} else {
				return Err(e);
			}
		}

		info!("Finished taking snapshot at #{}", num);

		let mut reader = self.reader.write();

		// destroy the old snapshot reader.
		*reader = None;

		if snapshot_dir.exists() {
			try!(fs::remove_dir_all(&snapshot_dir));
		}

		try!(fs::rename(temp_dir, &snapshot_dir));

		*reader = Some(try!(LooseReader::new(snapshot_dir)));

		guard.disarm();
		Ok(())
	}

	/// Initialize the restoration synchronously.
	/// The recover flag indicates whether to recover the restored snapshot.
	pub fn init_restore(&self, manifest: ManifestData, recover: bool) -> Result<(), Error> {
		let rest_dir = self.restoration_dir();

		let mut res = self.restoration.lock();

		self.state_chunks.store(0, Ordering::SeqCst);
		self.block_chunks.store(0, Ordering::SeqCst);

		// tear down existing restoration.
		*res = None;

		// delete and restore the restoration dir.
		if let Err(e) = fs::remove_dir_all(&rest_dir) {
			match e.kind() {
				ErrorKind::NotFound => {},
				_ => return Err(e.into()),
			}
		}

		try!(fs::create_dir_all(&rest_dir));

		// make new restoration.
		let writer = match recover {
			true => Some(try!(LooseWriter::new(self.temp_recovery_dir()))),
			false => None
		};

		let params = RestorationParams {
			manifest: manifest,
			pruning: self.pruning,
			db_path: self.restoration_db(),
			db_config: &self.db_config,
			writer: writer,
			genesis: &self.genesis_block,
			guard: Guard::new(rest_dir),
		};

		let state_chunks = params.manifest.state_hashes.len();
		let block_chunks = params.manifest.block_hashes.len();

		*res = Some(try!(Restoration::new(params)));

		*self.status.lock() = RestorationStatus::Ongoing {
			state_chunks: state_chunks as u32,
			block_chunks: block_chunks as u32,
			state_chunks_done: self.state_chunks.load(Ordering::SeqCst) as u32,
			block_chunks_done: self.block_chunks.load(Ordering::SeqCst) as u32,
		};

		self.restoring_snapshot.store(true, Ordering::SeqCst);
		Ok(())
	}

	// finalize the restoration. this accepts an already-locked
	// restoration as an argument -- so acquiring it again _will_
	// lead to deadlock.
	fn finalize_restoration(&self, rest: &mut Option<Restoration>) -> Result<(), Error> {
		trace!(target: "snapshot", "finalizing restoration");

		let recover = rest.as_ref().map_or(false, |rest| rest.writer.is_some());

		// destroy the restoration before replacing databases and snapshot.
		try!(rest.take().map(Restoration::finalize).unwrap_or(Ok(())));
		try!(self.replace_client_db());

		if recover {
			let mut reader = self.reader.write();
			*reader = None; // destroy the old reader if it existed.

			let snapshot_dir = self.snapshot_dir();

			if snapshot_dir.exists() {
				trace!(target: "snapshot", "removing old snapshot dir at {}", snapshot_dir.to_string_lossy());
				try!(fs::remove_dir_all(&snapshot_dir));
			}

			trace!(target: "snapshot", "copying restored snapshot files over");
			try!(fs::rename(self.temp_recovery_dir(), &snapshot_dir));

			*reader = Some(try!(LooseReader::new(snapshot_dir)));
		}

		let _ = fs::remove_dir_all(self.restoration_dir());
		*self.status.lock() = RestorationStatus::Inactive;

		Ok(())
	}

	/// Feed a chunk of either kind. no-op if no restoration or status is wrong.
	fn feed_chunk(&self, hash: H256, chunk: &[u8], is_state: bool) -> Result<(), Error> {
		// TODO: be able to process block chunks and state chunks at same time?
		let (result, db) = {
			let mut restoration = self.restoration.lock();

			match self.status() {
				RestorationStatus::Inactive | RestorationStatus::Failed => return Ok(()),
				RestorationStatus::Ongoing { .. } => {
					let (res, db) = {
						let rest = match *restoration {
							Some(ref mut r) => r,
							None => return Ok(()),
						};

						(match is_state {
							true => rest.feed_state(hash, chunk, &self.restoring_snapshot),
							false => rest.feed_blocks(hash, chunk, &*self.engine, &self.restoring_snapshot),
						}.map(|_| rest.is_done()), rest.db.clone())
					};

					let res = match res {
						Ok(is_done) => {
							match is_state {
								true => self.state_chunks.fetch_add(1, Ordering::SeqCst),
								false => self.block_chunks.fetch_add(1, Ordering::SeqCst),
							};

							match is_done {
								true => {
									try!(db.flush().map_err(::util::UtilError::SimpleString));
									drop(db);
									return self.finalize_restoration(&mut *restoration);
								},
								false => Ok(())
							}
						}
						other => other.map(drop),
					};
					(res, db)
				}
			}
		};
		result.and_then(|_| db.flush().map_err(|e| ::util::UtilError::SimpleString(e).into()))
	}

	/// Feed a state chunk to be processed synchronously.
	pub fn feed_state_chunk(&self, hash: H256, chunk: &[u8]) {
		match self.feed_chunk(hash, chunk, true) {
			Ok(()) => (),
			Err(e) => {
				warn!("Encountered error during state restoration: {}", e);
				*self.restoration.lock() = None;
				*self.status.lock() = RestorationStatus::Failed;
				let _ = fs::remove_dir_all(self.restoration_dir());
			}
		}
	}

	/// Feed a block chunk to be processed synchronously.
	pub fn feed_block_chunk(&self, hash: H256, chunk: &[u8]) {
		match self.feed_chunk(hash, chunk, false) {
			Ok(()) => (),
			Err(e) => {
				warn!("Encountered error during block restoration: {}", e);
				*self.restoration.lock() = None;
				*self.status.lock() = RestorationStatus::Failed;
				let _ = fs::remove_dir_all(self.restoration_dir());
			}
		}
	}
}

impl SnapshotService for Service {
	fn manifest(&self) -> Option<ManifestData> {
		self.reader.read().as_ref().map(|r| r.manifest().clone())
	}

	fn chunk(&self, hash: H256) -> Option<Bytes> {
		self.reader.read().as_ref().and_then(|r| r.chunk(hash).ok())
	}

	fn status(&self) -> RestorationStatus {
		let mut cur_status = self.status.lock();
		if let RestorationStatus::Ongoing { ref mut state_chunks_done, ref mut block_chunks_done, .. } = *cur_status {
			*state_chunks_done = self.state_chunks.load(Ordering::SeqCst) as u32;
			*block_chunks_done = self.block_chunks.load(Ordering::SeqCst) as u32;
		}

		cur_status.clone()
	}

	fn begin_restore(&self, manifest: ManifestData) {
		if let Err(e) = self.io_channel.lock().send(ClientIoMessage::BeginRestoration(manifest)) {
			trace!("Error sending snapshot service message: {:?}", e);
		}
	}

	fn abort_restore(&self) {
		self.restoring_snapshot.store(false, Ordering::SeqCst);
		*self.restoration.lock() = None;
		*self.status.lock() = RestorationStatus::Inactive;
	}

	fn restore_state_chunk(&self, hash: H256, chunk: Bytes) {
		if let Err(e) = self.io_channel.lock().send(ClientIoMessage::FeedStateChunk(hash, chunk)) {
			trace!("Error sending snapshot service message: {:?}", e);
		}
	}

	fn restore_block_chunk(&self, hash: H256, chunk: Bytes) {
		if let Err(e) = self.io_channel.lock().send(ClientIoMessage::FeedBlockChunk(hash, chunk)) {
			trace!("Error sending snapshot service message: {:?}", e);
		}
	}

	fn provide_canon_hashes(&self, canonical: &[(u64, H256)]) {
		let mut rest = self.restoration.lock();

		if let Some(ref mut rest) = rest.as_mut() {
			rest.note_canonical(canonical);
		}
	}
}

impl Drop for Service {
	fn drop(&mut self) {
		self.abort_restore();
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use service::ClientIoMessage;
	use io::{IoService};
	use devtools::RandomTempPath;
	use tests::helpers::get_test_spec;
	use util::journaldb::Algorithm;
	use error::Error;
	use snapshot::{ManifestData, RestorationStatus, SnapshotService};
	use super::*;

	struct NoopDBRestore;
	impl DatabaseRestore for NoopDBRestore {
		fn restore_db(&self, _new_db: &str) -> Result<(), Error> {
			Ok(())
		}
	}

	#[test]
	fn sends_async_messages() {
		let service = IoService::<ClientIoMessage>::start().unwrap();
		let spec = get_test_spec();

		let dir = RandomTempPath::new();
		let mut dir = dir.as_path().to_owned();
		let mut client_db = dir.clone();
		dir.push("snapshot");
		client_db.push("client");

		let snapshot_params = ServiceParams {
			engine: spec.engine.clone(),
			genesis_block: spec.genesis_block(),
			db_config: Default::default(),
			pruning: Algorithm::Archive,
			channel: service.channel(),
			snapshot_root: dir,
			db_restore: Arc::new(NoopDBRestore),
		};

		let service = Service::new(snapshot_params).unwrap();

		assert!(service.manifest().is_none());
		assert!(service.chunk(Default::default()).is_none());
		assert_eq!(service.status(), RestorationStatus::Inactive);

		let manifest = ManifestData {
			state_hashes: vec![],
			block_hashes: vec![],
			state_root: Default::default(),
			block_number: 0,
			block_hash: Default::default(),
		};

		service.begin_restore(manifest);
		service.abort_restore();
		service.restore_state_chunk(Default::default(), vec![]);
		service.restore_block_chunk(Default::default(), vec![]);
	}
}
