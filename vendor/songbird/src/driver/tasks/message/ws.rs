#![allow(missing_docs)]

use super::Interconnect;
use crate::{model::Event as GatewayEvent, ws::WsStream};

pub enum WsMessage {
    Ws(Box<WsStream>),
    ReplaceInterconnect(Interconnect),
    SetKeepalive(f64),
    Speaking(bool),
    Deliver(GatewayEvent),
    /// tinycord patch: send this JSON text as-is.
    Raw(String),
}
