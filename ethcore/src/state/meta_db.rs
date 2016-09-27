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
// along with Parity. If not, see <http://www.gnu.org/licenses/>.

//! Account meta-database.
//!
//! This is a journalled database which stores information on accounts.
//! The key-value mapping is Address -> [code_size, code_hash] where
//! the value is an rlp-encoded list of two items.
//!
//! We can set the meta-db to track a given branch, and to reorganize
//! efficiently to a different branch.

use bigint::hash::{H160, H256};
use bigint::uint::U256;

use util::kvdb::{Database, DBTransaction};
use rlp::{RlpDecodable, RlpEncodable, RlpStream, Stream};

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

// deltas in the journal -- these don't contain data for simple rollbacks.
enum JournalDelta {
	Destroy,
	Set(AccountMeta),
}

// deltas in the branch view -- these contain data making it simple to
// roll back.
enum BranchDelta {
	Destroy(AccountMeta),
	Init(AccountMeta),
	Replace(AccountMeta, AccountMeta),
}

impl<'a> From<&'a BranchDelta> for JournalDelta {
	fn from(bd: &'a BranchDelta) -> Self {
		match *bd {
			BranchDelta::Destroy(_) => JournalDelta::Destroy,
			BranchDelta::Init(ref new) | BranchDelta::Replace(ref new, _) => JournalDelta::Set(new.clone()),
		}
	}
}

/// Account meta-information.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountMeta {
	/// The size of this account's code.
	pub code_size: usize,
	/// The hash of this account's code.
	pub code_hash: H256,
	/// Storage root for the trie.
	pub storage_root: H256,
	/// Account balance.
	pub balance: U256,
	/// Account nonce.
	pub nonce: U256,
}

impl AccountMeta {
	// stream the meta info to RLP.
	fn stream_rlp(&self) -> Vec<u8> {
		RlpStream::new_list(5)
			.append(&self.code_size)
			.append(&self.code_hash)
			.append(&self.storage_root)
			.append(&self.balance)
			.append(&self.nonce)
			.out()
	}

	// build the meta information from (trusted) RLP.
	fn from_rlp(bytes: &[u8]) -> Self {
		let rlp = Rlp::new(bytes);

		AccountMeta {
			code_size: rlp.val_at(0),
			code_hash: rlp.val_at(1),
			storage_root: rlp.val_at(2),
			balance: rlp.val_at(3),
			nonce: rlp.val_at(4),
		}
	}
}

// The journal used to store meta info.
struct Journal {
	// maps block numbers (or more abstractly, eras) to potential canonical meta info.
	entries: BTreeMap<u64, HashMap<H256, JournalEntry>>,
	last_committed: H256,
}

// Each journal entry stores the parent hash of the block it corresponds to
// and the changes in the meta state it lead to.
struct JournalEntry {
	parent: H256,
	entries: Vec<(Account, JournalDelta)>,
}

/// Reorganization is impossible.
pub struct ReorgImpossible;

/// The account meta-database. See the module docs for more details.
/// It can't be queried without a `MetaBranch` which allows for accurate
/// queries along the current branch.
///
/// This has a short journal period, and is only really usable while syncing.
/// When replaying old transactions, it can't be used safely.
#[derive(Debug, Clone)]
pub struct MetaDB {
	col: Option<u32>,
	db: Arc<Database>,
	journal: Journal,
	latest_era: (H256, u64), // last committed era.
	branch: MetaBranch, // current branch.
}

impl MetaDB {
	/// Create a new `MetaDB` from a database and column. This will also load the journal.
	pub fn new(db: Arc<Database>, col: Option<u32>) -> Result<Self, String> {
		let latest_era = unimplemented!();

		let db = MetaDB {
			col: col,
			db: db,
			journal: unimplemented!(), // todo: load and save journal.
			latest_era: latest_era.clone(),
			branch: MetaBranch {
				ancestors: VecDeque::new(),
				current_changes: Vec::new(),
				era: latest_era.1,
				overlay: HashMap::new(),
				recent: HashMap::new(),
			}
		};
	}

