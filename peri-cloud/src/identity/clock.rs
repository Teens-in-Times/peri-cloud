/// Account expiry uses one deployment time source. Embeddings may supply a
/// controlled clock so expiry/replay scenarios can be verified without sleeps.
pub trait IdentityClock: Send + Sync {
    fn unix_seconds(&self) -> i64;
}

pub(crate) struct WallClock;

impl IdentityClock for WallClock {
    fn unix_seconds(&self) -> i64 {
        super::crypto::now()
    }
}
