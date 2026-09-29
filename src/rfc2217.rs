//! RFC 2217, the Telnet Com Port Control Option, for the console's second port: telnet
//! framing around the camera's bytes, and the COM-PORT-OPTION requests pyserial's
//! `rfc2217://` sends. A setting is acknowledged with the value then in force, which is how
//! pyserial tells an accepted value from a refused one.

pub const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

const BINARY: u8 = 0;
const ECHO: u8 = 1;
const SGA: u8 = 3;
const COM_PORT: u8 = 44;

// COM-PORT-OPTION requests; the answer to each is its number plus 100.
const SIGNATURE: u8 = 0;
const SET_BAUDRATE: u8 = 1;
const SET_DATASIZE: u8 = 2;
const SET_PARITY: u8 = 3;
const SET_STOPSIZE: u8 = 4;
const SET_CONTROL: u8 = 5;
const NOTIFY_MODEMSTATE: u8 = 7;
const SET_LINESTATE_MASK: u8 = 10;
const SET_MODEMSTATE_MASK: u8 = 11;
const PURGE_DATA: u8 = 12;
const ANSWER: u8 = 100;

// SET-CONTROL values.
const FLOW_ASK: u8 = 0;
const FLOW_NONE: u8 = 1;
const BREAK_ASK: u8 = 4;
const BREAK_ON: u8 = 5;
const BREAK_OFF: u8 = 6;
const DTR_ASK: u8 = 7;
const DTR_ON: u8 = 8;
const DTR_OFF: u8 = 9;
const RTS_ASK: u8 = 10;
const RTS_ON: u8 = 11;
const RTS_OFF: u8 = 12;
const INBOUND_FLOW_ASK: u8 = 13;
const INBOUND_FLOW_NONE: u8 = 14;

/// DSR and CTS, both reported asserted: the camera has no modem lines.
const MODEM_STATE: u8 = 0x30;
const PURGE_RECEIVE: u8 = 1;
const PURGE_BOTH: u8 = 3;

/// What the COM-PORT-OPTION requests act on. Values are RFC 2217's: parity 1 none, 2 odd,
/// 3 even; stop bits 1, 2, or 3 for one and a half.
pub trait Port {
    /// The rate last set, not the UART's read-back, which the clock divider rounds.
    fn baudrate(&self) -> u32;
    fn set_baudrate(&mut self, baud: u32);
    fn datasize(&self) -> u8;
    fn set_datasize(&mut self, bits: u8);
    fn parity(&self) -> u8;
    fn set_parity(&mut self, parity: u8);
    fn stopsize(&self) -> u8;
    fn set_stopsize(&mut self, stop: u8);
    fn set_break(&mut self, on: bool);
    fn set_lines(&mut self, dtr: bool, rts: bool);
    fn purge_input(&mut self);
}

#[derive(Clone, Copy)]
enum Parse {
    Data,
    Iac,
    Option(u8),
    Sub,
    SubIac,
}

/// One client's telnet session.
pub struct Session {
    parse: Parse,
    sub: [u8; 12],
    sub_len: usize,
    /// A subnegotiation longer than any this answers, skipped to its end.
    sub_overflow: bool,
    /// Options this end performs, and ones it agreed the client performs, as bits.
    ours: u64,
    theirs: u64,
    /// A client not in binary mode sends a bare CR as CR NUL.
    after_cr: bool,
    dtr: bool,
    rts: bool,
    breaking: bool,
    /// Clients learn the modem lines only from the server's notices, and pyserial refuses
    /// to report CTS or DSR before the first one.
    modem_notified: bool,
    /// The port's settings were changed, so they go back to the defaults at the end.
    pub changed: bool,
}

const fn bit(option: u8) -> u64 {
    1 << option
}

fn supported_ours(option: u8) -> bool {
    matches!(option, BINARY | ECHO | SGA | COM_PORT)
}

fn supported_theirs(option: u8) -> bool {
    matches!(option, BINARY | SGA | COM_PORT)
}

