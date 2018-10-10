// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Import Queue primitive: something which can verify and import blocks.
//!
//! This serves as an intermediate and abstracted step between synchronization
//! and import. Each mode of consensus will have its own requirements for block verification.
//! Some algorithms can verify in parallel, while others only sequentially.
//!
//! The `ImportQueue` trait allows such verification strategies to be instantiated.
//! The `BasicQueue` and `BasicVerifier` traits allow serial queues to be
//! instantiated simply.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::{Condvar, Mutex, RwLock};

use client::{BlockOrigin, ImportBlock, ImportResult};
use network_libp2p::{NodeIndex, Severity};

use runtime_primitives::traits::{Block as BlockT, Header as HeaderT, NumberFor, Zero};

use blocks::BlockData;
use chain::Client;
use error::{ErrorKind, Error};
use protocol::Context;
use service::ExecuteInContext;
use sync::ChainSync;

#[cfg(any(test, feature = "test-helpers"))]
use std::cell::RefCell;

/// Blocks import queue API.
pub trait ImportQueue<B: BlockT>: Send + Sync {
	/// Start background work for the queue as necessary.
	///
	/// This is called automatically by the network service when synchronization
	/// begins.

	fn start<E>(
		&self,
		_sync: Weak<RwLock<ChainSync<B>>>,
		_service: Weak<E>,
		_chain: Weak<Client<B>>
	) -> Result<(), Error> where
		Self: Sized,
		E: 'static + ExecuteInContext<B>,
	{
		Ok(())
	}
	/// Clear the queue when sync is restarting.
	fn clear(&self);
	/// Clears the import queue and stops importing.
	fn stop(&self);
	/// Get queue status.
	fn status(&self) -> ImportQueueStatus<B>;
	/// Is block with given hash currently in the queue.
	fn is_importing(&self, hash: &B::Hash) -> bool;
	/// Import bunch of blocks.
	fn import_blocks(&self, origin: BlockOrigin, blocks: Vec<BlockData<B>>);
}

/// Import queue status. It isn't completely accurate.
pub struct ImportQueueStatus<B: BlockT> {
	/// Number of blocks that are currently in the queue.
	pub importing_count: usize,
	/// The number of the best block that was ever in the queue since start/last failure.
	pub best_importing_number: <<B as BlockT>::Header as HeaderT>::Number,
}

/// Basic block import queue that is importing blocks sequentially in a separate thread,
/// with pluggable verification.
pub struct BasicQueue<B: BlockT> {
	handle: Mutex<Option<::std::thread::JoinHandle<()>>>,
	data: Arc<AsyncImportQueueData<B>>,
	instant_finality: bool,
}

/// Locks order: queue, queue_blocks, best_importing_number
struct AsyncImportQueueData<B: BlockT> {
	signal: Condvar,
	queue: Mutex<VecDeque<(BlockOrigin, Vec<BlockData<B>>)>>,
	queue_blocks: RwLock<HashSet<B::Hash>>,
	best_importing_number: RwLock<<<B as BlockT>::Header as HeaderT>::Number>,
	is_stopping: AtomicBool,
}

impl<B: BlockT> BasicQueue<B> {
	/// Instantiate a new basic queue, with given verifier.
	pub fn new(instant_finality: bool) -> Self {
		Self {
			handle: Mutex::new(None),
			data: Arc::new(AsyncImportQueueData::new()),
			instant_finality,
		}
	}
}

impl<B: BlockT> AsyncImportQueueData<B> {
	fn new() -> Self {
		Self {
			signal: Default::default(),
			queue: Mutex::new(VecDeque::new()),
			queue_blocks: RwLock::new(HashSet::new()),
			best_importing_number: RwLock::new(Zero::zero()),
			is_stopping: Default::default(),
		}
	}
}

