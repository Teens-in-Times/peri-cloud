//! Optional message adapters feed one account/session router. No tool policy or
//! reasoning loop lives in the adapter; structured interactions use Peri's broker.

pub mod access;
mod broker;
mod connector;
mod menu_store;
pub(crate) mod menu_view;
mod router;
pub(crate) mod store;
mod topology;
mod types;
pub use topology::GatewayView;

pub use connector::{NativeSessionConnector, SessionConnector};
pub use router::{ConversationActivity, Gateway, SavedResultPage, WorkspaceSelection};
pub use types::{
    ChannelRoute, Delivery, DeliveryBody, DeliveryError, GatewayReceipt, InboundMessage,
    InteractionAction, InteractionCard, MenuButton, MenuCard, MessageAdapter, ReceiptState,
    ReplyKind,
};

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
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
    #[error("gateway has active or unconfirmed work")]
    Busy,
    #[error("gateway is closing")]
    Closing,
    #[error("device connection is unavailable or has a different identity")]
    Connection,
    #[error("interaction is expired or no longer belongs to this session")]
    StaleInteraction,
    #[error("gateway owned operation requires recovery")]
    RecoveryRequired,
}

pub type GatewayResult<T> = Result<T, GatewayError>;
