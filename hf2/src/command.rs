use crate::mock::HidMockable;
use core::convert::TryFrom;
use log;
use scroll::{ctx, Pread, Pwrite, LE};

pub fn send<'a, C, RES>(command: &C, d: &hidapi::HidDevice) -> Result<RES, Error>
where
    C: Commander<'a, RES>,
    RES: scroll::ctx::TryFromCtx<'a, scroll::Endian>,
{
    command.send(d)
}

pub trait Commander<'a, RES: scroll::ctx::TryFromCtx<'a, scroll::Endian>> {
    const ID: u32;

    fn send(&self, d: &hidapi::HidDevice) -> Result<RES, Error>;
}

pub struct NoResponse {}

//todo, don't
impl<'a> ctx::TryFromCtx<'a, scroll::Endian> for NoResponse {
    type Error = Error;
    fn try_from_ctx(_this: &'a [u8], _le: scroll::Endian) -> Result<(Self, usize), Self::Error> {
        Ok((NoResponse {}, 0))
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct CommandResponse {
    ///arbitrary number set by the host, for example as sequence number. The response should repeat the tag.
    pub(crate) tag: u16,
    pub(crate) status: CommandResponseStatus, //    uint8_t status;
    ///additional information In case of non-zero status
    pub(crate) status_info: u8, // optional?
    ///LE bytes
    pub(crate) data: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum CommandResponseStatus {
    //command understood and executed correctly
    Success = 0x00,
    //command not understood
    ParseError = 0x01,
    //command execution error
    ExecutionError = 0x02,
}

impl TryFrom<u8> for CommandResponseStatus {
    type Error = Error;

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0 => Ok(CommandResponseStatus::Success),
            1 => Ok(CommandResponseStatus::ParseError),
            2 => Ok(CommandResponseStatus::ExecutionError),
            _ => Err(Error::Parse),
        }
    }
}

#[derive(Debug, PartialEq)]
enum PacketType {
    //Inner packet of a command message
    Inner = 0,
    //Final packet of a command message
    Final = 1,
    //Serial stdout
    StdOut = 2,
    //Serial stderr
    Stderr = 3,
}

impl TryFrom<u8> for PacketType {
    type Error = Error;

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0 => Ok(PacketType::Inner),
            1 => Ok(PacketType::Final),
            2 => Ok(PacketType::StdOut),
            3 => Ok(PacketType::Stderr),
            _ => Err(Error::Parse),
        }
    }
}

// doesnt know what the data is supposed to be decoded as
// thats linked via the seq number outside, so we cant decode here
impl<'a> ctx::TryFromCtx<'a, scroll::Endian> for CommandResponse {
    type Error = Error;
    fn try_from_ctx(this: &'a [u8], le: scroll::Endian) -> Result<(Self, usize), Self::Error> {
        if this.len() < 4 {
            return Err(Error::Parse);
        }

        let mut offset = 0;
        let tag = this.gread_with::<u16>(&mut offset, le)?;
        let status: u8 = this.gread_with::<u8>(&mut offset, le)?;
        let status = CommandResponseStatus::try_from(status)?;
        let status_info = this.gread_with::<u8>(&mut offset, le)?;

        Ok((
            CommandResponse {
                tag,
                status,
                status_info,
                data: this[offset..].to_vec(),
            },
            offset,
        ))
    }
}

#[derive(Debug)]
pub(crate) struct Command {
    ///Command ID
    id: u32,
    ///arbitrary number set by the host, for example as sequence number. The response should repeat the tag.
    tag: u16,
    ///reserved bytes in the command should be sent as zero and ignored by the device
    _reserved0: u8,
    ///reserved bytes in the command should be sent as zero and ignored by the device
    _reserved1: u8,
    ///LE bytes
    data: Vec<u8>,
}
impl Command {
    pub(crate) fn new(id: u32, tag: u16, data: Vec<u8>) -> Self {
        Self {
            id,
            tag,
            _reserved0: 0,
            _reserved1: 0,
            data,
        }
    }
}