impl<B: BlockT> ImportQueue<B> for BasicQueue<B> {
	fn start<E: 'static + ExecuteInContext<B>>(
		&self,
		sync: Weak<RwLock<ChainSync<B>>>,
		service: Weak<E>,
		chain: Weak<Client<B>>
	) -> Result<(), Error> {
		debug_assert!(self.handle.lock().is_none());

		let qdata = self.data.clone();
		let instant_finality = self.instant_finality;
		*self.handle.lock() = Some(::std::thread::Builder::new().name("ImportQueue".into()).spawn(move || {
			import_thread(sync, service, chain, qdata, instant_finality)
		}).map_err(|err| Error::from(ErrorKind::Io(err)))?);
		Ok(())
	}

	fn clear(&self) {
		let mut queue = self.data.queue.lock();
		let mut queue_blocks = self.data.queue_blocks.write();
		let mut best_importing_number = self.data.best_importing_number.write();
		queue_blocks.clear();
		queue.clear();
		*best_importing_number = Zero::zero();
	}

	fn stop(&self) {
		self.clear();
		if let Some(handle) = self.handle.lock().take() {
			self.data.is_stopping.store(true, Ordering::SeqCst);
			self.data.signal.notify_one();

			let _ = handle.join();
		}
	}

	fn status(&self) -> ImportQueueStatus<B> {
		ImportQueueStatus {
			importing_count: self.data.queue_blocks.read().len(),
			best_importing_number: *self.data.best_importing_number.read(),
		}
	}

	fn is_importing(&self, hash: &B::Hash) -> bool {
		self.data.queue_blocks.read().contains(hash)
	}

	fn import_blocks(&self, origin: BlockOrigin, blocks: Vec<BlockData<B>>) {
		if blocks.is_empty() {
			return;
		}

		trace!(target:"sync", "Scheduling {} blocks for import", blocks.len());

		let mut queue = self.data.queue.lock();
		let mut queue_blocks = self.data.queue_blocks.write();
		let mut best_importing_number = self.data.best_importing_number.write();
		let new_best_importing_number = blocks.last().and_then(|b| b.block.header.as_ref().map(|h| h.number().clone())).unwrap_or_else(|| Zero::zero());
		queue_blocks.extend(blocks.iter().map(|b| b.block.hash.clone()));
		if new_best_importing_number > *best_importing_number {
			*best_importing_number = new_best_importing_number;
		}
		queue.push_back((origin, blocks));
		self.data.signal.notify_one();
	}
}

impl<B: BlockT> Drop for BasicQueue<B> {
	fn drop(&mut self) {
		self.stop();
	}
}

/// Blocks import thread.
fn import_thread<B: BlockT, E: ExecuteInContext<B>>(
	sync: Weak<RwLock<ChainSync<B>>>,
	service: Weak<E>,
	chain: Weak<Client<B>>,
	qdata: Arc<AsyncImportQueueData<B>>,
	instant_finality: bool,
) {
	trace!(target: "sync", "Starting import thread");
	loop {
		if qdata.is_stopping.load(Ordering::SeqCst) {
			break;
		}

		let new_blocks = {
			let mut queue_lock = qdata.queue.lock();
			if queue_lock.is_empty() {
				qdata.signal.wait(&mut queue_lock);
			}

			match queue_lock.pop_front() {
				Some(new_blocks) => new_blocks,
				None => break,
			}
		};

		match (sync.upgrade(), service.upgrade(), chain.upgrade()) {
			(Some(sync), Some(service), Some(chain)) => {
				let blocks_hashes: Vec<B::Hash> = new_blocks.1.iter().map(|b| b.block.hash.clone()).collect();
				if !import_many_blocks(
					&mut SyncLink{chain: &sync, client: &*chain, context: &*service},
					Some(&*qdata),
					new_blocks,
					instant_finality,
				) {
					break;
				}

				let mut queue_blocks = qdata.queue_blocks.write();
				for blocks_hash in blocks_hashes {
					queue_blocks.remove(&blocks_hash);
				}
			},
			_ => break,
		}
	}

	trace!(target: "sync", "Stopping import thread");
}
/// ChainSync link trait.
trait SyncLinkApi<B: BlockT> {
	/// Get chain reference.
	fn chain(&self) -> &Client<B>;
	/// Block imported.
	fn block_imported(&mut self, hash: &B::Hash, number: NumberFor<B>);
	/// Maintain sync.
	fn maintain_sync(&mut self);
	/// Disconnect from peer.
	fn useless_peer(&mut self, who: NodeIndex, reason: &str);
	/// Disconnect from peer and restart sync.
	fn note_useless_and_restart_sync(&mut self, who: NodeIndex, reason: &str);
	/// Restart sync.
	fn restart(&mut self);
}


