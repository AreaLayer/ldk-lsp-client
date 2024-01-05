use crate::events::{Event, EventQueue};
use crate::lsps0::client::LSPS0ClientHandler;
use crate::lsps0::msgs::{
	LSPS0Message, LSPSMessage, ProtocolMessageHandler, RawLSPSMessage, LSPS_MESSAGE_TYPE_ID,
};
use crate::lsps0::service::LSPS0ServiceHandler;
use crate::message_queue::MessageQueue;

#[cfg(lsps1)]
use crate::lsps1::client::{LSPS1ClientConfig, LSPS1ClientHandler};
#[cfg(lsps1)]
use crate::lsps1::msgs::LSPS1Message;
#[cfg(lsps1)]
use crate::lsps1::service::{LSPS1ServiceConfig, LSPS1ServiceHandler};

use crate::lsps2::client::{LSPS2ClientConfig, LSPS2ClientHandler};
use crate::lsps2::msgs::LSPS2Message;
use crate::lsps2::service::{LSPS2ServiceConfig, LSPS2ServiceHandler};
use crate::prelude::{HashMap, String, Vec};
use crate::sync::{Arc, Mutex, RwLock};

use lightning::chain::{self, BestBlock, Confirm, Filter, Listen};
use lightning::ln::channelmanager::{AChannelManager, ChainParameters};
use lightning::ln::features::{InitFeatures, NodeFeatures};
use lightning::ln::msgs::{ErrorAction, LightningError};
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::CustomMessageReader;
use lightning::sign::EntropySource;
use lightning::util::logger::Level;
use lightning::util::ser::Readable;

use bitcoin::secp256k1::PublicKey;

use core::ops::Deref;
const LSPS_FEATURE_BIT: usize = 729;

/// A server-side configuration for [`LiquidityManager`].
///
/// Allows end-users to configure options when using the [`LiquidityManager`]
/// to provide liquidity services to clients.
pub struct LiquidityServiceConfig {
	/// Optional server-side configuration for LSPS1 channel requests.
	#[cfg(lsps1)]
	pub lsps1_service_config: Option<LSPS1ServiceConfig>,
	/// Optional server-side configuration for JIT channels
	/// should you want to support them.
	pub lsps2_service_config: Option<LSPS2ServiceConfig>,
}

/// A client-side configuration for [`LiquidityManager`].
///
/// Allows end-user to configure options when using the [`LiquidityManager`]
/// to access liquidity services from a provider.
pub struct LiquidityClientConfig {
	/// Optional client-side configuration for LSPS1 channel requests.
	#[cfg(lsps1)]
	pub lsps1_client_config: Option<LSPS1ClientConfig>,
	/// Optional client-side configuration for JIT channels.
	pub lsps2_client_config: Option<LSPS2ClientConfig>,
}

/// The main interface into LSP functionality.
///
/// Should be used as a [`CustomMessageHandler`] for your [`PeerManager`]'s [`MessageHandler`].
///
/// Users should provide a callback to process queued messages via
/// [`LiquidityManager::set_process_msgs_callback`] post construction. This allows the
/// [`LiquidityManager`] to wake the [`PeerManager`] when there are pending messages to be sent.
///
/// Users need to continually poll [`LiquidityManager::get_and_clear_pending_events`] in order to surface
/// [`Event`]'s that likely need to be handled.
///
/// If configured, users must forward the [`Event::HTLCIntercepted`] event parameters to [`LSPS2ServiceHandler::htlc_intercepted`]
/// and the [`Event::ChannelReady`] event parameters to [`LSPS2ServiceHandler::channel_ready`].
///
/// [`PeerManager`]: lightning::ln::peer_handler::PeerManager
/// [`MessageHandler`]: lightning::ln::peer_handler::MessageHandler
/// [`Event::HTLCIntercepted`]: lightning::events::Event::HTLCIntercepted
/// [`Event::ChannelReady`]: lightning::events::Event::ChannelReady
pub struct LiquidityManager<ES: Deref + Clone, CM: Deref + Clone, C: Deref + Clone>
where
	ES::Target: EntropySource,
	CM::Target: AChannelManager,
	C::Target: Filter,
{
	pending_messages: Arc<MessageQueue>,
	pending_events: Arc<EventQueue>,
	request_id_to_method_map: Mutex<HashMap<String, String>>,
	lsps0_client_handler: LSPS0ClientHandler<ES>,
	lsps0_service_handler: Option<LSPS0ServiceHandler>,
	#[cfg(lsps1)]
	lsps1_service_handler: Option<LSPS1ServiceHandler<ES, CM, C>>,
	#[cfg(lsps1)]
	lsps1_client_handler: Option<LSPS1ClientHandler<ES, CM, C>>,
	lsps2_service_handler: Option<LSPS2ServiceHandler<CM>>,
	lsps2_client_handler: Option<LSPS2ClientHandler<ES>>,
	service_config: Option<LiquidityServiceConfig>,
	_client_config: Option<LiquidityClientConfig>,
	best_block: Option<RwLock<BestBlock>>,
	_chain_source: Option<C>,
}