///Transmit a Command, command.data should already have been LE converted
pub(crate) fn xmit<T: HidMockable>(cmd: Command, d: &T) -> Result<(), Error> {
    log::debug!("{:?}", cmd);

    //Packets are up to 64 bytes long
    let buffer = &mut [0_u8; 65];

    buffer[0] = 0; // Report ID

    // header is at 1 so start at 2
    let mut offset = 2;

    //command struct is 8 bytes
    buffer.gwrite_with(cmd.id, &mut offset, LE)?;
    buffer.gwrite_with(cmd.tag, &mut offset, LE)?;
    buffer.gwrite_with(cmd._reserved0, &mut offset, LE)?;
    buffer.gwrite_with(cmd._reserved1, &mut offset, LE)?;

    //copy up to the first 55 bytes
    let mut count = if cmd.data.len() > 55 {
        55
    } else {
        cmd.data.len()
    };
    for (i, val) in cmd.data[..count].iter().enumerate() {
        buffer[i + offset] = *val;
    }

    //add those bytes to the offset too
    offset += count;

    //subtract header from offset for packet size
    if count == cmd.data.len() {
        buffer[1] = (PacketType::Final as u8) << 6 | (offset - 2) as u8;
        log::debug!("tx: {:02X?}", &buffer[..offset]);

        d.my_write(&buffer[..offset])?;
        return Ok(());
    } else {
        buffer[1] = (PacketType::Inner as u8) << 6 | (offset - 2) as u8;
        log::debug!("tx: {:02X?}", &buffer[..offset]);

        d.my_write(&buffer[..offset])?;
    }

    //send the rest in chunks up to 63
    for chunk in cmd.data[count..].chunks(64 - 1 as usize) {
        count += chunk.len();

        if count == cmd.data.len() {
            buffer[1] = (PacketType::Final as u8) << 6 | chunk.len() as u8;
        } else {
            buffer[1] = (PacketType::Inner as u8) << 6 | chunk.len() as u8;
        }

        for (i, val) in chunk.iter().enumerate() {
            buffer[i + 2] = *val
        }

        log::debug!("tx: {:02X?}", &buffer[..(chunk.len()+2)]);
        d.my_write(&buffer[..(chunk.len()+2)])?;
    }
    Ok(())
}

///Receive a CommandResponse, CommandResponse.data is not interpreted in any way.
pub(crate) fn rx<T: HidMockable>(d: &T) -> Result<CommandResponse, Error> {
    let mut bitsnbytes: Vec<u8> = vec![];

    let buffer = &mut [0_u8; 64];
    let mut retries = 5;

    // keep reading until Final packet
    'outer: while {
        let count = d.my_read(buffer)?;

        log::debug!("rx count: {:?}", count);

        if count < 1 {
            if retries <= 0 {
                return Err(Error::Parse);
            } else {
                retries -= 1;
                continue 'outer;
            }
        }

        let ptype = PacketType::try_from(buffer[0] >> 6)?;

        log::debug!("rx ptype: {:?}", ptype);

        let len: usize = (buffer[0] & 0x3F) as usize;

        log::debug!("rx len: {:?}", len);

        if len >= count {
            return Err(Error::Parse);
        }

        log::debug!(
            "rx header: {:02X?} data: {:02X?}",
            &buffer[0],
            &buffer[1..=len]
        );

        //skip the header byte and strip excess bytes remote is allowed to send
        bitsnbytes.extend_from_slice(&buffer[1..=len]);

        //funky do while notation
        ptype == PacketType::Inner
    } {}

    let resp = bitsnbytes.as_slice().pread_with::<CommandResponse>(0, LE)?;

    log::debug!("{:?}", resp);

    Ok(resp)
}

#[derive(Clone, Debug)]
pub enum Error {
    Arguments,
    Parse,
    CommandNotRecognized,
    Execution,
    Sequence,
    Transmission,
}

impl From<hidapi::HidError> for Error {
    fn from(_err: hidapi::HidError) -> Self {
        Error::Transmission
    }
}

impl From<scroll::Error> for Error {
    fn from(_err: scroll::Error) -> Self {
        Error::Parse
    }
}

impl From<core::str::Utf8Error> for Error {
    fn from(_err: core::str::Utf8Error) -> Self {
        Error::Parse
    }
}