/// Link with the ChainSync service.
struct SyncLink<'a, B: 'a + BlockT, E: 'a + ExecuteInContext<B>> {
	pub chain: &'a RwLock<ChainSync<B>>,
	pub client: &'a Client<B>,
	pub context: &'a E,
}

impl<'a, B: 'static + BlockT, E: 'a + ExecuteInContext<B>> SyncLink<'a, B, E> {
	/// Execute closure with locked ChainSync. 
	fn with_sync<F: Fn(&mut ChainSync<B>, &mut Context<B>)>(&mut self, closure: F) {
		let service = self.context;
		let sync = self.chain;
		service.execute_in_context(move |protocol| {
			let mut sync = sync.write();
			closure(&mut *sync, protocol)
		});
	}
}

impl<'a, B: 'static + BlockT, E: 'a + ExecuteInContext<B>> SyncLinkApi<B> for SyncLink<'a, B, E> {

	fn chain(&self) -> &Client<B> {
		self.client
	}

	fn block_imported(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		self.with_sync(|sync, _| sync.block_imported(&hash, number))
	}

	fn maintain_sync(&mut self) {
		self.with_sync(|sync, protocol| sync.maintain_sync(protocol))
	}

	fn useless_peer(&mut self, who: NodeIndex, reason: &str) {
		self.with_sync(|_, protocol| protocol.report_peer(who, Severity::Useless(reason)))
	}

	fn note_useless_and_restart_sync(&mut self, who: NodeIndex, reason: &str) {
		self.with_sync(|sync, protocol| {
			protocol.report_peer(who, Severity::Useless(reason));	// is this actually malign or just useless?
			sync.restart(protocol);
		})
	}

	fn restart(&mut self) {
		self.with_sync(|sync, protocol| sync.restart(protocol))
	}
}

/// Block import successful result.
#[derive(Debug, PartialEq)]
enum BlockImportResult<H: ::std::fmt::Debug + PartialEq, N: ::std::fmt::Debug + PartialEq> {
	/// Imported known block.
	ImportedKnown(H, N),
	/// Imported unknown block.
	ImportedUnknown(H, N),
}

/// Block import error.
#[derive(Debug, PartialEq)]
enum BlockImportError {
	/// Disconnect from peer and continue import of next bunch of blocks.
	Disconnect(NodeIndex),
	/// Disconnect from peer and restart sync.
	DisconnectAndRestart(NodeIndex),
	/// Restart sync.
	Restart,
}

/// Import a bunch of blocks.
fn import_many_blocks<'a, B: BlockT>(
	link: &mut SyncLinkApi<B>,
	qdata: Option<&AsyncImportQueueData<B>>,
	blocks: (BlockOrigin, Vec<BlockData<B>>),
	instant_finality: bool,
) -> bool
{
	let (blocks_origin, blocks) = blocks;
	let count = blocks.len();
	let mut imported = 0;

	let blocks_range = match (
			blocks.first().and_then(|b| b.block.header.as_ref().map(|h| h.number())),
			blocks.last().and_then(|b| b.block.header.as_ref().map(|h| h.number())),
		) {
			(Some(first), Some(last)) if first != last => format!(" ({}..{})", first, last),
			(Some(first), Some(_)) => format!(" ({})", first),
			_ => Default::default(),
		};
	trace!(target:"sync", "Starting import of {} blocks {}", count, blocks_range);

	// Blocks in the response/drain should be in ascending order.
	for block in blocks {
		let import_result = import_single_block(
			link.chain(),
			blocks_origin.clone(),
			block,
			instant_finality,
		);
		let is_import_failed = import_result.is_err();
		imported += process_import_result(link, import_result);
		if is_import_failed {
			qdata.map(|qdata| *qdata.best_importing_number.write() = Zero::zero());
			return true;
		}

		if qdata.map(|qdata| qdata.is_stopping.load(Ordering::SeqCst)).unwrap_or_default() {
			return false;
		}
	}

	trace!(target: "sync", "Imported {} of {}", imported, count);
	link.maintain_sync();
	true
}