impl Session {
    /// A session, and the offers that put a plain telnet client into character mode with
    /// the camera doing the echoing.
    pub fn open(reply: &mut Vec<u8>) -> Self {
        reply.extend_from_slice(&[IAC, WILL, ECHO, IAC, WILL, SGA, IAC, WILL, BINARY, IAC, DO, BINARY]);
        Self {
            parse: Parse::Data,
            sub: [0; 12],
            sub_len: 0,
            sub_overflow: false,
            ours: bit(ECHO) | bit(SGA) | bit(BINARY),
            theirs: bit(BINARY),
            after_cr: false,
            // What terminals assert when they open, so opening one changes nothing.
            dtr: true,
            rts: true,
            breaking: false,
            modem_notified: false,
            changed: false,
        }
    }

    /// Splits what the client sent into bytes for the camera and answers for the client.
    pub fn input(&mut self, bytes: &[u8], data: &mut Vec<u8>, reply: &mut Vec<u8>, port: &mut impl Port) {
        for &byte in bytes {
            self.parse = match self.parse {
                Parse::Data => {
                    if byte == IAC {
                        Parse::Iac
                    } else {
                        // CR NUL from a client outside binary mode is a bare CR.
                        if !(byte == 0 && self.after_cr && self.theirs & bit(BINARY) == 0) {
                            data.push(byte);
                        }
                        self.after_cr = byte == b'\r';
                        Parse::Data
                    }
                }
                Parse::Iac => match byte {
                    IAC => {
                        data.push(IAC);
                        self.after_cr = false;
                        Parse::Data
                    }
                    WILL | WONT | DO | DONT => Parse::Option(byte),
                    SB => {
                        self.sub_len = 0;
                        self.sub_overflow = false;
                        Parse::Sub
                    }
                    // NOP, go-ahead, and the rest: nothing to do.
                    _ => Parse::Data,
                },
                Parse::Option(command) => {
                    self.negotiate(command, byte, reply);
                    Parse::Data
                }
                Parse::Sub => {
                    if byte == IAC {
                        Parse::SubIac
                    } else {
                        self.sub_push(byte);
                        Parse::Sub
                    }
                }
                Parse::SubIac => match byte {
                    SE => {
                        if !self.sub_overflow {
                            self.subnegotiation(reply, port);
                        }
                        Parse::Data
                    }
                    IAC => {
                        self.sub_push(IAC);
                        Parse::Sub
                    }
                    _ => Parse::Data,
                },
            };
        }
    }

    fn sub_push(&mut self, byte: u8) {
        if self.sub_len < self.sub.len() {
            self.sub[self.sub_len] = byte;
            self.sub_len += 1;
        } else {
            self.sub_overflow = true;
        }
    }

    /// Agrees to what it supports and refuses the rest, answering only a change of state
    /// so that neither end answers the other's answer.
    fn negotiate(&mut self, command: u8, option: u8, reply: &mut Vec<u8>) {
        let mask = if option < 64 { bit(option) } else { 0 };
        match command {
            DO if mask != 0 && supported_ours(option) => {
                if self.ours & mask == 0 {
                    self.ours |= mask;
                    reply.extend_from_slice(&[IAC, WILL, option]);
                }
                if option == COM_PORT {
                    self.notify_modem_state(reply);
                }
            }
            DO => reply.extend_from_slice(&[IAC, WONT, option]),
            DONT if self.ours & mask != 0 => {
                self.ours &= !mask;
                reply.extend_from_slice(&[IAC, WONT, option]);
            }
            WILL if mask != 0 && supported_theirs(option) => {
                if self.theirs & mask == 0 {
                    self.theirs |= mask;
                    reply.extend_from_slice(&[IAC, DO, option]);
                }
                if option == COM_PORT {
                    self.notify_modem_state(reply);
                }
            }
            WILL => reply.extend_from_slice(&[IAC, DONT, option]),
            WONT if self.theirs & mask != 0 => {
                self.theirs &= !mask;
                reply.extend_from_slice(&[IAC, DONT, option]);
            }
            _ => {}
        }
    }