impl<ES: Deref + Clone, CM: Deref + Clone, C: Deref + Clone> LiquidityManager<ES, CM, C>
where
	ES::Target: EntropySource,
	CM::Target: AChannelManager,
	C::Target: Filter,
{
	/// Constructor for the [`LiquidityManager`].
	///
	/// Sets up the required protocol message handlers based on the given
	/// [`LiquidityClientConfig`] and [`LiquidityServiceConfig`].
	pub fn new(
		entropy_source: ES, channel_manager: CM, chain_source: Option<C>,
		chain_params: Option<ChainParameters>, service_config: Option<LiquidityServiceConfig>,
		client_config: Option<LiquidityClientConfig>,
	) -> Self
where {
		let pending_messages = Arc::new(MessageQueue::new());
		let pending_events = Arc::new(EventQueue::new());

		let lsps0_client_handler = LSPS0ClientHandler::new(
			entropy_source.clone(),
			Arc::clone(&pending_messages),
			Arc::clone(&pending_events),
		);

		let lsps0_service_handler = if service_config.is_some() {
			Some(LSPS0ServiceHandler::new(vec![], Arc::clone(&pending_messages)))
		} else {
			None
		};

		let lsps2_client_handler = client_config.as_ref().and_then(|config| {
			config.lsps2_client_config.map(|config| {
				LSPS2ClientHandler::new(
					entropy_source.clone(),
					Arc::clone(&pending_messages),
					Arc::clone(&pending_events),
					config.clone(),
				)
			})
		});
		let lsps2_service_handler = service_config.as_ref().and_then(|config| {
			config.lsps2_service_config.as_ref().map(|config| {
				LSPS2ServiceHandler::new(
					Arc::clone(&pending_messages),
					Arc::clone(&pending_events),
					channel_manager.clone(),
					config.clone(),
				)
			})
		});

		#[cfg(lsps1)]
		let lsps1_client_handler = client_config.as_ref().and_then(|config| {
			config.lsps1_client_config.as_ref().map(|config| {
				LSPS1ClientHandler::new(
					entropy_source.clone(),
					Arc::clone(&pending_messages),
					Arc::clone(&pending_events),
					channel_manager.clone(),
					chain_source.clone(),
					config.clone(),
				)
			})
		});

		#[cfg(lsps1)]
		let lsps1_service_handler = service_config.as_ref().and_then(|config| {
			config.lsps1_service_config.as_ref().map(|config| {
				LSPS1ServiceHandler::new(
					entropy_source.clone(),
					Arc::clone(&pending_messages),
					Arc::clone(&pending_events),
					channel_manager.clone(),
					chain_source.clone(),
					config.clone(),
				)
			})
		});

		Self {
			pending_messages,
			pending_events,
			request_id_to_method_map: Mutex::new(HashMap::new()),
			lsps0_client_handler,
			lsps0_service_handler,
			#[cfg(lsps1)]
			lsps1_client_handler,
			#[cfg(lsps1)]
			lsps1_service_handler,
			lsps2_client_handler,
			lsps2_service_handler,
			service_config,
			_client_config: client_config,
			best_block: chain_params.map(|chain_params| RwLock::new(chain_params.best_block)),
			_chain_source: chain_source,
		}
	}

	/// Returns a reference to the LSPS0 client-side handler.
	pub fn lsps0_client_handler(&self) -> &LSPS0ClientHandler<ES> {
		&self.lsps0_client_handler
	}

	/// Returns a reference to the LSPS0 server-side handler.
	pub fn lsps0_service_handler(&self) -> Option<&LSPS0ServiceHandler> {
		self.lsps0_service_handler.as_ref()
	}

	/// Returns a reference to the LSPS1 client-side handler.
	#[cfg(lsps1)]
	pub fn lsps1_client_handler(&self) -> Option<&LSPS1ClientHandler<ES, CM, C>> {
		self.lsps1_client_handler.as_ref()
	}

	/// Returns a reference to the LSPS1 server-side handler.
	#[cfg(lsps1)]
	pub fn lsps1_service_handler(&self) -> Option<&LSPS1ServiceHandler<ES, CM, C>> {
		self.lsps1_service_handler.as_ref()
	}

	/// Returns a reference to the LSPS2 client-side handler.
	pub fn lsps2_client_handler(&self) -> Option<&LSPS2ClientHandler<ES>> {
		self.lsps2_client_handler.as_ref()
	}

	/// Returns a reference to the LSPS2 server-side handler.
	pub fn lsps2_service_handler(&self) -> Option<&LSPS2ServiceHandler<CM>> {
		self.lsps2_service_handler.as_ref()
	}

	/// Allows to set a callback that will be called after new messages are pushed to the message
	/// queue.
	///
	/// Usually, you'll want to use this to call [`PeerManager::process_events`] to clear the
	/// message queue. For example:
	///
	/// ```
	/// # use lightning::io;
	/// # use lightning_liquidity::LiquidityManager;
	/// # use std::sync::{Arc, RwLock};
	/// # use std::sync::atomic::{AtomicBool, Ordering};
	/// # use std::time::SystemTime;
	/// # struct MyStore {}
	/// # impl lightning::util::persist::KVStore for MyStore {
	/// #     fn read(&self, primary_namespace: &str, secondary_namespace: &str, key: &str) -> io::Result<Vec<u8>> { Ok(Vec::new()) }
	/// #     fn write(&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: &[u8]) -> io::Result<()> { Ok(()) }
	/// #     fn remove(&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool) -> io::Result<()> { Ok(()) }
	/// #     fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> { Ok(Vec::new()) }
	/// # }
	/// # struct MyEntropySource {}
	/// # impl lightning::sign::EntropySource for MyEntropySource {
	/// #     fn get_secure_random_bytes(&self) -> [u8; 32] { [0u8; 32] }
	/// # }
	/// # struct MyEventHandler {}
	/// # impl MyEventHandler {
	/// #     async fn handle_event(&self, _: lightning::events::Event) {}
	/// # }
	/// # #[derive(Eq, PartialEq, Clone, Hash)]
	/// # struct MySocketDescriptor {}
	/// # impl lightning::ln::peer_handler::SocketDescriptor for MySocketDescriptor {
	/// #     fn send_data(&mut self, _data: &[u8], _resume_read: bool) -> usize { 0 }
	/// #     fn disconnect_socket(&mut self) {}
	/// # }
	/// # type MyBroadcaster = dyn lightning::chain::chaininterface::BroadcasterInterface + Send + Sync;
	/// # type MyFeeEstimator = dyn lightning::chain::chaininterface::FeeEstimator + Send + Sync;
	/// # type MyNodeSigner = dyn lightning::sign::NodeSigner + Send + Sync;
	/// # type MyUtxoLookup = dyn lightning::routing::utxo::UtxoLookup + Send + Sync;
	/// # type MyFilter = dyn lightning::chain::Filter + Send + Sync;
	/// # type MyLogger = dyn lightning::util::logger::Logger + Send + Sync;
	/// # type MyChainMonitor = lightning::chain::chainmonitor::ChainMonitor<lightning::sign::InMemorySigner, Arc<MyFilter>, Arc<MyBroadcaster>, Arc<MyFeeEstimator>, Arc<MyLogger>, Arc<MyStore>>;
	/// # type MyPeerManager = lightning::ln::peer_handler::SimpleArcPeerManager<MySocketDescriptor, MyChainMonitor, MyBroadcaster, MyFeeEstimator, Arc<MyUtxoLookup>, MyLogger>;
	/// # type MyNetworkGraph = lightning::routing::gossip::NetworkGraph<Arc<MyLogger>>;
	/// # type MyGossipSync = lightning::routing::gossip::P2PGossipSync<Arc<MyNetworkGraph>, Arc<MyUtxoLookup>, Arc<MyLogger>>;
	/// # type MyChannelManager = lightning::ln::channelmanager::SimpleArcChannelManager<MyChainMonitor, MyBroadcaster, MyFeeEstimator, MyLogger>;
	/// # type MyScorer = RwLock<lightning::routing::scoring::ProbabilisticScorer<Arc<MyNetworkGraph>, Arc<MyLogger>>>;
	/// # type MyLiquidityManager = LiquidityManager<Arc<MyEntropySource>, Arc<MyChannelManager>, Arc<MyFilter>>;
	/// # fn setup_background_processing(my_persister: Arc<MyStore>, my_event_handler: Arc<MyEventHandler>, my_chain_monitor: Arc<MyChainMonitor>, my_channel_manager: Arc<MyChannelManager>, my_logger: Arc<MyLogger>, my_peer_manager: Arc<MyPeerManager>, my_liquidity_manager: Arc<MyLiquidityManager>) {
	/// let process_msgs_pm = Arc::clone(&my_peer_manager);
	/// let process_msgs_callback = move || process_msgs_pm.process_events();
	///
	/// my_liquidity_manager.set_process_msgs_callback(process_msgs_callback);
	/// # }
	/// ```
	///
	/// [`PeerManager::process_events`]: lightning::ln::peer_handler::PeerManager::process_events
	pub fn set_process_msgs_callback(&self, callback: impl Fn() + Send + Sync + 'static) {
		self.pending_messages.set_process_msgs_callback(callback)
	}

	/// Blocks the current thread until next event is ready and returns it.
	///
	/// Typically you would spawn a thread or task that calls this in a loop.
	#[cfg(feature = "std")]
	pub fn wait_next_event(&self) -> Event {
		self.pending_events.wait_next_event()
	}

	/// Returns `Some` if an event is ready.
	///
	/// Typically you would spawn a thread or task that calls this in a loop.
	pub fn next_event(&self) -> Option<Event> {
		self.pending_events.next_event()
	}

	/// Asynchronously polls the event queue and returns once the next event is ready.
	///
	/// Typically you would spawn a thread or task that calls this in a loop.
	pub async fn next_event_async(&self) -> Event {
		self.pending_events.next_event_async().await
	}

	/// Returns and clears all events without blocking.
	///
	/// Typically you would spawn a thread or task that calls this in a loop.
	pub fn get_and_clear_pending_events(&self) -> Vec<Event> {
		self.pending_events.get_and_clear_pending_events()
	}

	fn handle_lsps_message(
		&self, msg: LSPSMessage, sender_node_id: &PublicKey,
	) -> Result<(), lightning::ln::msgs::LightningError> {
		match msg {
			LSPSMessage::Invalid => {
				return Err(LightningError { err: format!("{} did not understand a message we previously sent, maybe they don't support a protocol we are trying to use?", sender_node_id), action: ErrorAction::IgnoreAndLog(Level::Error)});
			}
			LSPSMessage::LSPS0(msg @ LSPS0Message::Response(..)) => {
				self.lsps0_client_handler.handle_message(msg, sender_node_id)?;
			}
			LSPSMessage::LSPS0(msg @ LSPS0Message::Request(..)) => {
				match &self.lsps0_service_handler {
					Some(lsps0_service_handler) => {
						lsps0_service_handler.handle_message(msg, sender_node_id)?;
					}
					None => {
						return Err(LightningError { err: format!("Received LSPS0 request message without LSPS0 service handler configured. From node = {:?}", sender_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)});
					}
				}
			}
			#[cfg(lsps1)]
			LSPSMessage::LSPS1(msg @ LSPS1Message::Response(..)) => match &self.lsps1_client_handler {
				Some(lsps1_client_handler) => {
					lsps1_client_handler.handle_message(msg, sender_node_id)?;
				}
				None => {
					return Err(LightningError { err: format!("Received LSPS1 response message without LSPS1 client handler configured. From node = {:?}", sender_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)});
				}
			},
			#[cfg(lsps1)]
			LSPSMessage::LSPS1(msg @ LSPS1Message::Request(..)) => match &self.lsps1_service_handler {
				Some(lsps1_service_handler) => {
					lsps1_service_handler.handle_message(msg, sender_node_id)?;
				}
				None => {
					return Err(LightningError { err: format!("Received LSPS1 request message without LSPS1 service handler configured. From node = {:?}", sender_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)});
				}
			},
			LSPSMessage::LSPS2(msg @ LSPS2Message::Response(..)) => {
				match &self.lsps2_client_handler {
					Some(lsps2_client_handler) => {
						lsps2_client_handler.handle_message(msg, sender_node_id)?;
					}
					None => {
						return Err(LightningError { err: format!("Received LSPS2 response message without LSPS2 client handler configured. From node = {:?}", sender_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)});
					}
				}
			}
			LSPSMessage::LSPS2(msg @ LSPS2Message::Request(..)) => {
				match &self.lsps2_service_handler {
					Some(lsps2_service_handler) => {
						lsps2_service_handler.handle_message(msg, sender_node_id)?;
					}
					None => {
						return Err(LightningError { err: format!("Received LSPS2 request message without LSPS2 service handler configured. From node = {:?}", sender_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)});
					}
				}
			}
		}
		Ok(())
	}
}