/// Single block import function.
fn import_single_block<B: BlockT>(
	chain: &Client<B>,
	block_origin: BlockOrigin,
	block: BlockData<B>,
	instant_finality: bool,
) -> Result<BlockImportResult<B::Hash, <<B as BlockT>::Header as HeaderT>::Number>, BlockImportError>
{
	let origin = block.origin;
	let block = block.block;
	match (block.header, block.justification) {
		(Some(header), Some(justification)) => {
			let number = header.number().clone();
			let hash = header.hash();
			let parent = header.parent_hash().clone();

			let result = chain.import(ImportBlock {
				origin: block_origin,
				header: header,
				external_justification: justification,
				internal_justification: vec![],
				body: block.body,
				finalized: instant_finality,
				auxiliary: Vec::new(),
			}, None);
			match result {
				Ok(ImportResult::AlreadyInChain) => {
					trace!(target: "sync", "Block already in chain {}: {:?}", number, hash);
					Ok(BlockImportResult::ImportedKnown(hash, number))
				},
				Ok(ImportResult::AlreadyQueued) => {
					trace!(target: "sync", "Block already queued {}: {:?}", number, hash);
					Ok(BlockImportResult::ImportedKnown(hash, number))
				},
				Ok(ImportResult::Queued) => {
					trace!(target: "sync", "Block queued {}: {:?}", number, hash);
					Ok(BlockImportResult::ImportedUnknown(hash, number))
				},
				Ok(ImportResult::UnknownParent) => {
					debug!(target: "sync", "Block with unknown parent {}: {:?}, parent: {:?}", number, hash, parent);
					Err(BlockImportError::Restart)
				},
				Ok(ImportResult::KnownBad) => {
					debug!(target: "sync", "Peer gave us a bad block {}: {:?}", number, hash);
					Err(BlockImportError::DisconnectAndRestart(origin)) //TODO: use persistent ID
				}
				Err(e) => {
					debug!(target: "sync", "Error importing block {}: {:?}: {:?}", number, hash, e);
					Err(BlockImportError::Restart)
				}
			}
		},
		(None, _) => {
			debug!(target: "sync", "Header {} was not provided by {} ", block.hash, origin);
			Err(BlockImportError::Disconnect(origin)) //TODO: use persistent ID
		},
		(_, None) => {
			debug!(target: "sync", "Justification set for block {} was not provided by {} ", block.hash, origin);
			Err(BlockImportError::Disconnect(origin)) //TODO: use persistent ID
		}
	}
}

/// Process single block import result.
fn process_import_result<'a, B: BlockT>(
	link: &mut SyncLinkApi<B>,
	result: Result<BlockImportResult<B::Hash, <<B as BlockT>::Header as HeaderT>::Number>, BlockImportError>
) -> usize
{
	match result {
		Ok(BlockImportResult::ImportedKnown(hash, number)) => {
			link.block_imported(&hash, number);
			1
		},
		Ok(BlockImportResult::ImportedUnknown(hash, number)) => {
			link.block_imported(&hash, number);
			1
		},
		Err(BlockImportError::Disconnect(who)) => {
			// TODO: FIXME: @arkpar BlockImport shouldn't be trying to manage the peer set.
			// This should contain an actual reason.
			link.useless_peer(who, "Import result was stated Disconnect");
			0
		},
		Err(BlockImportError::DisconnectAndRestart(who)) => {
			// TODO: FIXME: @arkpar BlockImport shouldn't be trying to manage the peer set.
			// This should contain an actual reason.
			link.note_useless_and_restart_sync(who, "Import result was stated DisconnectAndRestart");
			0
		},
		Err(BlockImportError::Restart) => {
			link.restart();
			0
		},
	}
}


#[cfg(any(test, feature = "test-helpers"))]
pub struct ImportCB<B: BlockT>(RefCell<Option<Box<dyn Fn(BlockOrigin, Vec<BlockData<B>>) -> bool>>>);

