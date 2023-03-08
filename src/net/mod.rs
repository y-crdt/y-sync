mod broadcast;
mod conn;

pub type ConnHandler<I, O> = conn::Connection<I, O>;
pub type BroadcastGroup = broadcast::BroadcastGroup;