impl<ES: Deref + Clone + Clone, CM: Deref + Clone, C: Deref + Clone> CustomMessageReader
	for LiquidityManager<ES, CM, C>
where
	ES::Target: EntropySource,
	CM::Target: AChannelManager,
	C::Target: Filter,
{
	type CustomMessage = RawLSPSMessage;

	fn read<RD: lightning::io::Read>(
		&self, message_type: u16, buffer: &mut RD,
	) -> Result<Option<Self::CustomMessage>, lightning::ln::msgs::DecodeError> {
		match message_type {
			LSPS_MESSAGE_TYPE_ID => Ok(Some(RawLSPSMessage::read(buffer)?)),
			_ => Ok(None),
		}
	}
}

impl<ES: Deref + Clone, CM: Deref + Clone, C: Deref + Clone> CustomMessageHandler
	for LiquidityManager<ES, CM, C>
where
	ES::Target: EntropySource,
	CM::Target: AChannelManager,
	C::Target: Filter,
{
	fn handle_custom_message(
		&self, msg: Self::CustomMessage, sender_node_id: &PublicKey,
	) -> Result<(), lightning::ln::msgs::LightningError> {
		let message = {
			let mut request_id_to_method_map = self.request_id_to_method_map.lock().unwrap();
			LSPSMessage::from_str_with_id_map(&msg.payload, &mut request_id_to_method_map)
		};

		match message {
			Ok(msg) => self.handle_lsps_message(msg, sender_node_id),
			Err(_) => {
				self.pending_messages.enqueue(sender_node_id, LSPSMessage::Invalid);
				Ok(())
			}
		}
	}

	fn get_and_clear_pending_msg(&self) -> Vec<(PublicKey, Self::CustomMessage)> {
		let mut request_id_to_method_map = self.request_id_to_method_map.lock().unwrap();
		self.pending_messages
			.get_and_clear_pending_msgs()
			.iter()
			.map(|(public_key, lsps_message)| {
				if let Some((request_id, method_name)) = lsps_message.get_request_id_and_method() {
					request_id_to_method_map.insert(request_id, method_name);
				}
				(
					*public_key,
					RawLSPSMessage { payload: serde_json::to_string(&lsps_message).unwrap() },
				)
			})
			.collect()
	}

	fn provided_node_features(&self) -> NodeFeatures {
		let mut features = NodeFeatures::empty();

		if self.service_config.is_some() {
			features.set_optional_custom_bit(LSPS_FEATURE_BIT).unwrap();
		}

		features
	}

	fn provided_init_features(&self, _their_node_id: &PublicKey) -> InitFeatures {
		let mut features = InitFeatures::empty();

		if self.service_config.is_some() {
			features.set_optional_custom_bit(LSPS_FEATURE_BIT).unwrap();
		}

		features
	}
}

