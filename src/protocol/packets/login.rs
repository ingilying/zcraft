use crate::protocol::{Encode, put_string};
use std::io;

#[derive(Debug, PartialEq, Eq)]
pub struct LoginDisconnect {
    pub reason: String,
}

impl Encode for LoginDisconnect {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_string(&self.reason, out)
    }
}
