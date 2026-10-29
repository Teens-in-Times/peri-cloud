//! Optional message adapters feed one account/session router. No tool policy or
//! reasoning loop lives in the adapter; structured interactions use Peri's broker.

pub mod access;
mod broker;
mod plain;
pub(crate) use plain::plain_chat;
mod connector;
mod context_files;
pub(crate) mod fleet;
pub(crate) use connector::inspect_client;
pub use fleet::{
    Bookmark, BookmarkCommand, CheckView, ConnectionLease, DeviceInspection, DeviceManagement,
    DeviceWork,
};
mod logical_conversations;
mod normalize;
pub use normalize::{MessageNormalizer, NormalizedMessage};
mod menu_store;
pub(crate) mod menu_view;
mod router;
pub(crate) mod store;
mod topology;
mod types;
mod web_chat;
mod web_entry;
pub use topology::GatewayView;
pub use web_chat::{WebChat, WebDevice, WebMessage, WebSendResult};
pub use web_entry::WebAdapter;
pub(crate) use web_entry::WEB_ADAPTER;

pub use connector::{DeviceReachability, NativeSessionConnector, SessionConnector};
pub use router::{
    ActivityPhase, CommandReceipt, ConversationActivity, Gateway, SavedResultPage, TaskReceipt,
    WorkspaceSelection,
};
pub use types::{
    ChannelRoute, Delivery, DeliveryBody, DeliveryError, GatewayReceipt, InboundMessage,
    InteractionAction, InteractionCard, MenuButton, MenuCard, MessageAdapter, ReceiptState,
    ReplyKind,
};

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("private or legacy conversation history cannot enter a shared conversation")]
    PrivateHistory,
    #[error(transparent)]
    Memory(#[from] crate::memory::MemoryError),
    #[error(transparent)]
    Schedule(#[from] crate::scheduler::ScheduleError),
    #[error(transparent)]
    Identity(#[from] crate::identity::IdentityError),
    #[error(transparent)]
    State(#[from] crate::state::StateError),
    #[error(transparent)]
    Runtime(#[from] crate::RuntimeError),
    #[error("gateway state is unavailable")]
    Database(#[from] sqlx::Error),
    #[error("gateway record is invalid")]
    Encoding(#[from] serde_json::Error),
    #[error("gateway input is invalid")]
    Invalid,
    #[error("gateway message identity was reused with different content")]
    MessageConflict,
    #[error("workspace bookmark was changed, removed or belongs to another device")]
    BookmarkConflict,
    #[error("device workspace bookmark limit reached")]
    BookmarkLimit,
    #[error("gateway has active or unconfirmed work")]
    Busy,
    #[error("gateway is closing")]
    Closing,
    #[error("device connection is unavailable or has a different identity")]
    Connection,
    #[error("device has no deployment-owned transport configuration")]
    NotConfigured,
    #[error("interaction is expired or no longer belongs to this session")]
    StaleInteraction,
    #[error("gateway owned operation requires recovery")]
    RecoveryRequired,
}

pub type GatewayResult<T> = Result<T, GatewayError>;