impl<ES: Deref + Clone, CM: Deref + Clone, C: Deref + Clone> Listen for LiquidityManager<ES, CM, C>
where
	ES::Target: EntropySource,
	CM::Target: AChannelManager,
	C::Target: Filter,
{
	fn filtered_block_connected(
		&self, header: &bitcoin::block::Header, txdata: &chain::transaction::TransactionData,
		height: u32,
	) {
		if let Some(best_block) = &self.best_block {
			let best_block = best_block.read().unwrap();
			assert_eq!(best_block.block_hash(), header.prev_blockhash,
			"Blocks must be connected in chain-order - the connected header must build on the last connected header");
			assert_eq!(best_block.height(), height - 1,
			"Blocks must be connected in chain-order - the connected block height must be one greater than the previous height");
		}

		self.transactions_confirmed(header, txdata, height);
		self.best_block_updated(header, height);
	}

	fn block_disconnected(&self, header: &bitcoin::block::Header, height: u32) {
		let new_height = height - 1;
		if let Some(best_block) = &self.best_block {
			let mut best_block = best_block.write().unwrap();
			assert_eq!(best_block.block_hash(), header.block_hash(),
				"Blocks must be disconnected in chain-order - the disconnected header must be the last connected header");
			assert_eq!(best_block.height(), height,
				"Blocks must be disconnected in chain-order - the disconnected block must have the correct height");
			*best_block = BestBlock::new(header.prev_blockhash, new_height)
		}

		// TODO: Call block_disconnected on all sub-modules that require it, e.g., LSPS1MessageHandler.
		// Internally this should call transaction_unconfirmed for all transactions that were
		// confirmed at a height <= the one we now disconnected.
	}
}