impl From<std::io::Error> for Error {
    fn from(_err: std::io::Error) -> Self {
        Error::Arguments
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MyMock;

    #[test]
    fn send_fragmented() {
        let data: Vec<Vec<u8>> = vec![
            vec![
                0x00,
                0x3f, 0x06, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
                0x00, 0x03, 0x20, 0xd7, 0x5e, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x51, 0x5f, 0x00,
                0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00,
            ],
            vec![
                0x00,
                0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00,
                0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f,
                0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00,
                0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f,
                0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f,
            ],
            vec![
                0x00,
                0x3f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00,
                0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d,
                0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00,
                0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d,
                0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d,
            ],
            vec![
                0x00,
                0x3f, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f,
                0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00,
                0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f,
                0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            vec![
                0x00,
                0x50, 0x00, 0x00, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d, 0x5f, 0x00, 0x00, 0x4d,
                0x5f, 0x00, 0x00,
            ],
        ];

        let le_page: Vec<u8> = vec![
            0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x03, 0x20, 0xD7, 0x5E, 0x00, 0x00, 0x4D, 0x5F,
            0x00, 0x00, 0x51, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F,
            0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F,
            0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F,
            0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F,
            0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F,
            0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F,
            0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
            0x4D, 0x5F, 0x00, 0x00, 0x4D, 0x5F, 0x00, 0x00,
        ];

        let writer = |v: &[u8]| -> usize {
            static mut I: usize = 0;

            let res: &Vec<u8> = unsafe {
                let res = &data[I];
                I += 1;
                res
            };

            assert_eq!(res.as_slice(), v);

            v.len()
        };

        let mock = MyMock {
            reader: || vec![],
            writer,
        };

        let command = Command::new(0x0006, 4, le_page);

        xmit(command, &mock).unwrap();
    }

    #[test]
    fn receive_fragmented() {
        let data: Vec<Vec<u8>> = vec![
            vec![
                0x3F, 0x04, 0x00, 0x00, 0x00, 0x55, 0x46, 0x32, 0x20, 0x42, 0x6F, 0x6F, 0x74, 0x6C,
                0x6F, 0x61, 0x64, 0x65, 0x72, 0x20, 0x76, 0x33, 0x2E, 0x36, 0x2E, 0x30, 0x20, 0x53,
                0x46, 0x48, 0x57, 0x52, 0x4F, 0x0D, 0x0A, 0x4D, 0x6F, 0x64, 0x65, 0x6C, 0x3A, 0x20,
                0x50, 0x79, 0x47, 0x61, 0x6D, 0x65, 0x72, 0x0D, 0x0A, 0x42, 0x6F, 0x61, 0x72, 0x64,
                0x2D, 0x49, 0x44, 0x3A, 0x20, 0x53, 0x41, 0x4D,
            ],
            vec![
                0x54, 0x44, 0x35, 0x31, 0x4A, 0x31, 0x39, 0x41, 0x2D, 0x50, 0x79, 0x47, 0x61, 0x6D,
                0x65, 0x72, 0x2D, 0x4D, 0x34, 0x0D, 0x0A,
            ],
        ];

        let result: Vec<u8> = vec![
            0x55, 0x46, 0x32, 0x20, 0x42, 0x6F, 0x6F, 0x74, 0x6C, 0x6F, 0x61, 0x64, 0x65, 0x72,
            0x20, 0x76, 0x33, 0x2E, 0x36, 0x2E, 0x30, 0x20, 0x53, 0x46, 0x48, 0x57, 0x52, 0x4F,
            0x0D, 0x0A, 0x4D, 0x6F, 0x64, 0x65, 0x6C, 0x3A, 0x20, 0x50, 0x79, 0x47, 0x61, 0x6D,
            0x65, 0x72, 0x0D, 0x0A, 0x42, 0x6F, 0x61, 0x72, 0x64, 0x2D, 0x49, 0x44, 0x3A, 0x20,
            0x53, 0x41, 0x4D, 0x44, 0x35, 0x31, 0x4A, 0x31, 0x39, 0x41, 0x2D, 0x50, 0x79, 0x47,
            0x61, 0x6D, 0x65, 0x72, 0x2D, 0x4D, 0x34, 0x0D, 0x0A,
        ];

        let reader = || -> Vec<u8> {
            static mut I: usize = 0;

            let res: &Vec<u8> = unsafe {
                let res = &data[I];
                I += 1;
                res
            };

            res.to_vec()
        };

        let mock = MyMock {
            reader,
            writer: |_v| 0,
        };

        let response = CommandResponse {
            tag: 0x0004,
            status: CommandResponseStatus::Success,
            status_info: 0x00,
            data: result.to_vec(),
        };

        let rsp = rx(&mock).unwrap();
        assert_eq!(rsp, response);
    }
}