    fn notify_modem_state(&mut self, reply: &mut Vec<u8>) {
        if !self.modem_notified {
            self.modem_notified = true;
            answer(reply, NOTIFY_MODEMSTATE, &[MODEM_STATE]);
        }
    }

    fn subnegotiation(&mut self, reply: &mut Vec<u8>, port: &mut impl Port) {
        let sub = &self.sub[..self.sub_len];
        let [COM_PORT, command, value @ ..] = sub else {
            return;
        };
        let (command, value) = (*command, value.to_vec());
        let byte = value.first().copied().unwrap_or(0);
        match command {
            SIGNATURE if value.is_empty() => answer(reply, command, b"thingino-backpack"),
            SET_BAUDRATE => {
                if let Ok(baud) = <[u8; 4]>::try_from(value.as_slice()).map(u32::from_be_bytes) {
                    if baud != 0 {
                        port.set_baudrate(baud);
                        self.changed = true;
                    }
                }
                answer(reply, command, &port.baudrate().to_be_bytes());
            }
            SET_DATASIZE => {
                if (5..=8).contains(&byte) {
                    port.set_datasize(byte);
                    self.changed = true;
                }
                answer(reply, command, &[port.datasize()]);
            }
            SET_PARITY => {
                if (1..=3).contains(&byte) {
                    port.set_parity(byte);
                    self.changed = true;
                }
                answer(reply, command, &[port.parity()]);
            }
            SET_STOPSIZE => {
                if (1..=3).contains(&byte) {
                    port.set_stopsize(byte);
                    self.changed = true;
                }
                answer(reply, command, &[port.stopsize()]);
            }
            SET_CONTROL => {
                let state = self.control(byte, port);
                answer(reply, command, &[state]);
            }
            NOTIFY_MODEMSTATE => answer(reply, command, &[MODEM_STATE]),
            SET_LINESTATE_MASK | SET_MODEMSTATE_MASK => answer(reply, command, &[byte]),
            PURGE_DATA => {
                if byte == PURGE_RECEIVE || byte == PURGE_BOTH {
                    port.purge_input();
                }
                answer(reply, command, &[byte]);
            }
            // Flow-control suspend and resume, a client's own signature, and notices that
            // only go the other way.
            _ => {}
        }
    }

    /// SET-CONTROL: answers the state now in force, which is the requested value when the
    /// request was honoured.
    fn control(&mut self, value: u8, port: &mut impl Port) -> u8 {
        match value {
            FLOW_ASK | FLOW_NONE => FLOW_NONE,
            BREAK_ASK => {
                if self.breaking {
                    BREAK_ON
                } else {
                    BREAK_OFF
                }
            }
            BREAK_ON | BREAK_OFF => {
                self.breaking = value == BREAK_ON;
                port.set_break(self.breaking);
                value
            }
            DTR_ASK => {
                if self.dtr {
                    DTR_ON
                } else {
                    DTR_OFF
                }
            }
            DTR_ON | DTR_OFF => {
                self.dtr = value == DTR_ON;
                port.set_lines(self.dtr, self.rts);
                value
            }
            RTS_ASK => {
                if self.rts {
                    RTS_ON
                } else {
                    RTS_OFF
                }
            }
            RTS_ON | RTS_OFF => {
                self.rts = value == RTS_ON;
                port.set_lines(self.dtr, self.rts);
                value
            }
            INBOUND_FLOW_ASK => INBOUND_FLOW_NONE,
            // Software and hardware flow control, inbound or not: there is no flow control
            // on this UART, and answering what is in force tells the client so.
            _ => FLOW_NONE,
        }
    }
}

fn answer(reply: &mut Vec<u8>, command: u8, value: &[u8]) {
    reply.extend_from_slice(&[IAC, SB, COM_PORT, command + ANSWER]);
    for &byte in value {
        reply.push(byte);
        if byte == IAC {
            reply.push(IAC);
        }
    }
    reply.extend_from_slice(&[IAC, SE]);
}