impl<ES: Deref + Clone, CM: Deref + Clone, C: Deref + Clone> Confirm for LiquidityManager<ES, CM, C>
where
	ES::Target: EntropySource,
	CM::Target: AChannelManager,
	C::Target: Filter,
{
	fn transactions_confirmed(
		&self, _header: &bitcoin::block::Header, _txdata: &chain::transaction::TransactionData,
		_height: u32,
	) {
		// TODO: Call transactions_confirmed on all sub-modules that require it, e.g., LSPS1MessageHandler.
	}

	fn transaction_unconfirmed(&self, _txid: &bitcoin::Txid) {
		// TODO: Call transaction_unconfirmed on all sub-modules that require it, e.g., LSPS1MessageHandler.
		// Internally this should call transaction_unconfirmed for all transactions that were
		// confirmed at a height <= the one we now unconfirmed.
	}

	fn best_block_updated(&self, _header: &bitcoin::block::Header, _height: u32) {
		// TODO: Call best_block_updated on all sub-modules that require it, e.g., LSPS1MessageHandler.
	}

	fn get_relevant_txids(&self) -> Vec<(bitcoin::Txid, u32, Option<bitcoin::BlockHash>)> {
		// TODO: Collect relevant txids from all sub-modules that, e.g., LSPS1MessageHandler.
		Vec::new()
	}
}