	/// Journal all pending changes under the given era and id. Also updates
	/// the branch view to point at this era.
	pub fn journal_under(&mut self, batch: &mut DBTransaction, now: u64, id: H256, parent_id: H256) {
		// convert meta branch pending changes to journal entry.
		let pending: Vec<(Address, JournalDelta)> = self.branch.current_changes
			.iter()
			.map(|&(ref addr, ref delta)| (addr.clone(, delta.into()))
			.collect();

		self.branch.accrue(id);
		self.journal.entries.entry(now).or_insert_with(BTreeMap::new).insert(id, JournalEntry {
			parent_id: H256,
		})
	}

	/// Mark an era as canonical. May invalidate the current branch view.
	///
	/// This immediately sets the last committed hash, leading to a potential
	/// race condition if the meta DB is when .
	/// As such, it's not suitable to be used outside of the main sync.
	pub fn mark_canonical(&mut self, batch: &mut DBTransaction, end_era: u64, canon_id: H256) -> Result<(), String> {
		let entries = match self.journal.entries.remove(end_era) {
			Some(entries) => entries,
			None => {
				warn!("No meta DB journal for era={}", end_era);
				return Ok(())
			}
		};

		// TODO: delete old branches building off of invalidated candidates.
		for (id, entry) in entries {
			let key = RlpStream::new_list(2).append(&end_era).append(&id).drain();
			try!(batch.delete(self.col, &key[..]));

			if id == canon_id {
				for (address, delta) in entry.entries {
					match delta {
						JournalDelta::Destroy => try!(batch.delete(self.col, &*address)),
						JournalDelta::Set(meta) =>
							try!(batch.put_vec(self.col, &*address, meta.stream_rlp())),
					}
				}
			}
		}

		self.last_committed = canon_id;

		// prune the branch view and reset it if it's based off a non-canonical
		// block.
		if !self.branch.remove_ancient(&canon_id) {
			self.clear_branch();
		}
	}

	/// Query the state of an account. A return value
	/// of `None` means that the account does not exist on this branch.
	pub fn get(&self, address: &Address) -> Option<AccountMeta> {
		self.branch.overlay.get(address).or_else(|| {
			match self.database.get(address) {
				Ok(maybe) => maybe.map(::rlp::decode),
				Err(e) => {
					warn!("Low-level database error: {}", e);
					None
				}
			}
		})
	}

	/// Set the head to the requested branch.
	/// The block must be in the journal already.
	///
	/// Will fail if the common ancestor and both branches aren't in the journal.
	/// This shouldn't be possible for anything within the history period.
	///
	/// Note that this will point to the meta state at the point immediately
	/// after the given id.
	///
	/// On failure, branch state is undefined and must be set to a possible
	/// branch before continued use.
	pub fn branch_to(&mut self, hash: H256, new_era: u64) -> Result<(), ReorgImpossible> {
		if new_era == self.latest_era.1 {
			self.clear_branch();

			return if hash != self.latest_era.0 {
				Err(ReorgImpossible);
			} else {
				Ok(())
			}
		}

		// check for equivalent branch.
		let branch_head = self.branch.ancestors.last()
			.map_or_else(|| &self.last_committed, |&(ref h, _)| h);

		let mut to_era = new_era;
		let mut to_branch = vec![];
		let mut ancestor_hash = hash.clone();

		if new_era == self.branch.era && branch_head == &hash { return Ok(()) }

		let journal_entry = |era, id|
			self.journal.entries.get(era)
				.and_then(|entries| entries.get(id)).ok_or(ReorgImpossible);

		// reset to same level by rolling back the branch
		while self.branch.latest_era() > to_era {
			// protected by check above.
			self.branch.rollback().expect("branch known to have enough journalled ancestors; qed");
		}

		while to_era > self.branch.latest_era() {
			let entry = try!(journal_entry(&to_era, &ancestor_hash));

			to_branch.push(ancestor_hash);
			ancestor_hash = entry.parent;
			to_era -= 1;
		}

		// rewind the branch until we find a common ancestor
		while try!(self.branch.latest_id().ok_or(ReorgImpossible)) != ancestor_hash {
			try!(self.branch.rollback().ok_or(ReorgImpossible));

			let entry = try!(journal_entry(&to_era, &ancestor_hash));

			to_branch.push(ancestor_hash);
			ancestor_hash = entry.parent;
			to_era -= 1;
		}

		self.branch.clear(); // clear the current changes overlay before proceeding.

		// and then walk forward, accruing all of the fork branch's changes into
		// the branch.
		for (era, id) in (to_era..new_era).zip(to_era.into_iter().rev()) {
			let entry = journal_entry(&era, &id).expect("this entry fetched previously; qed");

			self.branch.accrue_journal(id, &entry.entries, &*self.db, self.col);
		}

		assert_eq!(self.branch.latest_era(), new_era);
		assert_eq!(self.branch.latest_id(), Some(hash))
	}

	// set the branch to completely empty.
	fn clear_branch(&mut self) {
		self.branch = MetaBranch {
			ancestors: VecDeque::new(),
			current_changes: Vec::new(),
			era: self.latest_era.1,
			overlay: HashMap::new(),
			recent: HashMap::new(),
		};
	}
}

// A reorg-friendly view over a branch based on the `MetaDB`.
#[derive(Debug)]
struct MetaBranch {
	ancestors: VecDeque<(H256, Vec<(Address, Delta)>)>,
	current_changes: Vec<(Address, Delta)>,
	era: u64, // era of the best block in the ancestors.

