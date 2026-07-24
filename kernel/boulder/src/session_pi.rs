// kernel/boulder/src/session_pi.rs
//! Sessionπ — runtime session types for kernel IPC
//!
//! Syntax (encoded as bytecode, not strings):
//!   END, SEND(tag), RECV(tag), OFFER(n), CHOICE(n), REC(label), VAR(label)
//!
//! dual(SEND t; S)  = RECV t; dual(S)
//! dual(OFFER as)   = CHOICE dual(as)
//! dual(REC S)      = REC dual(S)
//!
//! Progress: every transition consumes one protocol step (linear).
//! Alien bit: endpoints carry a proof index into a dual pair table;
//! wormhole enqueue refuses payloads that don't match the expected tag.


pub const MAX_PROTO_OPS: usize = 32;
pub const MAX_SESSIONS: usize = 64;
pub const MAX_CHOICE: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Op {
    End = 0,
    Send = 1,
    Recv = 2,
    /// External choice: peer picks branch 0..n-1; ops follow as groups
    Offer = 3,
    /// Internal choice: we pick branch
    Choice = 4,
    Rec = 5,
    Var = 6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtoOp {
    pub op: Op,
    /// message tag or branch count or rec label
    pub meta: u16,
}

impl ProtoOp {
    pub const END: Self = Self {
        op: Op::End,
        meta: 0,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct Protocol {
    pub ops: [ProtoOp; MAX_PROTO_OPS],
    pub len: u8,
}

impl Protocol {
    pub const fn empty() -> Self {
        Self {
            ops: [ProtoOp::END; MAX_PROTO_OPS],
            len: 0,
        }
    }

    pub fn dual(self) -> Self {
        let mut out = Self::empty();
        out.len = self.len;
        for i in 0..self.len as usize {
            let o = self.ops[i];
            out.ops[i] = match o.op {
                Op::Send => ProtoOp {
                    op: Op::Recv,
                    meta: o.meta,
                },
                Op::Recv => ProtoOp {
                    op: Op::Send,
                    meta: o.meta,
                },
                Op::Offer => ProtoOp {
                    op: Op::Choice,
                    meta: o.meta,
                },
                Op::Choice => ProtoOp {
                    op: Op::Offer,
                    meta: o.meta,
                },
                other => ProtoOp {
                    op: other,
                    meta: o.meta,
                },
            };
        }
        out
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessFault {
    Dead,
    TagMismatch { expected: u16, got: u16 },
    NotSendState,
    NotRecvState,
    NotChoiceState,
    NotOfferState,
    Ended,
    Capacity,
    BranchOob,
}

#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    pub live: bool,
    pub id: u32,
    pub peer: u32,
    pub proto: Protocol,
    pub pc: u8,
    pub owner_pid: u32,
}

impl Endpoint {
    pub const EMPTY: Self = Self {
        live: false,
        id: 0,
        peer: 0,
        proto: Protocol::empty(),
        pc: 0,
        owner_pid: 0,
    };

    fn op(&self) -> ProtoOp {
        if self.pc as usize >= self.proto.len as usize {
            return ProtoOp::END;
        }
        self.proto.ops[self.pc as usize]
    }
}

pub struct SessionTable {
    eps: [Endpoint; MAX_SESSIONS],
    len: usize,
    next_id: u32,
}

impl SessionTable {
    pub const fn new() -> Self {
        Self {
            eps: [Endpoint::EMPTY; MAX_SESSIONS],
            len: 0,
            next_id: 1,
        }
    }

    /// Open a dual pair. Returns (client_ep, server_ep) ids.
    pub fn open_dual(
        &mut self,
        client_pid: u32,
        server_pid: u32,
        client_proto: Protocol,
    ) -> Result<(u32, u32), SessFault> {
        if self.len + 2 > MAX_SESSIONS {
            return Err(SessFault::Capacity);
        }
        let c_id = self.next_id;
        let s_id = self.next_id.wrapping_add(1);
        self.next_id = self.next_id.wrapping_add(2);

        self.eps[self.len] = Endpoint {
            live: true,
            id: c_id,
            peer: s_id,
            proto: client_proto,
            pc: 0,
            owner_pid: client_pid,
        };
        self.len += 1;
        self.eps[self.len] = Endpoint {
            live: true,
            id: s_id,
            peer: c_id,
            proto: client_proto.dual(),
            pc: 0,
            owner_pid: server_pid,
        };
        self.len += 1;
        Ok((c_id, s_id))
    }

    fn idx(&self, id: u32) -> Option<usize> {
        self.eps
            .iter()
            .take(self.len)
            .position(|e| e.live && e.id == id)
    }

    /// Linear SEND: must be in Send state with matching tag, then advance both sides.
    pub fn send(&mut self, id: u32, tag: u16) -> Result<(), SessFault> {
        let i = self.idx(id).ok_or(SessFault::Dead)?;
        let op = self.eps[i].op();
        if op.op == Op::End {
            return Err(SessFault::Ended);
        }
        if op.op != Op::Send {
            return Err(SessFault::NotSendState);
        }
        if op.meta != tag {
            return Err(SessFault::TagMismatch {
                expected: op.meta,
                got: tag,
            });
        }
        let peer = self.eps[i].peer;
        let j = self.idx(peer).ok_or(SessFault::Dead)?;
        let pop = self.eps[j].op();
        if pop.op != Op::Recv || pop.meta != tag {
            return Err(SessFault::TagMismatch {
                expected: pop.meta,
                got: tag,
            });
        }
        self.eps[i].pc = self.eps[i].pc.saturating_add(1);
        self.eps[j].pc = self.eps[j].pc.saturating_add(1);
        Ok(())
    }

    pub fn recv(&mut self, id: u32, tag: u16) -> Result<(), SessFault> {
        // recv is dual of send from the other side — same machine
        let i = self.idx(id).ok_or(SessFault::Dead)?;
        let op = self.eps[i].op();
        if op.op != Op::Recv {
            return Err(SessFault::NotRecvState);
        }
        if op.meta != tag {
            return Err(SessFault::TagMismatch {
                expected: op.meta,
                got: tag,
            });
        }
        // peer must be in Send — advance both via send on peer
        let peer = self.eps[i].peer;
        self.send(peer, tag)
    }

    pub fn choose(&mut self, id: u32, branch: u16) -> Result<(), SessFault> {
        let i = self.idx(id).ok_or(SessFault::Dead)?;
        let op = self.eps[i].op();
        if op.op != Op::Choice {
            return Err(SessFault::NotChoiceState);
        }
        if branch >= op.meta {
            return Err(SessFault::BranchOob);
        }
        let peer = self.eps[i].peer;
        let j = self.idx(peer).ok_or(SessFault::Dead)?;
        if self.eps[j].op().op != Op::Offer {
            return Err(SessFault::NotOfferState);
        }
        // Encoding convention: pc points at CHOICE/OFFER; next ops are
        // branch bodies laid out sequentially with END separators.
        // For v1: pc := pc+1+branch (simple tag-style branches).
        let adv = 1 + branch as u8;
        self.eps[i].pc = self.eps[i].pc.saturating_add(adv);
        self.eps[j].pc = self.eps[j].pc.saturating_add(adv);
        Ok(())
    }

    pub fn close(&mut self, id: u32) -> Result<(), SessFault> {
        let i = self.idx(id).ok_or(SessFault::Dead)?;
        if self.eps[i].op().op != Op::End {
            return Err(SessFault::NotSendState);
        }
        let peer = self.eps[i].peer;
        self.eps[i].live = false;
        if let Some(j) = self.idx(peer) {
            self.eps[j].live = false;
        }
        Ok(())
    }
}

/// Example: crest compositor handshake
/// client: !Auth.?Ok.!Frame.end
pub fn proto_crest_handshake() -> Protocol {
    let mut p = Protocol::empty();
    p.ops[0] = ProtoOp {
        op: Op::Send,
        meta: 1,
    }; // Auth
    p.ops[1] = ProtoOp {
        op: Op::Recv,
        meta: 2,
    }; // Ok
    p.ops[2] = ProtoOp {
        op: Op::Send,
        meta: 3,
    }; // Frame
    p.ops[3] = ProtoOp::END;
    p.len = 4;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_handshake_progresses() {
        let mut t = SessionTable::new();
        let (c, s) = t.open_dual(10, 20, proto_crest_handshake()).unwrap();
        t.send(c, 1).unwrap();
        t.send(s, 2).unwrap(); // server sends Ok (client was in Recv Ok)
        t.send(c, 3).unwrap();
        t.close(c).unwrap();
    }

    #[test]
    fn wrong_tag_dies() {
        let mut t = SessionTable::new();
        let (c, _) = t.open_dual(1, 2, proto_crest_handshake()).unwrap();
        assert!(matches!(t.send(c, 99), Err(SessFault::TagMismatch { .. })));
    }
}