#[cfg(any(test, feature = "test-helpers"))]
impl<B: BlockT> ImportCB<B> {
	pub fn new() -> Self {
		ImportCB(RefCell::new(None))
	}
	pub fn set<F>(&self, cb: Box<F>)
		where F: 'static + Fn(BlockOrigin, Vec<BlockData<B>>) -> bool
	{
		*self.0.borrow_mut() = Some(cb);
	}
	pub fn call(&self, origin: BlockOrigin, data: Vec<BlockData<B>>) -> bool {
		let b = self.0.borrow();
		b.as_ref().expect("The Callback has been set before. qed.")(origin, data)
	}
}

#[cfg(any(test, feature = "test-helpers"))]
unsafe impl<B: BlockT> Send for ImportCB<B> {}
#[cfg(any(test, feature = "test-helpers"))]
unsafe impl<B: BlockT> Sync for ImportCB<B> {}

/// Blocks import queue that is importing blocks in the same thread.
/// The boolean value indicates whether blocks should be imported without instant finality.
#[cfg(any(test, feature = "test-helpers"))]
pub struct SyncImportQueue<B: BlockT>(bool, ImportCB<B>);
#[cfg(any(test, feature = "test-helpers"))]
impl<B: BlockT> SyncImportQueue<B> {
	pub fn new(finale: bool) -> Self {
		SyncImportQueue(finale, ImportCB::new())
	}
}

#[cfg(any(test, feature = "test-helpers"))]
impl<B: 'static + BlockT> ImportQueue<B> for SyncImportQueue<B>
{
	fn start<E: 'static + ExecuteInContext<B>>(
		&self,
		sync: Weak<RwLock<ChainSync<B>>>,
		service: Weak<E>,
		chain: Weak<Client<B>>
	) -> Result<(), Error> {
		self.1.set(Box::new(move | origin, new_blocks | {
			match (sync.upgrade(), service.upgrade(), chain.upgrade()) {
				(Some(sync), Some(service), Some(chain)) => import_many_blocks(
						&mut SyncLink{chain: &sync, client: &*chain, context: &*service},
						None,
						(origin, new_blocks),
						true,
					),
				_ => false
			}
		}));
		Ok(())
	}
	fn clear(&self) { }

	fn stop(&self) { }

	fn status(&self) -> ImportQueueStatus<B> {
		ImportQueueStatus {
			importing_count: 0,
			best_importing_number: Zero::zero(),
		}
	}

	fn is_importing(&self, _hash: &B::Hash) -> bool {
		false
	}

	fn import_blocks(&self, origin: BlockOrigin, blocks: Vec<BlockData<B>>) {
		self.1.call(origin, blocks);
	}
}

#[cfg(test)]
pub mod tests {
	use client;
	use message;
	use test_client::{self, TestClient};
	use test_client::runtime::{Block, Hash};
	use on_demand::tests::DummyExecutor;
	use runtime_primitives::generic::BlockId;
	use super::*;


	struct TestLink {
		chain: Arc<Client<Block>>,
		imported: usize,
		maintains: usize,
		disconnects: usize,
		restarts: usize,
	}

	impl TestLink {
		fn new() -> TestLink {
			TestLink {
				chain: Arc::new(test_client::new()),
				imported: 0,
				maintains: 0,
				disconnects: 0,
				restarts: 0,
			}
		}

		fn total(&self) -> usize {
			self.imported + self.maintains + self.disconnects + self.restarts
		}
	}

	impl SyncLinkApi<Block> for TestLink {
		fn chain(&self) -> &Client<Block> { &*self.chain }
		fn block_imported(&mut self, _hash: &Hash, _number: NumberFor<Block>) { self.imported += 1; }
		fn maintain_sync(&mut self) { self.maintains += 1; }
		fn useless_peer(&mut self, _: NodeIndex, _: &str) { self.disconnects += 1; }
		fn note_useless_and_restart_sync(&mut self, _: NodeIndex, _: &str) { self.disconnects += 1; self.restarts += 1; }
		fn restart(&mut self) { self.restarts += 1; }
	}