	// current state of account meta, accruing from the database's last
	// to the current changes. `None` means killed, missing means no change from db,
	// present means known value.
	overlay: HashMap<Address, Option<AccountMeta>>,

	// recently touched addresses -- maps addresses to refcount.
	// when we pop an ancestor. current changes aren't counted
	// until accrued.
	recent: HashMap<Address, u32>,
}

impl MetaBranch {
	// latest tracked era.
	fn latest_era(&self) -> u64 {
		self.era
	}

	// latest tracked id.
	fn latest_id(&self) -> Option<&H256> {
		self.ancestors.last().map(|&(ref hash, _)| hash)
	}

	// clear current changes.
	fn clear_current(&mut self) {
		self.current_changes.clear()
	}

	// Roll back current changes and pop an ancestor. Returns the hash
	// of the ancestor just popped, or none if there isn't one.
	//
	// replaces the current changes with those of the popped ancestor.
	fn rollback(&mut self) -> Option<H256> {
		use std::collections::hash_map::Entry;

		// process changes in reverse for proper backtracking.
		for (address, delta) in self.current_changes.drain(..).rev() {
			match delta {
				BranchDelta::Killed(prev) => self.overlay.insert(address.clone(), Some(prev)),
				BranchDelta::Init(_) => self.overlay.remove(address),
				BranchDelta::Replace(prev) => self.overlay.insert(address.clone(), Some(prev)),
			}
		}

		match self.ancestors.pop_back() {
			Some((hash, delta)) => {
				self.era -= 1;
				self.prune_recent(delta.iter().map(|&(ref addr, _)| addr).cloned());

				self.current_changes = delta;
				Some(hash)
			}
			None => None,
		}
	}

	// Accrue current changes under the given hash, incrementing the latest era.
	fn accrue(&mut self, hash: H256) {
		let current_changes = ::std::mem::replace(&mut self.current_changes, Vec::new());

		for addr in current_changes.iter().map(|&(ref addr, _)| addr).cloned() {
			*self.recent.entry(addr).or_insert(0) += 1;
		}

		self.ancestors.push_back((hash, ::std::mem::replace(&mut self.current_changes, Vec::new())));
		self.era += 1;
	}

	// Accrue deltas from the journal.
	// The hash deltas here must immediately follow the block this branch tracks.
	// This is a relatively expensive operation, but should only be triggered on reorganizations.
	fn accrue_journal(&mut self, hash: H256, j_deltas: &[(Address, JournalDelta)], db: &Database, col: Option<u32>) {
		let mut deltas = Vec::with_capacity(j_deltas.len());

		for &(ref addr, ref j_delta) in j_deltas {
			*self.recent.entry(addr.clone()).or_insert(0) += 1;

			let prev = self.overlay.get(addr)..or_else(|| {
				match db.get(&*addr, col) {
					Ok(maybe_prev) => maybe_prev.map(|raw| AccountMeta::from_rlp(&raw)),
					Err(e) => {
						warn!("Low-level database error: {}", e);
						None
					}
				}
			});

			let delta = match j_delta {
				JournalDelta::Destroy => {
					let prev = prev.expect("cannot destroy without account existing; qed");
					self.overlay.insert(addr.clone(), (hash.clone(), None));

					BranchDelta::Destroy(prev)
				}
				JournalDelta::Set(meta) => {
					self.overlay.insert(addr.clone(), (hash.clone(), Some(meta.clone())));

					match prev {
						Some(prev) => BranchDelta::Replace(meta.clone()),
						None => BranchDelta::Init(meta.clone()),
					}
				}
			};

			deltas.push(delta);
		}

		self.era += 1;
		self.ancestors.push_back(hash, deltas);
	}

	// decrement the refcount in `recent` for any address in the given
	// iterable.
	fn prune_recent<I>(&mut self, iter: I) where I: IntoIterator<Item=Address> {
		for addr in iter {
			match self.recent.entry(addr) {
				Entry::Occupied(x) {
					*x.get_mut() -= 1;
					if x.get() == 0 {
						x.remove();
					}
				}
				_ => {}
			}
		}
	}

	// get rid of the most ancient ancestor, and remove any stale entries from the overlay.
	// if the ancient ancestor isn't equal to the canon id, returns false, true otherwise.
	// this signals that the branch needs to be wiped.
	fn remove_ancient(&mut self, canon_id: &H256) -> bool {
		use std::collections::hash_map::Entry;

		let (hash, delta) = match self.ancestors.pop_front() {
			Some((hash, delta)) if hash == canon_id => (hash, delta),
			_ => return false;
		};

		if self.ancestors.is_empty() { self.era -= 1 }

		self.prune_recent(delta.into_iter().map(|(addr, _)| addr));
		true
	}
}