	fn prepare_good_block() -> (client::Client<test_client::Backend, test_client::Executor, Block>, Hash, u64, BlockData<Block>) {
		let client = test_client::new();
		let block = client.new_block().unwrap().bake().unwrap();
		client.justify_and_import(BlockOrigin::File, block).unwrap();

		let (hash, number) = (client.block_hash(1).unwrap().unwrap(), 1);
		let block = message::BlockData::<Block> {
			hash: client.block_hash(1).unwrap().unwrap(),
			header: client.header(&BlockId::Number(1)).unwrap(),
			body: None,
			receipt: None,
			message_queue: None,
			justification: client.justification(&BlockId::Number(1)).unwrap(),
		};

		(client, hash, number, BlockData { block, origin: 0 })
	}

	#[test]
	fn import_single_good_block_works() {
		let (_, hash, number, block) = prepare_good_block();
		assert_eq!(
			import_single_block(&test_client::new(), BlockOrigin::File, block, true),
			Ok(BlockImportResult::ImportedUnknown(hash, number))
		);
	}

	#[test]
	fn import_single_good_known_block_is_ignored() {
		let (client, hash, number, block) = prepare_good_block();
		assert_eq!(
			import_single_block(&client, BlockOrigin::File, block, true),
			Ok(BlockImportResult::ImportedKnown(hash, number))
		);
	}

	#[test]
	fn import_single_good_block_without_header_fails() {
		let (_, _, _, mut block) = prepare_good_block();
		block.block.header = None;
		assert_eq!(
			import_single_block(&test_client::new(), BlockOrigin::File, block, true),
			Err(BlockImportError::Disconnect(0))
		);
	}

	#[test]
	fn import_single_good_block_without_justification_fails() {
		let (_, _, _, mut block) = prepare_good_block();
		block.block.justification = None;
		assert_eq!(
			import_single_block(&test_client::new(), BlockOrigin::File, block, true),
			Err(BlockImportError::Disconnect(0))
		);
	}

	#[test]
	fn process_import_result_works() {
		let mut link = TestLink::new();
		assert_eq!(process_import_result::<Block>(&mut link, Ok(BlockImportResult::ImportedKnown(Default::default(), 0))), 1);
		assert_eq!(link.total(), 1);

		let mut link = TestLink::new();
		assert_eq!(process_import_result::<Block>(&mut link, Ok(BlockImportResult::ImportedKnown(Default::default(), 0))), 1);
		assert_eq!(link.total(), 1);
		assert_eq!(link.imported, 1);

		let mut link = TestLink::new();
		assert_eq!(process_import_result::<Block>(&mut link, Ok(BlockImportResult::ImportedUnknown(Default::default(), 0))), 1);
		assert_eq!(link.total(), 1);
		assert_eq!(link.imported, 1);

		let mut link = TestLink::new();
		assert_eq!(process_import_result::<Block>(&mut link, Err(BlockImportError::Disconnect(0))), 0);
		assert_eq!(link.total(), 1);
		assert_eq!(link.disconnects, 1);

		let mut link = TestLink::new();
		assert_eq!(process_import_result::<Block>(&mut link, Err(BlockImportError::DisconnectAndRestart(0))), 0);
		assert_eq!(link.total(), 2);
		assert_eq!(link.disconnects, 1);
		assert_eq!(link.restarts, 1);

		let mut link = TestLink::new();
		assert_eq!(process_import_result::<Block>(&mut link, Err(BlockImportError::Restart)), 0);
		assert_eq!(link.total(), 1);
		assert_eq!(link.restarts, 1);
	}

	#[test]
	fn import_many_blocks_stops_when_stopping() {
		let (_, _, _, block) = prepare_good_block();
		let qdata = AsyncImportQueueData::new();
		qdata.is_stopping.store(true, Ordering::SeqCst);
		assert!(!import_many_blocks(
			&mut TestLink::new(),
			Some(&qdata),
			(BlockOrigin::File, vec![block.clone(), block]),
			true
		));
	}

	#[test]
	fn async_import_queue_drops() {
		let queue = BasicQueue::new(true);
		let service = Arc::new(DummyExecutor);
		let chain = Arc::new(test_client::new());
		queue.start(Weak::new(), Arc::downgrade(&service), Arc::downgrade(&chain) as Weak<Client<Block>>).unwrap();
		drop(queue);
	}
}
